//! Adding repos and watching PRs, over the in-process transport against a
//! fake GitHub.

use std::sync::Arc;

use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::github::{GitHub, GitHubError};
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Watching};
use slopwatch_protocol::{
    ClientFrame, ClientHello, Command, ErrorCode, PollState, PrStatus, Reply, RepoName,
    ResponseBody, ServerFrame, Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

/// A GitHub where `me` has two open PRs in `jnsdls/app` and someone else
/// has one.
fn github() -> Arc<FakeGitHub> {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    github.open_pr(&repo(), 1, "me", "Add the thing");
    github.open_pr(&repo(), 2, "them", "Not mine");
    github.open_pr(&repo(), 3, "me", "Fix the other thing");
    github
}

fn daemon(github: Arc<FakeGitHub>, store: Store) -> Arc<Daemon> {
    let github: Arc<dyn GitHub> = github;
    Arc::new(Daemon::with_build_id(
        "test",
        Arc::new(Watching::new(store, github).unwrap()),
    ))
}

/// A connected client that keeps its own copy of the `watched_prs` topic
/// from the frames it receives.
struct Client {
    connection: InProcessClient,
    seq: Option<u64>,
    view: WatchedPrs,
}

impl Client {
    async fn connect(daemon: &Arc<Daemon>) -> Self {
        let mut connection = InProcessClient::connect(Arc::clone(daemon)).await.unwrap();
        connection
            .send(&ClientFrame::Hello(ClientHello::local()))
            .await
            .unwrap();
        let hello = connection.recv().await.unwrap();
        assert!(matches!(hello, Some(ServerFrame::Hello(_))), "{hello:?}");
        Self {
            connection,
            seq: None,
            view: WatchedPrs::default(),
        }
    }

    async fn subscribed(daemon: &Arc<Daemon>) -> Self {
        let mut client = Self::connect(daemon).await;
        client
            .ok(Command::Subscribe {
                topic: Topic::WatchedPrs,
            })
            .await;
        assert!(client.seq.is_some(), "subscribing sends a snapshot");
        client
    }

    /// Sends `command`, applies the topic updates that arrive before its
    /// response, and returns the response.
    async fn send(&mut self, command: Command) -> ResponseBody {
        let mut frame = self.connection.request(command).await.unwrap();
        loop {
            match frame {
                Some(ServerFrame::Response(response)) => return response.result,
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
            frame = self.connection.recv().await.unwrap();
        }
    }

    async fn ok(&mut self, command: Command) -> Reply {
        match self.send(command).await {
            ResponseBody::Ok(reply) => reply,
            ResponseBody::Error(error) => panic!("expected ok, got {error:?}"),
        }
    }

    fn apply(&mut self, update: TopicUpdate) {
        let TopicUpdate::WatchedPrs { seq, update } = update;
        match update {
            WatchedPrsUpdate::Snapshot(snapshot) => self.view = snapshot,
            WatchedPrsUpdate::Delta(delta) => {
                let last = self.seq.expect("a delta never comes before the snapshot");
                assert_eq!(seq, last + 1, "deltas arrive in sequence order");
                self.view.apply(delta);
            }
        }
        self.seq = Some(seq);
    }

    fn status(&self, number: u64) -> Option<PrStatus> {
        self.view.pr(&repo(), number).map(|pr| pr.status)
    }

    fn numbers(&self) -> Vec<u64> {
        self.view.prs.iter().map(|pr| pr.number).collect()
    }
}

#[tokio::test]
async fn available_repos_are_the_ones_github_says_i_can_push_to() {
    let github = github();
    github.add_read_only_repo(&RepoName::new("someone", "else"));
    let mut client = Client::connect(&daemon(github, Store::in_memory())).await;

    let reply = client.ok(Command::ListAvailableRepos).await;

    assert_eq!(
        reply,
        Reply::AvailableRepos {
            repos: vec![repo()]
        }
    );
}

#[tokio::test]
async fn adding_a_repo_lists_its_open_prs_authored_by_me() {
    let mut client = Client::subscribed(&daemon(github(), Store::in_memory())).await;

    client.ok(Command::AddRepo { repo: repo() }).await;

    assert_eq!(client.view.repos, [repo()]);
    assert_eq!(client.numbers(), [1, 3]);
    assert_eq!(client.view.prs[0].title, "Add the thing");
    assert_eq!(client.status(1), Some(PrStatus::NotWatched));
}

#[tokio::test]
async fn adding_a_repo_github_doesnt_have_is_not_found() {
    let mut client = Client::subscribed(&daemon(github(), Store::in_memory())).await;

    let answer = client
        .send(Command::AddRepo {
            repo: RepoName::new("jnsdls", "nope"),
        })
        .await;

    let ResponseBody::Error(error) = answer else {
        panic!("expected an error, got {answer:?}");
    };
    assert_eq!(error.code, ErrorCode::NotFound);
    assert!(client.view.repos.is_empty());
}

#[tokio::test]
async fn watching_from_the_app_labels_the_pr_and_creates_the_label_once() {
    let github = github();
    let mut client = Client::subscribed(&daemon(Arc::clone(&github), Store::in_memory())).await;
    client.ok(Command::AddRepo { repo: repo() }).await;

    client
        .ok(Command::Watch {
            repo: repo(),
            number: 1,
        })
        .await;
    client
        .ok(Command::Watch {
            repo: repo(),
            number: 3,
        })
        .await;

    assert!(github.is_labeled(&repo(), 1));
    assert!(github.is_labeled(&repo(), 3));
    assert_eq!(github.label_creations(&repo()), 1);
    assert_eq!(client.status(1), Some(PrStatus::Waiting));

    client
        .ok(Command::Unwatch {
            repo: repo(),
            number: 1,
        })
        .await;

    assert!(!github.is_labeled(&repo(), 1));
    assert_eq!(client.status(1), Some(PrStatus::NotWatched));

    // A poll reads back the labels the app set, and changes nothing.
    let seq = client.seq;
    client.ok(Command::Refresh).await;
    assert_eq!(client.status(1), Some(PrStatus::NotWatched));
    assert_eq!(client.status(3), Some(PrStatus::Waiting));
    assert_eq!(
        client.seq,
        seq.map(|seq| seq + 1),
        "only the poll state changed"
    );
}

#[tokio::test]
async fn labelling_on_github_watches_the_pr_within_one_poll() {
    let github = github();
    let mut client = Client::subscribed(&daemon(Arc::clone(&github), Store::in_memory())).await;
    client.ok(Command::AddRepo { repo: repo() }).await;

    github.label_on_github(&repo(), 3, true);
    client.ok(Command::Refresh).await;

    assert_eq!(client.status(3), Some(PrStatus::Waiting));

    github.label_on_github(&repo(), 3, false);
    client.ok(Command::Refresh).await;

    assert_eq!(client.status(3), Some(PrStatus::NotWatched));
}

#[tokio::test]
async fn a_watched_pr_waits_until_its_base_branch_has_a_pipeline() {
    let github = github();
    let mut client = Client::subscribed(&daemon(Arc::clone(&github), Store::in_memory())).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client
        .ok(Command::Watch {
            repo: repo(),
            number: 1,
        })
        .await;

    assert_eq!(client.status(1), Some(PrStatus::Waiting));

    github.add_pipeline(&repo(), "main");
    client.ok(Command::Refresh).await;

    assert_eq!(client.status(1), Some(PrStatus::Ready));
}

#[tokio::test]
async fn polling_follows_prs_that_change_and_close() {
    let github = github();
    let mut client = Client::subscribed(&daemon(Arc::clone(&github), Store::in_memory())).await;
    client.ok(Command::AddRepo { repo: repo() }).await;

    github.rename_pr(&repo(), 1, "Add the thing, properly");
    github.close_pr(&repo(), 3);
    github.open_pr(&repo(), 4, "me", "Something new");
    client.ok(Command::Refresh).await;

    assert_eq!(client.numbers(), [1, 4]);
    assert_eq!(client.view.prs[0].title, "Add the thing, properly");
    assert_eq!(client.view.poll, PollState::Online);
}

#[tokio::test]
async fn watching_a_pr_the_daemon_doesnt_know_is_not_found() {
    let mut client = Client::subscribed(&daemon(github(), Store::in_memory())).await;
    client.ok(Command::AddRepo { repo: repo() }).await;

    let answer = client
        .send(Command::Watch {
            repo: repo(),
            number: 2,
        })
        .await;

    assert!(
        matches!(&answer, ResponseBody::Error(error) if error.code == ErrorCode::NotFound),
        "{answer:?}"
    );
}

#[tokio::test]
async fn a_failed_poll_shows_offline_until_the_next_one_succeeds() {
    let github = github();
    let mut client = Client::subscribed(&daemon(Arc::clone(&github), Store::in_memory())).await;
    client.ok(Command::AddRepo { repo: repo() }).await;

    github.fail_polls(Some(GitHubError::Other("no network".into())));
    let answer = client.send(Command::Refresh).await;

    assert!(matches!(answer, ResponseBody::Error(_)), "{answer:?}");
    assert!(
        matches!(&client.view.poll, PollState::Offline { message } if message.contains("no network")),
        "{:?}",
        client.view.poll
    );
    assert_eq!(client.numbers(), [1, 3], "PRs stay as last seen");

    github.fail_polls(None);
    client.ok(Command::Refresh).await;

    assert_eq!(client.view.poll, PollState::Online);
}

#[tokio::test]
async fn a_reconnecting_client_resubscribes_and_gets_a_fresh_snapshot() {
    let github = github();
    let daemon = daemon(Arc::clone(&github), Store::in_memory());
    let mut first = Client::subscribed(&daemon).await;
    first.ok(Command::AddRepo { repo: repo() }).await;
    drop(first);

    github.label_on_github(&repo(), 1, true);
    let mut other = Client::connect(&daemon).await;
    other.ok(Command::Refresh).await;

    let again = Client::subscribed(&daemon).await;

    assert_eq!(again.view.repos, [repo()]);
    assert_eq!(again.status(1), Some(PrStatus::Waiting));
    assert_eq!(again.status(3), Some(PrStatus::NotWatched));
}

#[tokio::test]
async fn deltas_reach_every_subscriber() {
    let daemon = daemon(github(), Store::in_memory());
    let mut actor = Client::subscribed(&daemon).await;
    let mut watcher = Client::subscribed(&daemon).await;

    actor.ok(Command::AddRepo { repo: repo() }).await;
    // The watcher's next response comes after the deltas already sent.
    watcher.ok(Command::Ping).await;

    assert_eq!(watcher.view, actor.view);
    assert_eq!(watcher.numbers(), [1, 3]);
}

#[tokio::test]
async fn added_repos_and_their_prs_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let github = github();
    {
        let daemon = daemon(Arc::clone(&github), Store::open(&path).unwrap());
        let mut client = Client::subscribed(&daemon).await;
        client.ok(Command::AddRepo { repo: repo() }).await;
        client
            .ok(Command::Watch {
                repo: repo(),
                number: 1,
            })
            .await;
    }

    let daemon = daemon(Arc::clone(&github), Store::open(&path).unwrap());
    let mut client = Client::subscribed(&daemon).await;

    assert_eq!(client.view.repos, [repo()]);
    assert_eq!(client.status(1), Some(PrStatus::Waiting));
    assert_eq!(client.view.poll, PollState::Pending);

    // The label already exists, so watching another PR doesn't recreate it.
    client
        .ok(Command::Watch {
            repo: repo(),
            number: 3,
        })
        .await;
    assert_eq!(github.label_creations(&repo()), 1);
}
