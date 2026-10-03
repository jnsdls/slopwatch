//! Notifications (ADR 0013): what the daemon records for the GUI to post,
//! what an ack and a restart do to it, how a closed Inbox entry takes its
//! banner back, and when the daemon launches the GUI. The daemon runs over
//! the in-process transport against a fake GitHub, with a shell script
//! Plugin.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use slopwatch_core::Workspace;
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::notifications::{LaunchPace, Launcher, launch_gui_when_unheard};
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Manifest, STEP_DIALECT};
use slopwatch_protocol::{
    About, ClientFrame, ClientHello, Command, Inbox, InboxUpdate, Notification, NotificationId,
    NotificationsDelta, NotificationsUpdate, PrRef, Reply, RepoName, ResponseBody, ServerFrame,
    Topic, TopicUpdate,
};

const WAIT: Duration = Duration::from_secs(20);

const SCRIPT: &str = r#"
read -r start
act=$(printf '%s' "$start" | sed -n 's/.*"act":"\([a-z]*\)".*/\1/p')
printf '{"type":"outcome","verdict":"%s"}\n' "$act"
"#;

const PASSES: &str = "version: 1
steps:
  check: { uses: script, with: { act: pass } }
gate: [check]
";

const FAILS: &str = "version: 1
steps:
  check: { uses: script, with: { act: fail } }
gate: [check]
";

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

fn pr(number: u64) -> PrRef {
    PrRef {
        repo: repo(),
        number,
    }
}

fn manifest() -> Manifest {
    Manifest {
        id: "script".into(),
        version: "1.0.0".into(),
        dialect: STEP_DIALECT,
        features: vec![],
        config_schema: json!({ "type": "object" }),
        workspace: Workspace::None,
        effects: vec![],
        secrets: vec![],
        timeout: None,
        stall_after: None,
        concurrency: None,
    }
}

struct Harness {
    github: Arc<FakeGitHub>,
    daemon: Arc<Daemon>,
    runs: Arc<Runs>,
    data: tempfile::TempDir,
}

impl Harness {
    /// My labelled PRs `prs` in `jnsdls/app`, with `pipeline` on main.
    fn new(pipeline: &str, prs: &[u64]) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        for &number in prs {
            github.open_pr(&repo(), number, "me", &format!("PR {number}"));
            github.label_on_github(&repo(), number, true);
        }
        github.set_pipeline(&repo(), "main", pipeline);
        let data = tempfile::tempdir().unwrap();
        let (daemon, runs) = daemon(&github, data.path());
        Harness {
            github,
            daemon,
            runs,
            data,
        }
    }

    /// A new daemon on the same data dir, as after a crash.
    fn restart(&mut self) {
        (self.daemon, self.runs) = daemon(&self.github, self.data.path());
    }
}

fn daemon(github: &Arc<FakeGitHub>, data: &Path) -> (Arc<Daemon>, Arc<Runs>) {
    let store = Store::open(&data.join("state.db")).unwrap();
    let dyn_github: Arc<dyn GitHub> = Arc::clone(github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
    let library = Arc::new(Library::open(data.join("steps")).unwrap());
    let args = vec!["-c".to_owned(), SCRIPT.to_owned(), "script".to_owned()];
    let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library)).with_plugin(
        manifest(),
        PathBuf::from("/bin/sh"),
        args,
    );
    let runs = Runs::start(
        store,
        dyn_github,
        Arc::clone(&watching),
        RunsConfig {
            data_dir: data.to_owned(),
            plugins,
            login_path: None,
            retention: Retention::default(),
        },
    )
    .unwrap();
    let daemon = Daemon::with_build_id("test", watching, library).with_runs(Arc::clone(&runs));
    (Arc::new(daemon), runs)
}

/// A client that follows the Inbox and, like the GUI, the notifications.
struct Client {
    connection: InProcessClient,
    inbox: Inbox,
    /// What's pending, as the topic says.
    pending: Vec<Notification>,
    /// Every Put and Done delta, in order.
    deltas: Vec<NotificationsDelta>,
}

impl Client {
    async fn connect(daemon: &Arc<Daemon>, topics: &[Topic]) -> Self {
        let mut connection = InProcessClient::connect(Arc::clone(daemon)).await.unwrap();
        connection
            .send(&ClientFrame::Hello(ClientHello::local()))
            .await
            .unwrap();
        let hello = connection.recv().await.unwrap();
        assert!(matches!(hello, Some(ServerFrame::Hello(_))), "{hello:?}");
        let mut client = Self {
            connection,
            inbox: Inbox::default(),
            pending: Vec::new(),
            deltas: Vec::new(),
        };
        for topic in topics {
            client
                .ok(Command::Subscribe {
                    topic: topic.clone(),
                    since: None,
                })
                .await;
        }
        client
    }

    /// Connects the way the GUI does, to the Inbox and the notifications.
    async fn gui(daemon: &Arc<Daemon>) -> Self {
        Self::connect(daemon, &[Topic::Inbox, Topic::Notifications]).await
    }

    async fn ok(&mut self, command: Command) -> Reply {
        let mut frame = self.connection.request(command).await.unwrap();
        loop {
            match frame {
                Some(ServerFrame::Response(response)) => match response.result {
                    ResponseBody::Ok(reply) => return reply,
                    ResponseBody::Error(error) => panic!("expected ok, got {error:?}"),
                },
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
            frame = self.connection.recv().await.unwrap();
        }
    }

    fn apply(&mut self, update: TopicUpdate) {
        match update {
            TopicUpdate::Inbox { update, .. } => match update {
                InboxUpdate::Snapshot(snapshot) => self.inbox = snapshot,
                InboxUpdate::Delta(delta) => self.inbox.apply(delta),
            },
            TopicUpdate::Notifications { update, .. } => match update {
                NotificationsUpdate::Snapshot(snapshot) => self.pending = snapshot,
                NotificationsUpdate::Delta(delta) => {
                    match &delta {
                        NotificationsDelta::Put { notification } => {
                            self.pending.retain(|held| held.id() != notification.id());
                            self.pending.push(notification.clone());
                        }
                        NotificationsDelta::Done { id } => {
                            self.pending.retain(|held| held.id() != id);
                        }
                    }
                    self.deltas.push(delta);
                }
            },
            _ => {}
        }
    }

    async fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            let frame = tokio::time::timeout_at(deadline, self.connection.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}: {:#?}", self.pending))
                .unwrap();
            match frame {
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    async fn ack(&mut self, done: Vec<Notification>) {
        self.ok(Command::AckNotifications { done }).await;
    }

    /// Acks everything pending, as a GUI does once it posted it.
    async fn ack_all(&mut self) {
        self.ack(self.pending.clone()).await;
    }
}

fn posts(pending: &[Notification]) -> Vec<(&NotificationId, &str, &str)> {
    pending
        .iter()
        .filter_map(|notification| match notification {
            Notification::Post { id, title, body } => Some((id, title.as_str(), body.as_str())),
            Notification::Retract { .. } => None,
        })
        .collect()
}

/// A PR entry for PR 1, with its notification pending.
async fn not_shippable(harness: &Harness) -> Client {
    let mut client = Client::gui(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client
        .until("the PR entry's notification", |c| {
            c.inbox.count() == 1 && !c.pending.is_empty()
        })
        .await;
    client
}

#[tokio::test]
async fn a_new_inbox_entry_is_pending_until_the_gui_acks_it() {
    let harness = Harness::new(FAILS, &[1]);
    let mut client = not_shippable(&harness).await;

    let entry = client.inbox.entries[0].clone();
    let pending = posts(&client.pending);
    assert_eq!(pending.len(), 1, "{:#?}", client.pending);
    let (id, title, body) = pending[0];
    assert_eq!(
        *id,
        NotificationId {
            about: About::Entry(entry.id),
            pr: pr(1),
        },
        "the id names the entry and the PR a click opens"
    );
    assert_eq!(title, "Not shippable");
    assert_eq!(body, "jnsdls/app#1: `check`: fail");

    let id = id.clone();
    let post = client.pending[0].clone();
    client.ack(vec![post.clone()]).await;
    assert!(client.pending.is_empty());
    assert_eq!(
        client.deltas.last(),
        Some(&NotificationsDelta::Done { id: id.clone() })
    );

    client.ack(vec![post]).await;
    let again = Client::gui(&harness.daemon).await;
    assert!(
        again.pending.is_empty(),
        "an acked notification stays acked"
    );
}

#[tokio::test]
async fn a_shippable_run_is_pending_with_the_prs_title() {
    let harness = Harness::new(PASSES, &[1]);
    let mut client = Client::gui(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client
        .until("the shippable notification", |c| !c.pending.is_empty())
        .await;

    let pending = posts(&client.pending);
    let (id, title, body) = pending[0];
    assert!(matches!(id.about, About::Shippable(_)), "{id}");
    assert_eq!(id.pr, pr(1));
    assert_eq!(title, "Shippable");
    assert_eq!(body, "jnsdls/app#1: PR 1");
    assert!(client.inbox.entries.is_empty());
}

#[tokio::test]
async fn a_restart_sends_nothing_acked_again_and_keeps_what_wasnt() {
    let mut harness = Harness::new(FAILS, &[1, 2]);
    let mut client = Client::gui(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client
        .until("both PR entries' notifications", |c| c.pending.len() == 2)
        .await;
    let acked = client.pending[0].clone();
    let kept = client.pending[1].clone();
    client.ack(vec![acked]).await;
    drop(client);

    harness.restart();
    let mut client = Client::gui(&harness.daemon).await;
    client.ok(Command::Refresh).await;

    assert_eq!(client.pending, [kept], "only the unacked one comes back");
}

#[tokio::test]
async fn closing_an_entry_retracts_its_posted_banner() {
    let harness = Harness::new(FAILS, &[1]);
    let mut client = not_shippable(&harness).await;
    let id = client.pending[0].id().clone();
    client.ack_all().await;

    harness.github.set_pipeline(&repo(), "main", PASSES);
    client.ok(Command::Refresh).await;
    client
        .until("the retract", |c| {
            c.pending
                .iter()
                .any(|n| matches!(n, Notification::Retract { .. }))
        })
        .await;

    assert!(client.inbox.entries.is_empty(), "the next Run closed it");
    assert!(
        client
            .pending
            .contains(&Notification::Retract { id: id.clone() }),
        "{:#?}",
        client.pending
    );
    client
        .ack(vec![Notification::Retract { id: id.clone() }])
        .await;
    client
        .until("the shippable notification", |c| {
            posts(&c.pending)
                .iter()
                .any(|(id, ..)| matches!(id.about, About::Shippable(_)))
        })
        .await;
    assert!(
        client.pending.iter().all(|n| n.id() != &id),
        "the retract is done once acked"
    );
}

#[tokio::test]
async fn an_entry_that_closes_with_no_gui_listening_never_posts() {
    let harness = Harness::new(FAILS, &[1]);
    let mut client = Client::connect(&harness.daemon, &[Topic::Inbox]).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client.until("the PR entry", |c| c.inbox.count() == 1).await;

    client
        .ok(Command::DismissEntry {
            entry: client.inbox.entries[0].id,
        })
        .await;

    let gui = Client::gui(&harness.daemon).await;
    assert!(gui.pending.is_empty(), "{:#?}", gui.pending);
}

#[tokio::test]
async fn an_entry_that_closes_before_the_guis_ack_arrives_still_retracts() {
    let harness = Harness::new(FAILS, &[1]);
    let mut client = not_shippable(&harness).await;
    let post = client.pending[0].clone();
    let retract = Notification::Retract {
        id: post.id().clone(),
    };

    // The GUI posted it, but the entry closes before its ack lands.
    client
        .ok(Command::DismissEntry {
            entry: client.inbox.entries[0].id,
        })
        .await;
    assert_eq!(client.pending, std::slice::from_ref(&retract));
    client.ack(vec![post]).await;

    assert_eq!(
        client.pending,
        std::slice::from_ref(&retract),
        "the late ack for the Post doesn't cover the Retract"
    );
    client.ack(vec![retract]).await;
    assert!(client.pending.is_empty());
}

#[derive(Default)]
struct CountingLauncher(AtomicUsize);

impl Launcher for CountingLauncher {
    fn launch(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl CountingLauncher {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

const QUICK: LaunchPace = LaunchPace {
    grace: Duration::from_millis(0),
    retry: Duration::from_millis(100),
};

fn watch_launches(harness: &Harness) -> Arc<CountingLauncher> {
    let launcher = Arc::new(CountingLauncher::default());
    tokio::spawn(launch_gui_when_unheard(
        Arc::clone(harness.runs.notifications()),
        Arc::clone(&launcher) as Arc<dyn Launcher>,
        QUICK,
    ));
    launcher
}

async fn wait_for(what: &str, done: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn with_no_gui_subscribed_a_new_entry_launches_one() {
    let harness = Harness::new(FAILS, &[1]);
    let launcher = watch_launches(&harness);
    let mut client = Client::connect(&harness.daemon, &[Topic::Inbox]).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client.until("the PR entry", |c| c.inbox.count() == 1).await;

    wait_for("a launch", || launcher.count() > 0).await;

    let mut gui = Client::gui(&harness.daemon).await;
    assert_eq!(gui.pending.len(), 1, "the launched GUI finds it pending");
    gui.ack_all().await;
    drop(gui);
    let launched = launcher.count();
    tokio::time::sleep(QUICK.retry * 3).await;
    assert_eq!(
        launcher.count(),
        launched,
        "nothing pending, so nothing launches"
    );
}

#[tokio::test]
async fn with_a_gui_subscribed_nothing_launches() {
    let harness = Harness::new(FAILS, &[1]);
    let launcher = watch_launches(&harness);

    let _client = not_shippable(&harness).await;
    tokio::time::sleep(QUICK.retry * 3).await;

    assert_eq!(launcher.count(), 0);
}
