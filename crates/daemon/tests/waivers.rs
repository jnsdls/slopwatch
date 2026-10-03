//! Waivers and the Gate override end to end: a fake GitHub, the daemon over
//! the in-process transport, and real Step processes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use slopwatch_core::{EndReason, GateState, Verdict, WaiverCategory};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Check, CheckState, Checks, ChecksState};
use slopwatch_protocol::{
    Actor, ClientFrame, ClientHello, Command, ErrorCode, PullRequest, Reply, RepoName,
    ResponseBody, RunEvent, RunId, RunView, ServerFrame, StepStatus, Topic, TopicUpdate, Waiver,
    WatchedPrs, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(20);

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

/// CI plus a second Step, `lint`, both in the Gate.
const CI_AND_LINT: &str = "version: 1\nsteps:\n  ci: { uses: ci }\n  lint: { uses: ci, with: { name: lint } }\ngate: [ci, lint]\n";

struct Harness {
    github: Arc<FakeGitHub>,
    store: Store,
    daemon: Arc<Daemon>,
    _data: tempfile::TempDir,
    _bin: Option<tempfile::TempDir>,
}

/// My PR 1 in `jnsdls/app`, labelled, with `pipeline` on main.
fn harness(pipeline: &str) -> Harness {
    let github = github(pipeline);
    let store = Store::in_memory();
    let data = tempfile::tempdir().unwrap();
    let daemon = daemon(
        &github,
        &store,
        &data,
        Path::new(env!("CARGO_BIN_EXE_slopwatchd")),
    );
    Harness {
        github,
        store,
        daemon,
        _data: data,
        _bin: None,
    }
}

/// Like [`harness`], but Step `slow` never reports: it runs until the
/// daemon goes away and its stdin closes. Every other Step runs as usual.
fn harness_with_slow_step(pipeline: &str) -> Harness {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = tempfile::tempdir().unwrap();
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$SLOPWATCH_STEP\" = slow ]; then\n\
         \x20 while read line; do :; done\n\
         \x20 exit 0\n\
         fi\n\
         exec '{exe}' \"$@\"\n",
        exe = env!("CARGO_BIN_EXE_slopwatchd"),
    );
    let plugins = bin.path().join("plugins.sh");
    std::fs::write(&plugins, script).unwrap();
    std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(0o755)).unwrap();
    let github = github(pipeline);
    let store = Store::in_memory();
    let data = tempfile::tempdir().unwrap();
    let daemon = daemon(&github, &store, &data, &plugins);
    Harness {
        github,
        store,
        daemon,
        _data: data,
        _bin: Some(bin),
    }
}

fn github(pipeline: &str) -> Arc<FakeGitHub> {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    github.open_pr(&repo(), 1, "me", "Add the thing");
    github.label_on_github(&repo(), 1, true);
    github.set_pipeline(&repo(), "main", pipeline);
    github
}

fn daemon(
    github: &Arc<FakeGitHub>,
    store: &Store,
    data: &tempfile::TempDir,
    plugin_exe: &Path,
) -> Arc<Daemon> {
    let dyn_github: Arc<dyn GitHub> = Arc::clone(github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
    let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
    let runs = Runs::start(
        store.clone(),
        dyn_github,
        Arc::clone(&watching),
        RunsConfig {
            data_dir: data.path().to_owned(),
            plugins: Plugins::new(plugin_exe, Arc::clone(&library)),
            login_path: None,
            retention: Retention::default(),
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
            actions_job: None,
        }],
    }
}

fn fail_checks(harness: &Harness) {
    harness.github.set_checks(
        &repo(),
        1,
        checks(ChecksState::Failure, CheckState::Failure),
    );
}

/// A client that keeps its own copy of `watched_prs` and of every Run it
/// subscribed to.
struct Client {
    connection: InProcessClient,
    prs: WatchedPrs,
    runs: HashMap<RunId, RunView>,
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

    /// Sends `command` and returns the error the daemon refused it with.
    async fn refused(&mut self, command: Command) -> (ErrorCode, String) {
        match self.send(command).await {
            ResponseBody::Error(error) => (error.code, error.message),
            ResponseBody::Ok(reply) => panic!("expected an error, got {reply:?}"),
        }
    }

    fn apply(&mut self, update: TopicUpdate) {
        match update {
            TopicUpdate::WatchedPrs { update, .. } => match update {
                WatchedPrsUpdate::Snapshot(snapshot) => self.prs = snapshot,
                WatchedPrsUpdate::Delta(delta) => self.prs.apply(delta),
            },
            // These tests don't subscribe to the Inbox.
            TopicUpdate::Inbox { .. } => {}
            TopicUpdate::Run { id, seq, event, .. } => {
                self.runs.entry(id).or_default().apply(seq, event);
            }
            TopicUpdate::StepLog { .. } => {}
        }
    }

    async fn subscribe(&mut self, run: RunId) {
        self.ok(Command::Subscribe {
            topic: Topic::Run(run),
            since: None,
        })
        .await;
    }

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

    fn history(&self) -> Vec<RunId> {
        self.pr().runs.iter().map(|run| run.id).collect()
    }

    fn run(&self, id: RunId) -> &RunView {
        &self.runs[&id]
    }

    fn verdict(&self, run: RunId, step: &str) -> (Verdict, Option<RunId>) {
        match &self
            .run(run)
            .step(step)
            .expect("the Run lists the Step")
            .status
        {
            StepStatus::Settled {
                verdict,
                reused_from,
                ..
            } => (*verdict, *reused_from),
            other => panic!("`{step}` hasn't settled: {other:?}"),
        }
    }

    fn waiver(&self, run: RunId, step: &str) -> Option<&Waiver> {
        self.run(run).step(step)?.waiver.as_ref()
    }

    async fn refresh(&mut self) {
        self.ok(Command::Refresh).await;
    }

    /// Waits for a Run newer than `after`, subscribes to it and waits for
    /// it to end.
    async fn next_run_ends(&mut self, after: RunId) -> RunId {
        self.until("a new Run", |c| {
            c.history().first().is_some_and(|&run| run != after)
        })
        .await;
        let run = self.history()[0];
        self.subscribe(run).await;
        self.until("the new Run to end", |c| ended(c, run)).await;
        run
    }
}

fn ended(client: &Client, run: RunId) -> bool {
    client.runs.get(&run).is_some_and(|view| view.end.is_some())
}

/// Adds the repo, subscribes to the first Run, fails the checks and waits
/// for the Run to end not shippable.
async fn first_run_fails(harness: &Harness) -> (Client, RunId) {
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history()[0];
    client.subscribe(run).await;
    fail_checks(harness);
    client.refresh().await;
    client
        .until("the first Run to end", |c| ended(c, run))
        .await;
    assert_eq!(client.run(run).end, Some(EndReason::NotShippable));
    (client, run)
}

fn waive(run: RunId, step: &str, category: WaiverCategory, reason: &str) -> Command {
    Command::WaiveStep {
        run,
        step: step.into(),
        category,
        reason: reason.into(),
    }
}

/// The Steps whose process the Run started, from its journal.
fn started_steps(store: &Store, run: RunId) -> Vec<String> {
    store
        .events_after(run, 0)
        .unwrap()
        .into_iter()
        .filter_map(
            |stored| match serde_json::from_str(&stored.event).unwrap() {
                RunEvent::StepStarted { step, .. } => Some(step),
                _ => None,
            },
        )
        .collect()
}

#[tokio::test]
async fn waiving_the_only_failing_term_passes_the_gate_in_a_same_sha_run_that_runs_nothing() {
    let harness = harness(slopwatch_daemon::github::fake::CI_PIPELINE);
    let (mut client, first) = first_run_fails(&harness).await;

    client
        .ok(waive(
            first,
            "ci",
            WaiverCategory::FalsePositive,
            "the runner was out of disk",
        ))
        .await;
    let second = client.next_run_ends(first).await;

    let view = client.run(second);
    assert_eq!(view.head_sha, client.run(first).head_sha, "the same SHA");
    assert_eq!(
        client.verdict(second, "ci"),
        (Verdict::Fail, Some(first)),
        "ci keeps its failing Outcome"
    );
    assert_eq!(
        client.waiver(second, "ci"),
        Some(&Waiver {
            category: WaiverCategory::FalsePositive,
            reason: "the runner was out of disk".into(),
            actor: Actor::Developer {
                via: "in_process".into()
            },
        }),
        "the Run's record keeps the Waiver"
    );
    assert_eq!(view.gate, Some(GateState::Pass));
    assert_eq!(view.end, Some(EndReason::Shippable));
    assert!(view.waived, "it reads shippable (waived)");
    assert!(started_steps(&harness.store, second).is_empty());
    client
        .until("the row to show the waived end", |c| c.pr().runs[0].waived)
        .await;

    let first_view = client.run(first);
    assert_eq!(first_view.end, Some(EndReason::NotShippable));
    assert!(
        client.waiver(first, "ci").is_none(),
        "an ended Run never changes"
    );
}

#[tokio::test]
async fn a_waived_cancelled_step_keeps_its_outcome_rather_than_running_again() {
    let harness = harness_with_slow_step(
        "version: 1\nsteps:\n  ci: { uses: ci }\n  slow: { uses: ci }\ngate: [ci, slow]\n",
    );
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let first = client.history()[0];
    client.subscribe(first).await;
    harness.github.set_checks(
        &repo(),
        1,
        checks(ChecksState::Success, CheckState::Success),
    );
    client.refresh().await;
    client
        .until("ci to settle", |c| {
            matches!(
                c.run(first).step("ci").map(|s| &s.status),
                Some(StepStatus::Settled { .. })
            )
        })
        .await;
    client.ok(Command::CancelRun { run: first }).await;
    client.until("the Run to end", |c| ended(c, first)).await;
    assert_eq!(client.verdict(first, "slow").0, Verdict::Cancelled);

    // Waive the hanging Step's cancelled Verdict, as the Gate semantics
    // suggest for a Step that never reports.
    client
        .ok(waive(
            first,
            "slow",
            WaiverCategory::DoesntApply,
            "slow never finishes on this repo",
        ))
        .await;
    let second = client.next_run_ends(first).await;

    assert_eq!(
        client.verdict(second, "slow"),
        (Verdict::Cancelled, Some(first))
    );
    assert_eq!(client.verdict(second, "ci"), (Verdict::Pass, Some(first)));
    assert!(started_steps(&harness.store, second).is_empty());
    assert_eq!(client.run(second).end, Some(EndReason::Shippable));
    assert!(client.run(second).waived);
}

#[tokio::test]
async fn a_gate_override_waives_every_failing_term_in_one_action() {
    let harness = harness(CI_AND_LINT);
    let (mut client, first) = first_run_fails(&harness).await;
    assert_eq!(client.verdict(first, "lint").0, Verdict::Fail);

    client
        .ok(Command::OverrideGate {
            run: first,
            category: WaiverCategory::AcceptedRisk,
            reason: "shipping the hotfix now".into(),
        })
        .await;
    let second = client.next_run_ends(first).await;

    for step in ["ci", "lint"] {
        let waiver = client.waiver(second, step).expect("both are waived");
        assert_eq!(waiver.category, WaiverCategory::AcceptedRisk);
        assert_eq!(waiver.reason, "shipping the hotfix now");
    }
    assert!(started_steps(&harness.store, second).is_empty());
    assert_eq!(client.run(second).end, Some(EndReason::Shippable));
    assert!(client.run(second).waived);
    client
        .until("the row to show the waived end", |c| c.pr().runs[0].waived)
        .await;

    let (code, message) = client
        .refused(Command::OverrideGate {
            run: second,
            category: WaiverCategory::AcceptedRisk,
            reason: "again".into(),
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid);
    assert!(message.contains("isn't failing"), "{message}");
}

#[tokio::test]
async fn a_step_skipped_behind_a_waived_step_gets_its_first_run() {
    let harness = harness(
        "version: 1\nsteps:\n  ci: { uses: ci }\n  after: { uses: ci, needs: [ci], with: { name: after } }\ngate: [ci]\n",
    );
    let (mut client, first) = first_run_fails(&harness).await;
    assert_eq!(client.verdict(first, "after").0, Verdict::Skipped);

    client
        .ok(waive(first, "ci", WaiverCategory::FalsePositive, "flaky"))
        .await;
    let second = client.next_run_ends(first).await;

    // The Waiver counts ci as pass for Conditions too, so `after` now
    // runs. ci, which settled on its own, doesn't run again.
    assert_eq!(client.verdict(second, "ci"), (Verdict::Fail, Some(first)));
    assert_eq!(client.verdict(second, "after"), (Verdict::Fail, None));
    assert_eq!(started_steps(&harness.store, second), ["after"]);
    assert_eq!(client.run(second).end, Some(EndReason::Shippable));
}

#[tokio::test]
async fn a_push_after_a_waiver_judges_the_new_sha_without_it() {
    let harness = harness(slopwatch_daemon::github::fake::CI_PIPELINE);
    let (mut client, first) = first_run_fails(&harness).await;
    client
        .ok(waive(first, "ci", WaiverCategory::FixInFollowup, "see #12"))
        .await;
    let waived = client.next_run_ends(first).await;
    assert_eq!(client.run(waived).end, Some(EndReason::Shippable));

    let pushed = harness.github.push(&repo(), 1);
    fail_checks(&harness);
    client.refresh().await;
    let third = client.next_run_ends(waived).await;

    let view = client.run(third);
    assert_eq!(view.head_sha, pushed);
    assert_eq!(client.verdict(third, "ci"), (Verdict::Fail, None));
    assert!(client.waiver(third, "ci").is_none());
    assert_eq!(view.gate, Some(GateState::Fail));
    assert_eq!(view.end, Some(EndReason::NotShippable));
    assert!(!view.waived);
    assert_eq!(started_steps(&harness.store, third), ["ci"]);
}

#[tokio::test]
async fn only_a_settled_non_pass_verdict_can_be_waived() {
    let harness = harness(slopwatch_daemon::github::fake::CI_PIPELINE);
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history()[0];
    client.subscribe(run).await;
    let category = WaiverCategory::FalsePositive;

    let (code, message) = client.refused(waive(run, "ci", category, "flaky")).await;
    assert_eq!(code, ErrorCode::Invalid);
    assert!(message.contains("still running"), "{message}");

    let (code, _) = client.refused(waive(run, "nope", category, "x")).await;
    assert_eq!(code, ErrorCode::NotFound);
    let (code, _) = client.refused(waive(RunId(999), "ci", category, "x")).await;
    assert_eq!(code, ErrorCode::NotFound);

    harness.github.set_checks(
        &repo(),
        1,
        checks(ChecksState::Success, CheckState::Success),
    );
    client.refresh().await;
    client.until("the Run to end", |c| ended(c, run)).await;
    let (code, message) = client.refused(waive(run, "ci", category, "x")).await;
    assert_eq!(code, ErrorCode::Invalid);
    assert!(message.contains("passed"), "{message}");
    assert_eq!(client.history(), [run], "no Run started");
}

#[tokio::test]
async fn a_waiver_needs_a_reason_and_the_latest_run() {
    let harness = harness(slopwatch_daemon::github::fake::CI_PIPELINE);
    let (mut client, first) = first_run_fails(&harness).await;
    let category = WaiverCategory::FalsePositive;

    let (code, _) = client.refused(waive(first, "ci", category, "  ")).await;
    assert_eq!(code, ErrorCode::Invalid);

    client.ok(waive(first, "ci", category, "flaky")).await;
    let second = client.next_run_ends(first).await;

    let (code, message) = client.refused(waive(first, "ci", category, "again")).await;
    assert_eq!(code, ErrorCode::Invalid);
    assert!(message.contains("latest Run"), "{message}");
    let (code, message) = client.refused(waive(second, "ci", category, "again")).await;
    assert_eq!(code, ErrorCode::Invalid);
    assert!(message.contains("already waived"), "{message}");
    assert_eq!(client.history(), [second, first]);
}

#[tokio::test]
async fn a_waiver_in_a_run_still_going_moves_its_gate_at_once() {
    let harness = harness_with_slow_step(
        "version: 1\nsteps:\n  ci: { uses: ci }\n  slow: { uses: ci }\ngate: [ci]\n",
    );
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history()[0];
    client.subscribe(run).await;
    fail_checks(&harness);
    client.refresh().await;
    client
        .until("the Gate to fail", |c| {
            c.run(run).gate == Some(GateState::Fail)
        })
        .await;

    client
        .ok(waive(run, "ci", WaiverCategory::FalsePositive, "flaky"))
        .await;

    let view = client.run(run);
    assert_eq!(view.gate, Some(GateState::Pass));
    assert!(view.step("ci").unwrap().waiver.is_some());
    assert_eq!(view.end, None, "the advisory Step still runs");
    assert_eq!(client.history(), [run], "no new Run");
}
