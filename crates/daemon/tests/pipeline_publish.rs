//! Publishing a draft Pipeline as a PR (ADR 0007), end to end: the daemon
//! over the in-process transport, its drafts and Runs on one store, and the
//! fake GitHub's real git repos underneath.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use slopwatch_core::{Edit, EndReason, Node, apply_edits};
use slopwatch_daemon::drafts::Drafts;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::github::{GitHub, NewCommit, PIPELINE_BRANCH, PIPELINE_PATH};
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::pipeline::PipelineDraft;
use slopwatch_protocol::step::UpdateMethod;
use slopwatch_protocol::step::{Check, CheckState, Checks, ChecksState};
use slopwatch_protocol::{
    ClientFrame, ClientHello, Command, ErrorBody, ErrorCode, RepoName, ResponseBody, ServerFrame,
    Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(20);

/// The Pipeline on main: hand-written, with comments.
const MAIN: &str = "\
# This repo's Pipeline.
version: 1
steps:
  # Every PR.
  ci: { uses: ci }

  review:
    uses: ci          # a stand-in reviewer
    needs: [ci]
gate: [ci, review]
";

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

struct Harness {
    github: Arc<FakeGitHub>,
    store: Store,
    daemon: Arc<Daemon>,
    _data: tempfile::TempDir,
}

/// `jnsdls/app` with [`MAIN`] on main and the repo added.
async fn harness() -> (Harness, Client) {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    github.set_pipeline(&repo(), "main", MAIN);
    let harness = daemon(github);
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client.open().await;
    (harness, client)
}

fn daemon(github: Arc<FakeGitHub>) -> Harness {
    let store = Store::in_memory();
    let data = tempfile::tempdir().unwrap();
    let exe = env!("CARGO_BIN_EXE_slopwatchd");
    let dyn_github: Arc<dyn GitHub> = Arc::clone(&github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
    let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
    let runs = Runs::start(
        store.clone(),
        dyn_github,
        Arc::clone(&watching),
        RunsConfig {
            data_dir: data.path().to_owned(),
            plugins: Plugins::new(exe, Arc::clone(&library)),
            login_path: None,
            retention: Retention::default(),
            keychain: Arc::new(MemoryKeychain::default()),
        },
    )
    .unwrap();
    let drafts = Drafts::new(
        store.clone(),
        Arc::new(Plugins::new(exe, Arc::clone(&library))),
        Arc::clone(&library),
        Arc::clone(&runs) as _,
    );
    let daemon = Arc::new(
        Daemon::with_build_id("test", watching, library)
            .with_runs(runs)
            .with_drafts(Arc::new(drafts)),
    );
    Harness {
        github,
        store,
        daemon,
        _data: data,
    }
}

struct Client {
    client: InProcessClient,
    draft: Option<PipelineDraft>,
    prs: WatchedPrs,
}

impl Client {
    async fn connect(daemon: &Arc<Daemon>) -> Client {
        let mut client = InProcessClient::connect(Arc::clone(daemon)).await.unwrap();
        client
            .send(&ClientFrame::Hello(ClientHello::local()))
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(ServerFrame::Hello(_))
        ));
        let mut client = Client {
            client,
            draft: None,
            prs: WatchedPrs::default(),
        };
        client
            .ok(Command::Subscribe {
                topic: Topic::WatchedPrs,
                since: None,
            })
            .await;
        client
    }

    fn apply(&mut self, update: TopicUpdate) {
        match update {
            TopicUpdate::Pipeline { draft } => self.draft = Some(*draft),
            TopicUpdate::WatchedPrs { update, .. } => match update {
                WatchedPrsUpdate::Snapshot(snapshot) => self.prs = snapshot,
                WatchedPrsUpdate::Delta(delta) => self.prs.apply(delta),
            },
            _ => {}
        }
    }

    async fn send(&mut self, command: Command) -> ResponseBody {
        let mut frame = self.client.request(command).await.unwrap();
        loop {
            match frame {
                Some(ServerFrame::Response(response)) => return response.result,
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
            frame = self.client.recv().await.unwrap();
        }
    }

    async fn ok(&mut self, command: Command) {
        match self.send(command).await {
            ResponseBody::Ok(_) => {}
            ResponseBody::Error(error) => panic!("expected ok, got {error:?}"),
        }
    }

    async fn refused(&mut self, command: Command) -> ErrorBody {
        match self.send(command).await {
            ResponseBody::Error(error) => error,
            ResponseBody::Ok(reply) => panic!("expected a refusal, got {reply:?}"),
        }
    }

    /// Applies frames until `done` holds.
    async fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            let frame = tokio::time::timeout_at(deadline, self.client.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                .unwrap();
            match frame {
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    async fn open(&mut self) {
        self.ok(Command::Subscribe {
            topic: Topic::Pipeline(repo()),
            since: None,
        })
        .await;
    }

    fn draft(&self) -> &PipelineDraft {
        self.draft.as_ref().expect("a draft arrived")
    }

    async fn edit(&mut self, edits: Vec<Edit>) {
        let edits_seen = self.draft().edits.len();
        self.ok(Command::EditPipeline {
            repo: repo(),
            edits_seen,
            edits,
            positions: Default::default(),
        })
        .await;
    }

    fn publish_command(&self) -> Command {
        Command::PublishPipeline {
            repo: repo(),
            edits_seen: self.draft().edits.len(),
        }
    }

    async fn publish(&mut self) {
        self.ok(self.publish_command()).await;
    }
}

fn step(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => panic!("not an object"),
    }
}

/// A `lint` Step the Gate requires.
fn add_lint() -> Vec<Edit> {
    vec![
        Edit::AddStep {
            id: "lint".into(),
            step: step(json!({ "uses": "ci", "with": { "name": "lint" } })),
        },
        Edit::AddGateTerm {
            term: "lint".into(),
        },
    ]
}

fn review_timeout(timeout: &str) -> Vec<Edit> {
    vec![Edit::SetKey {
        step: "review".into(),
        key: "timeout".into(),
        value: timeout.into(),
    }]
}

fn branch_file(harness: &Harness) -> String {
    harness
        .github
        .file(&repo(), PIPELINE_BRANCH, PIPELINE_PATH)
        .expect("the branch has a Pipeline")
}

#[tokio::test]
async fn publishing_opens_a_pr_whose_diff_touches_only_the_edited_nodes() {
    let (harness, mut client) = harness().await;
    client.edit(add_lint()).await;
    client.edit(review_timeout("30m")).await;

    client.publish().await;

    let prs = harness.github.prs_from(&repo(), PIPELINE_BRANCH);
    assert_eq!(prs.len(), 1, "one Pipeline PR");
    let file = branch_file(&harness);
    assert_eq!(
        file,
        "\
# This repo's Pipeline.
version: 1
steps:
  # Every PR.
  ci: { uses: ci }

  review:
    uses: ci          # a stand-in reviewer
    needs: [ci]
    timeout: 30m
  lint: { uses: ci, with: { name: lint } }
gate: [ci, review, lint]
"
    );
    // The PR's base is main as the draft was replayed onto it.
    assert_eq!(harness.github.api_commits().len(), 1);
    let (branch, sha) = &harness.github.api_commits()[0];
    assert_eq!(branch, PIPELINE_BRANCH);
    assert!(
        harness.store.pushed_by_slopwatch(&repo(), sha).unwrap(),
        "the commit is in the push journal"
    );
    let draft = client.draft();
    let published = draft.published.as_ref().expect("the draft knows its PR");
    assert_eq!(published.number, prs[0]);
    assert_eq!(&published.head, sha);
    assert_eq!(
        published.url,
        format!("https://github.com/jnsdls/app/pull/{}", prs[0])
    );
    assert_eq!(draft.text, file, "the draft is what was published");
    assert_eq!(draft.edits.len(), 3, "and keeps its edits");
    assert!(draft.conflicts.is_empty());
}

#[tokio::test]
async fn publishing_again_updates_the_same_pr() {
    let (harness, mut client) = harness().await;
    client.edit(add_lint()).await;
    client.publish().await;
    let first = harness.github.prs_from(&repo(), PIPELINE_BRANCH);

    client.edit(review_timeout("1h")).await;
    client.publish().await;

    assert_eq!(harness.github.prs_from(&repo(), PIPELINE_BRANCH), first);
    let commits = harness.github.api_commits();
    assert_eq!(commits.len(), 2, "one commit per publish");
    assert_eq!(harness.github.head_sha(&repo(), first[0]), commits[1].1);
    assert!(branch_file(&harness).contains("timeout: 1h"));
    assert!(branch_file(&harness).contains("lint: { uses: ci"));
    assert_eq!(
        client.draft().published.as_ref().unwrap().head,
        commits[1].1
    );

    // Publishing what the branch has already makes no commit.
    client.publish().await;
    assert_eq!(harness.github.api_commits().len(), 2);

    // Discarding the draft forgets the PR, so "Merge it now" can't merge
    // what was thrown away. The PR itself stays open on GitHub.
    client
        .ok(Command::DiscardPipelineDraft { repo: repo() })
        .await;
    assert_eq!(client.draft().published, None);
    let error = client
        .refused(Command::MergePipeline { repo: repo() })
        .await;
    assert!(error.message.contains("Publish it first"), "{error:?}");
    assert!(harness.github.is_open(&repo(), first[0]));
    client.edit(review_timeout("2h")).await;
    client.publish().await;
    assert_eq!(
        client.draft().published.as_ref().unwrap().number,
        first[0],
        "the next publish finds it again"
    );
}

#[tokio::test]
async fn a_pipeline_changed_on_main_since_the_draft_started_gets_the_edits_on_top() {
    let (harness, mut client) = harness().await;
    client.edit(add_lint()).await;
    // Someone lands a change to `ci` on main meanwhile.
    let changed = MAIN.replace("ci: { uses: ci }", "ci: { uses: ci, timeout: 20m }");
    let main = harness.github.set_pipeline(&repo(), "main", &changed);

    client.publish().await;

    assert_eq!(
        branch_file(&harness),
        apply_edits(&changed, &add_lint()).unwrap()
    );
    let draft = client.draft();
    assert_eq!(draft.base.commit, main, "the draft now starts from main");
    assert_eq!(draft.text, branch_file(&harness));
}

#[tokio::test]
async fn an_open_pipeline_pr_catches_up_with_main_before_its_next_commit() {
    let (harness, mut client) = harness().await;
    client.edit(add_lint()).await;
    client.publish().await;
    let pr = harness.github.prs_from(&repo(), PIPELINE_BRANCH)[0];
    let changed = MAIN.replace("ci: { uses: ci }", "ci: { uses: ci, timeout: 20m }");
    harness.github.set_pipeline(&repo(), "main", &changed);

    client.edit(review_timeout("1h")).await;
    client.publish().await;

    assert_eq!(harness.github.updates(), [(pr, UpdateMethod::Merge)]);
    assert_eq!(harness.github.prs_from(&repo(), PIPELINE_BRANCH), [pr]);
    let mut all = add_lint();
    all.extend(review_timeout("1h"));
    assert_eq!(branch_file(&harness), apply_edits(&changed, &all).unwrap());
}

#[tokio::test]
async fn a_node_edited_on_both_sides_stops_the_publish_and_shows_both_versions() {
    let (harness, mut client) = harness().await;
    client.edit(review_timeout("30m")).await;
    harness.github.set_pipeline(
        &repo(),
        "main",
        &MAIN.replace("    needs: [ci]\n", "    needs: [ci]\n    timeout: 2h\n"),
    );

    let error = client.refused(client.publish_command()).await;

    assert_eq!(error.code, ErrorCode::Invalid);
    assert_eq!(
        error.message,
        "Publishing stopped: Step `review` changed on the branch since the draft started, \
         and the draft changes it too."
    );
    let conflicts = &client.draft().conflicts;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].node, Node::Step("review".into()));
    assert_eq!(
        conflicts[0].draft.as_deref(),
        Some("review:\n  uses: ci          # a stand-in reviewer\n  needs: [ci]\n  timeout: 30m")
    );
    assert_eq!(
        conflicts[0].branch.as_deref(),
        Some("review:\n  uses: ci          # a stand-in reviewer\n  needs: [ci]\n  timeout: 2h")
    );
    assert!(harness.github.api_commits().is_empty(), "nothing committed");
    assert!(!harness.github.has_branch(&repo(), PIPELINE_BRANCH));

    // Starting over takes main's version.
    client
        .ok(Command::DiscardPipelineDraft { repo: repo() })
        .await;
    let draft = client.draft();
    assert!(draft.edits.is_empty());
    assert!(draft.conflicts.is_empty());
    assert!(draft.text.contains("timeout: 2h"));
}

#[tokio::test]
async fn a_pipeline_that_wouldnt_load_isnt_published() {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    let harness = daemon(github);
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client.open().await;
    // A Step and no Gate term.
    client
        .edit(vec![Edit::AddStep {
            id: "ci".into(),
            step: step(json!({ "uses": "ci" })),
        }])
        .await;

    let error = client.refused(client.publish_command()).await;

    assert_eq!(
        error.message,
        "Publishing stopped: the Pipeline wouldn't load: the Gate references no Steps."
    );
    assert!(harness.github.api_commits().is_empty());

    let error = client
        .refused(Command::PublishPipeline {
            repo: repo(),
            edits_seen: 0,
        })
        .await;
    assert!(error.message.starts_with("The draft changed"), "{error:?}");
}

#[tokio::test]
async fn a_closed_pipeline_pr_gets_a_new_one_from_main() {
    let (harness, mut client) = harness().await;
    client.edit(add_lint()).await;
    client.publish().await;
    let first = harness.github.prs_from(&repo(), PIPELINE_BRANCH)[0];
    harness.github.close_pr(&repo(), first);

    client.edit(review_timeout("1h")).await;
    client.publish().await;

    let prs = harness.github.prs_from(&repo(), PIPELINE_BRANCH);
    assert_eq!(prs.len(), 2);
    let second = prs[1];
    assert!(harness.github.is_open(&repo(), second));
    assert_eq!(client.draft().published.as_ref().unwrap().number, second);
    let mut all = add_lint();
    all.extend(review_timeout("1h"));
    assert_eq!(branch_file(&harness), apply_edits(MAIN, &all).unwrap());
}

#[tokio::test]
async fn a_commit_a_crash_left_open_is_settled_by_the_next_publish() {
    let (harness, mut client) = harness().await;
    client.edit(add_lint()).await;
    client.publish().await;
    let tip = harness.github.branch_sha(&repo(), PIPELINE_BRANCH);
    // The daemon recorded the intent and GitHub made the commit, but the
    // daemon died before it heard back.
    let text = apply_edits(&branch_file(&harness), &review_timeout("1h")).unwrap();
    harness
        .store
        .insert_pipeline_commit(&repo(), PIPELINE_BRANCH, &tip, &text)
        .unwrap();
    let made = harness
        .github
        .commit_files(
            &repo(),
            &NewCommit {
                branch: PIPELINE_BRANCH,
                expected_head: &tip,
                headline: "Update the slopwatch Pipeline",
                body: "",
                files: &[(PIPELINE_PATH, &text)],
            },
        )
        .await
        .unwrap();

    client.edit(review_timeout("1h")).await;
    client.publish().await;

    assert!(harness.store.pushed_by_slopwatch(&repo(), &made).unwrap());
    assert!(
        harness
            .store
            .open_pipeline_commits(&repo())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        harness.github.api_commits().len(),
        2,
        "the branch had the file already"
    );
    assert_eq!(client.draft().published.as_ref().unwrap().head, made);
}

#[tokio::test]
async fn merge_it_now_lands_the_pipeline_and_watched_prs_get_same_sha_runs() {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    github.set_pipeline(&repo(), "main", MAIN);
    github.open_pr(&repo(), 1, "me", "Add the thing");
    github.label_on_github(&repo(), 1, true);
    github.set_checks(
        &repo(),
        1,
        Checks {
            state: ChecksState::Success,
            runs: vec![Check {
                name: "test".into(),
                state: CheckState::Success,
                url: None,
                actions_job: None,
            }],
        },
    );
    let harness = daemon(github);
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client.ok(Command::Refresh).await;
    let runs = |client: &Client| {
        client
            .prs
            .pr(&repo(), 1)
            .map(|pr| pr.runs.clone())
            .unwrap_or_default()
    };
    client
        .until("the first Run to end", |c| {
            runs(c).first().is_some_and(|run| run.end.is_some())
        })
        .await;
    let first = runs(&client)[0].clone();
    assert_eq!(first.end, Some(EndReason::Shippable));

    client.open().await;
    client.edit(add_lint()).await;
    client.publish().await;
    let published = client.draft().published.clone().unwrap();
    client.ok(Command::MergePipeline { repo: repo() }).await;

    assert!(harness.github.is_merged(&repo(), published.number));
    let landed = harness.github.file(&repo(), "main", PIPELINE_PATH).unwrap();
    assert_eq!(landed, apply_edits(MAIN, &add_lint()).unwrap());
    let draft = client.draft();
    assert!(draft.edits.is_empty(), "the draft starts over from main");
    assert_eq!(draft.published, None);
    assert_eq!(draft.text, landed);
    client
        .until("a same-SHA Run under the new Pipeline to end", |c| {
            runs(c)
                .first()
                .is_some_and(|run| run.id != first.id && run.end.is_some())
        })
        .await;
    let second = runs(&client)[0].clone();
    assert_eq!(second.head_sha, first.head_sha, "the same SHA");
    assert_eq!(second.end, Some(EndReason::Shippable));

    let error = client
        .refused(Command::MergePipeline { repo: repo() })
        .await;
    assert!(error.message.contains("Publish it first"), "{error:?}");
}

#[tokio::test]
async fn a_draft_whose_edits_landed_on_main_starts_over_when_opened() {
    let (harness, mut client) = harness().await;
    client.edit(add_lint()).await;
    // The developer landed the change some other way, with a comment.
    let landed = apply_edits(MAIN, &add_lint())
        .unwrap()
        .replace("gate:", "# Lint too.\ngate:");
    let main = harness.github.set_pipeline(&repo(), "main", &landed);

    let mut other = Client::connect(&harness.daemon).await;
    other.open().await;

    let draft = other.draft();
    assert!(draft.edits.is_empty());
    assert_eq!(draft.base.commit, main);
    assert_eq!(draft.text, landed);
    // A draft with edits of its own keeps them.
    other.edit(review_timeout("1h")).await;
    let mut third = Client::connect(&harness.daemon).await;
    third.open().await;
    assert_eq!(third.draft().edits, review_timeout("1h"));
}
