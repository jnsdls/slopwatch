//! Runs end to end: a fake GitHub, the daemon over the in-process
//! transport, and real Step processes from the `slopwatchd` binary.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use slopwatch_core::{EndReason, GateState, Verdict};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Check, CheckState, Checks, ChecksState};
use slopwatch_protocol::{
    ClientFrame, ClientHello, Command, ErrorCode, PullRequest, Reply, RepoName, ResponseBody,
    RunEvent, RunId, RunView, ServerFrame, StepStatus, Topic, TopicUpdate, WatchedPrs,
    WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(20);

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

struct Harness {
    github: Arc<FakeGitHub>,
    store: Store,
    daemon: Arc<Daemon>,
    _data: tempfile::TempDir,
}

/// My PR 1 in `jnsdls/app`, labelled, with the CI-only Pipeline on main.
fn harness() -> Harness {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    github.open_pr(&repo(), 1, "me", "Add the thing");
    github.label_on_github(&repo(), 1, true);
    github.add_pipeline(&repo(), "main");
    let store = Store::in_memory();
    let data = tempfile::tempdir().unwrap();
    let daemon = daemon(&github, &store, &data);
    Harness {
        github,
        store,
        daemon,
        _data: data,
    }
}

fn daemon(github: &Arc<FakeGitHub>, store: &Store, data: &tempfile::TempDir) -> Arc<Daemon> {
    let dyn_github: Arc<dyn GitHub> = Arc::clone(github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
    let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
    let runs = Runs::start(
        store.clone(),
        dyn_github,
        Arc::clone(&watching),
        RunsConfig {
            data_dir: data.path().to_owned(),
            plugins: Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library)),
        },
    )
    .unwrap();
    Arc::new(Daemon::with_build_id("test", watching, library).with_runs(runs))
}

fn checks(state: ChecksState, check: CheckState) -> Checks {
    Checks {
        state,
        runs: vec![Check {
            name: "test".into(),
            state: check,
            url: None,
        }],
    }
}

/// A client that keeps its own copy of `watched_prs` and of every Run it
/// subscribed to, from the frames it receives.
struct Client {
    connection: InProcessClient,
    prs: WatchedPrs,
    runs: HashMap<RunId, RunView>,
    /// Every Run event received, in order.
    events: Vec<(RunId, u64)>,
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
        let mut client = Self {
            connection,
            prs: WatchedPrs::default(),
            runs: HashMap::new(),
            events: Vec::new(),
        };
        client
            .ok(Command::Subscribe {
                topic: Topic::WatchedPrs,
                since: None,
            })
            .await;
        client
    }

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
        match update {
            TopicUpdate::WatchedPrs { update, .. } => match update {
                WatchedPrsUpdate::Snapshot(snapshot) => self.prs = snapshot,
                WatchedPrsUpdate::Delta(delta) => self.prs.apply(delta),
            },
            TopicUpdate::Run { id, seq, event } => {
                self.events.push((id, seq));
                let view = self.runs.entry(id).or_default();
                assert_eq!(seq, view.seq + 1, "Run events arrive in order, once each");
                view.apply(seq, event);
            }
        }
    }

    async fn subscribe(&mut self, run: RunId, since: Option<u64>) {
        self.ok(Command::Subscribe {
            topic: Topic::Run(run),
            since,
        })
        .await;
    }

    /// Applies frames until `done` holds.
    async fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            let frame = tokio::time::timeout_at(deadline, self.connection.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
                .unwrap();
            match frame {
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    fn pr(&self) -> &PullRequest {
        self.prs.pr(&repo(), 1).expect("PR 1 is listed")
    }

    /// The PR's Run history, newest first.
    fn history(&self) -> Vec<RunId> {
        self.pr().runs.iter().map(|run| run.id).collect()
    }

    fn run(&self, id: RunId) -> &RunView {
        &self.runs[&id]
    }

    fn ci(&self, id: RunId) -> &StepStatus {
        &self.run(id).step("ci").expect("the Run lists ci").status
    }

    async fn refresh(&mut self) {
        self.ok(Command::Refresh).await;
    }
}

/// Adds the repo, which starts the first Run, and subscribes to it.
async fn first_run(harness: &Harness) -> (Client, RunId) {
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let history = client.history();
    assert_eq!(history.len(), 1, "watching the PR started one Run");
    let run = history[0];
    client.subscribe(run, None).await;
    assert_eq!(
        client.run(run).head_sha,
        harness.github.head_sha(&repo(), 1)
    );
    (client, run)
}

fn ended(client: &Client, run: RunId) -> bool {
    client.runs.get(&run).is_some_and(|view| view.end.is_some())
}

#[tokio::test]
async fn a_ci_only_pipeline_runs_ci_and_the_gate_follows_a_pass() {
    let harness = harness();
    let (mut client, run) = first_run(&harness).await;

    assert_eq!(client.run(run).base, "main");
    assert_eq!(
        client.run(run).base_sha,
        harness.github.branch_sha(&repo(), "main"),
        "the Run records the base SHA its Pipeline came from"
    );
    assert_eq!(*client.ci(run), StepStatus::Running);
    assert_eq!(client.run(run).gate, Some(GateState::Pending));

    harness.github.set_checks(
        &repo(),
        1,
        checks(ChecksState::Pending, CheckState::Pending),
    );
    client.refresh().await;
    assert_eq!(*client.ci(run), StepStatus::Running, "CI waits for checks");

    harness.github.set_checks(
        &repo(),
        1,
        checks(ChecksState::Success, CheckState::Success),
    );
    client.refresh().await;
    client.until("the Run to end", |c| ended(c, run)).await;

    let view = client.run(run);
    assert!(matches!(
        view.step("ci").unwrap().status,
        StepStatus::Settled {
            verdict: Verdict::Pass,
            ..
        }
    ));
    assert_eq!(view.gate, Some(GateState::Pass));
    assert_eq!(view.end, Some(EndReason::Shippable));
    client
        .until("the row to show the end", |c| {
            c.pr().runs[0].end == Some(EndReason::Shippable)
        })
        .await;

    // An ended Run on the same head doesn't start again.
    client.refresh().await;
    assert_eq!(client.history(), [run]);
}

#[tokio::test]
async fn failing_checks_fail_ci_and_the_gate_with_a_finding_per_check() {
    let harness = harness();
    let (mut client, run) = first_run(&harness).await;

    harness.github.set_checks(
        &repo(),
        1,
        checks(ChecksState::Failure, CheckState::Failure),
    );
    client.refresh().await;
    client.until("the Run to end", |c| ended(c, run)).await;

    let view = client.run(run);
    let StepStatus::Settled {
        verdict, outputs, ..
    } = &view.step("ci").unwrap().status
    else {
        panic!("ci settled");
    };
    assert_eq!(*verdict, Verdict::Fail);
    assert_eq!(outputs.findings[0].message, "Check `test` failed");
    assert_eq!(view.gate, Some(GateState::Fail));
    assert_eq!(view.end, Some(EndReason::NotShippable));
}

#[tokio::test]
async fn a_push_supersedes_the_run_and_starts_one_on_the_new_sha() {
    let harness = harness();
    let (mut client, first) = first_run(&harness).await;

    let pushed = harness.github.push(&repo(), 1);
    client.refresh().await;
    client
        .until("the first Run to end", |c| ended(c, first))
        .await;

    assert_eq!(client.run(first).end, Some(EndReason::Superseded));
    assert!(matches!(
        client.ci(first),
        StepStatus::Settled {
            verdict: Verdict::Cancelled,
            ..
        }
    ));
    let history = client.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1], first, "newest first");
    let second = history[0];
    assert_eq!(client.pr().runs[0].head_sha, pushed);
    client.subscribe(second, None).await;
    assert_eq!(client.run(second).head_sha, pushed);
    assert_eq!(*client.ci(second), StepStatus::Running);
}

#[tokio::test]
async fn a_push_slopwatch_made_ends_the_run_as_pushed() {
    let harness = harness();
    let (mut client, first) = first_run(&harness).await;

    let pushed = harness.github.push(&repo(), 1);
    harness.store.record_push(&repo(), &pushed).unwrap();
    client.refresh().await;
    client
        .until("the first Run to end", |c| ended(c, first))
        .await;

    assert_eq!(client.run(first).end, Some(EndReason::Pushed));
}

#[tokio::test]
async fn a_pipeline_change_on_the_head_branch_doesnt_change_how_the_pr_is_judged() {
    let harness = harness();
    let mut client = Client::connect(&harness.daemon).await;
    // The PR rewrites the Pipeline so its Gate reads a Step named `lint`.
    let head = harness.github.push_file(
        &repo(),
        1,
        ".slopwatch/pipeline.yml",
        "version: 1\nsteps:\n  lint: { uses: ci }\ngate: [lint]\n",
    );

    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history()[0];
    client.subscribe(run, None).await;

    let view = client.run(run);
    assert_eq!(view.head_sha, head);
    let steps: Vec<&str> = view.steps.iter().map(|s| s.info.id.as_str()).collect();
    assert_eq!(steps, ["ci"], "the Pipeline comes from main, not the head");
    assert_eq!(view.gate_text, "[ci]");
}

#[tokio::test]
async fn a_client_that_reconnects_mid_run_gets_only_the_events_it_missed() {
    let harness = harness();
    let (first, run) = first_run(&harness).await;
    let seen = first.run(run).seq;
    assert_eq!(seen, 2, "started, then ci started");
    drop(first);

    harness.github.set_checks(
        &repo(),
        1,
        checks(ChecksState::Success, CheckState::Success),
    );
    let mut other = Client::connect(&harness.daemon).await;
    other.refresh().await;
    other
        .until("the Run to end", |c| {
            c.pr().runs[0].end == Some(EndReason::Shippable)
        })
        .await;

    let mut again = Client::connect(&harness.daemon).await;
    // Seed its view with what it had before it went away.
    again
        .runs
        .insert(run, view_until(&harness.store, run, seen));
    again.subscribe(run, Some(seen)).await;

    let received: Vec<u64> = again.events.iter().map(|&(_, seq)| seq).collect();
    assert_eq!(received.first(), Some(&(seen + 1)));
    assert!(received.iter().all(|&seq| seq > seen));
    assert_eq!(again.run(run).end, Some(EndReason::Shippable));
}

/// The Run as a client that saw its events up to `seq` had it.
fn view_until(store: &Store, run: RunId, seq: u64) -> RunView {
    let mut view = RunView::default();
    for (at, text) in store.events_after(run, 0).unwrap() {
        if at <= seq {
            let event: RunEvent = serde_json::from_str(&text).unwrap();
            view.apply(at, event);
        }
    }
    view
}

#[tokio::test]
async fn an_invalid_pipeline_blocks_the_pr_with_a_reason_and_no_run() {
    let harness = harness();
    harness.github.set_pipeline(
        &repo(),
        "main",
        "version: 1\nsteps:\n  review: { uses: nobody }\ngate: [review]\n",
    );
    let mut client = Client::connect(&harness.daemon).await;

    client.ok(Command::AddRepo { repo: repo() }).await;

    assert!(client.history().is_empty());
    let blocked = client.pr().blocked.clone().unwrap_or_default();
    assert!(
        blocked.contains("invalid") && blocked.contains("nobody"),
        "{blocked}"
    );

    harness.github.add_pipeline(&repo(), "main");
    client.refresh().await;

    assert_eq!(client.history().len(), 1, "a fixed Pipeline starts the Run");
    assert_eq!(client.pr().blocked, None);
}

#[tokio::test]
async fn unwatching_cancels_the_run_and_rewatching_starts_another() {
    let harness = harness();
    let (mut client, first) = first_run(&harness).await;

    client
        .ok(Command::Unwatch {
            repo: repo(),
            number: 1,
        })
        .await;
    client.until("the Run to end", |c| ended(c, first)).await;
    assert_eq!(client.run(first).end, Some(EndReason::Cancelled));

    client
        .ok(Command::Watch {
            repo: repo(),
            number: 1,
        })
        .await;
    assert_eq!(client.history().len(), 2);
}

#[tokio::test]
async fn a_closed_pr_ends_its_run_as_closed() {
    let harness = harness();
    let (mut client, run) = first_run(&harness).await;

    harness.github.close_pr(&repo(), 1);
    client.refresh().await;
    client.until("the Run to end", |c| ended(c, run)).await;

    assert_eq!(client.run(run).end, Some(EndReason::Closed));
}

#[tokio::test]
async fn subscribing_to_a_run_that_doesnt_exist_is_not_found() {
    let harness = harness();
    let mut client = Client::connect(&harness.daemon).await;

    let answer = client
        .send(Command::Subscribe {
            topic: Topic::Run(RunId(99)),
            since: None,
        })
        .await;

    assert!(
        matches!(&answer, ResponseBody::Error(error) if error.code == ErrorCode::NotFound),
        "{answer:?}"
    );
}

#[tokio::test]
async fn a_restarted_daemon_picks_its_run_back_up() {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    github.open_pr(&repo(), 1, "me", "Add the thing");
    github.label_on_github(&repo(), 1, true);
    github.add_pipeline(&repo(), "main");
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("state.db");
    let run = {
        let store = Store::open(&path).unwrap();
        let daemon = daemon(&github, &store, &data);
        let mut client = Client::connect(&daemon).await;
        client.ok(Command::AddRepo { repo: repo() }).await;
        daemon.kill_steps().await;
        client.history()[0]
    };

    github.set_checks(
        &repo(),
        1,
        checks(ChecksState::Success, CheckState::Success),
    );
    let store = Store::open(&path).unwrap();
    let daemon = daemon(&github, &store, &data);
    let mut client = Client::connect(&daemon).await;
    client.subscribe(run, None).await;
    client.refresh().await;
    client.until("the Run to end", |c| ended(c, run)).await;

    assert_eq!(client.history(), [run], "the same Run, not a new one");
    assert_eq!(client.run(run).end, Some(EndReason::Shippable));
}

#[tokio::test]
async fn a_run_resolves_library_steps_from_the_library() {
    let harness = harness();
    let mut client = Client::connect(&harness.daemon).await;
    client
        .ok(Command::SaveLibraryStep {
            step: "checks".into(),
            text: "uses: ci\n".into(),
        })
        .await;
    harness.github.set_pipeline(
        &repo(),
        "main",
        "version: 1\nsteps:\n  ci: { uses: lib/checks }\ngate: [ci]\n",
    );

    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history()[0];
    client.subscribe(run, None).await;

    assert_eq!(client.run(run).steps[0].info.plugin, "ci");
    assert_eq!(*client.ci(run), StepStatus::Running);
}
