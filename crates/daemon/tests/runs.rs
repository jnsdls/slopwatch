//! Runs end to end: a fake GitHub, the daemon over the in-process
//! transport, and real Step processes from the `slopwatchd` binary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

mod support;

use slopwatch_core::{EndReason, GateState, Verdict};
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::github::{GitHub, GitHubError};
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
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
    daemon_running(
        github,
        store,
        data,
        Path::new(env!("CARGO_BIN_EXE_slopwatchd")),
    )
}

/// A daemon whose built-in Plugins run from `plugin_exe`.
fn daemon_running(
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
            keychain: Arc::new(MemoryKeychain::default()),
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
            // These tests don't subscribe to the Inbox.
            TopicUpdate::Inbox { .. } => {}
            TopicUpdate::Run { id, seq, event, .. } => {
                self.events.push((id, seq));
                let view = self.runs.entry(id).or_default();
                assert_eq!(seq, view.seq + 1, "Run events arrive in order, once each");
                view.apply(seq, event);
            }
            TopicUpdate::StepLog { .. }
            | TopicUpdate::Notifications { .. }
            | TopicUpdate::Pipeline { .. } => {}
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
    for stored in store.events_after(run, 0).unwrap() {
        if stored.seq <= seq {
            let event: RunEvent = serde_json::from_str(&stored.event).unwrap();
            view.apply(stored.seq, event);
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

// Crash-only restart (ADR 0009). Each life of the daemon gets a runtime of
// its own, and dropping that runtime drops every task at once, the way a
// SIGKILL would. The Step processes stay behind, as they would.

/// A Pipeline with `ci` and a `slow` Step, both in the Gate.
const SLOW_PIPELINE: &str =
    "version: 1\nsteps:\n  ci: { uses: ci }\n  slow: { uses: ci }\ngate: [ci, slow]\n";

/// A PR whose base has [`SLOW_PIPELINE`], with passing checks, and a data
/// dir that outlives each daemon.
struct Restarts {
    /// Lives across every life, so a crash leaves `slow` behind for the
    /// next daemon to kill, and whatever is left dies with the test.
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    data: tempfile::TempDir,
    bin: tempfile::TempDir,
}

impl Restarts {
    fn new() -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        github.open_pr(&repo(), 1, "me", "Add the thing");
        github.label_on_github(&repo(), 1, true);
        github.set_pipeline(&repo(), "main", SLOW_PIPELINE);
        github.set_checks(
            &repo(),
            1,
            checks(ChecksState::Success, CheckState::Success),
        );
        let bin = tempfile::tempdir().unwrap();
        let restarts = Self {
            _reaper: support::Reaper::new(bin.path()),
            github,
            data: tempfile::tempdir().unwrap(),
            bin,
        };
        restarts.write_plugins();
        restarts
    }

    /// Stands in for the built-in Plugins. Step `slow` starts a child in
    /// its group, notes both pids, and waits until the daemon goes away and
    /// its stdin closes. Its leader then exits and leaves the child behind,
    /// which gives up once `bin` is gone or after about two minutes. Once
    /// `release` exists, `slow` runs as `ci`.
    fn write_plugins(&self) {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = self.bin.path().display();
        let script = format!(
            "#!/bin/sh\n\
             if [ \"$SLOPWATCH_STEP\" = slow ] && [ ! -e '{bin}/release' ]; then\n\
             \x20 (i=0; while [ -d '{bin}' ] && [ $i -lt 1200 ]; do sleep 0.1; i=$((i + 1)); done) &\n\
             \x20 echo $$ $! >> '{bin}/slow.pids'\n\
             \x20 while read line; do :; done\n\
             \x20 exit 0\n\
             fi\n\
             exec '{exe}' \"$@\"\n",
            exe = env!("CARGO_BIN_EXE_slopwatchd"),
        );
        let path = self.plugins();
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn plugins(&self) -> PathBuf {
        self.bin.path().join("plugins.sh")
    }

    /// Lets later `slow` Steps pass.
    fn release_slow(&self) {
        std::fs::write(self.bin.path().join("release"), "").unwrap();
    }

    /// The leader and child pid of each `slow` process so far.
    fn slow_pids(&self) -> Vec<(i32, i32)> {
        std::fs::read_to_string(self.bin.path().join("slow.pids"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let (leader, child) = line.split_once(' ').unwrap();
                (leader.parse().unwrap(), child.parse().unwrap())
            })
            .collect()
    }

    /// Runs one life of the daemon: `life` drives it, and then the daemon
    /// dies with no warning.
    fn life<T>(&self, life: impl AsyncFnOnce(&Arc<Daemon>, &mut Client) -> T) -> T {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let store = Store::open(&self.data.path().join("state.db")).unwrap();
            let daemon = daemon_running(&self.github, &store, &self.data, &self.plugins());
            let mut client = Client::connect(&daemon).await;
            life(&daemon, &mut client).await
        });
        drop(runtime);
        result
    }

    fn events(&self, run: RunId) -> Vec<RunEvent> {
        let store = Store::open(&self.data.path().join("state.db")).unwrap();
        store
            .events_after(run, 0)
            .unwrap()
            .into_iter()
            .map(|stored| serde_json::from_str(&stored.event).unwrap())
            .collect()
    }

    /// How many times the Run started `step`.
    fn starts(&self, run: RunId, step: &str) -> usize {
        self.events(run)
            .iter()
            .filter(|event| matches!(event, RunEvent::StepStarted { step: s, .. } if s == step))
            .count()
    }
}

/// The first life: the Run starts, `ci` passes, and `slow` is running when
/// the daemon dies.
async fn crash_mid_run(restarts: &Restarts, client: &mut Client) -> RunId {
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history()[0];
    client.subscribe(run, None).await;
    client
        .until("ci to pass", |c| {
            matches!(
                c.ci(run),
                StepStatus::Settled {
                    verdict: Verdict::Pass,
                    ..
                }
            )
        })
        .await;
    until(|| restarts.slow_pids().len() == 1).await;
    run
}

async fn until(done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn slow_status(client: &Client, run: RunId) -> StepStatus {
    client.run(run).step("slow").unwrap().status.clone()
}

#[test]
fn a_restart_resumes_the_run_with_its_settled_outcomes_and_kills_what_was_left() {
    let restarts = Restarts::new();
    let run = restarts.life(async |_, client| crash_mid_run(&restarts, client).await);
    let (_, child) = restarts.slow_pids()[0];
    assert!(alive(child), "a crash leaves the Step's child running");
    restarts.release_slow();

    restarts.life(async |_, client| {
        until(|| !alive(child)).await;
        let run_dir = restarts.data.path().join("worktrees").join(run.to_string());
        assert!(!run_dir.join("slow.1").exists(), "its directory went too");

        client.subscribe(run, None).await;
        client.refresh().await;
        client.until("the Run to end", |c| ended(c, run)).await;

        assert_eq!(client.history(), [run], "the same Run, no cancelled one");
        assert_eq!(client.run(run).end, Some(EndReason::Shippable));
        assert!(matches!(
            slow_status(client, run),
            StepStatus::Settled {
                verdict: Verdict::Pass,
                ..
            }
        ));
    });

    assert_eq!(restarts.starts(run, "ci"), 1, "ci's Outcome was kept");
    assert_eq!(restarts.starts(run, "slow"), 2, "slow started again");
}

#[test]
fn nothing_resumes_until_a_poll_has_answered() {
    let restarts = Restarts::new();
    let run = restarts.life(async |_, client| crash_mid_run(&restarts, client).await);
    restarts.release_slow();
    restarts
        .github
        .fail_polls(Some(GitHubError::Other("offline".into())));

    restarts.life(async |daemon, client| {
        client.subscribe(run, None).await;
        let _ = daemon.poll().await;
        client
            .ok(Command::Watch {
                repo: repo(),
                number: 1,
            })
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(restarts.starts(run, "slow"), 1, "slow waits for a poll");

        restarts.github.fail_polls(None);
        client.refresh().await;
        client.until("the Run to end", |c| ended(c, run)).await;
        assert_eq!(client.run(run).end, Some(EndReason::Shippable));
    });
}

#[test]
fn a_push_while_the_daemon_was_down_ends_the_run_and_starts_another() {
    let restarts = Restarts::new();
    let first = restarts.life(async |_, client| crash_mid_run(&restarts, client).await);
    let pushed = restarts.github.push(&repo(), 1);
    restarts.release_slow();

    restarts.life(async |_, client| {
        client.subscribe(first, None).await;
        client.refresh().await;
        client
            .until("the first Run to end", |c| ended(c, first))
            .await;

        assert_eq!(client.run(first).end, Some(EndReason::Superseded));
        assert!(matches!(
            slow_status(client, first),
            StepStatus::Settled {
                verdict: Verdict::Cancelled,
                ..
            }
        ));
        assert_eq!(client.history().len(), 2);
        assert_eq!(client.pr().runs[0].head_sha, pushed);
    });

    assert_eq!(restarts.starts(first, "slow"), 1, "slow didn't start again");
}

#[test]
fn a_step_interrupted_by_two_restarts_in_a_row_ends_in_error() {
    let restarts = Restarts::new();
    let run = restarts.life(async |_, client| crash_mid_run(&restarts, client).await);
    // The second restart is the `restart` command an update sends.
    restarts.life(async |daemon, client| {
        client.refresh().await;
        until(|| restarts.slow_pids().len() == 2).await;
        daemon.kill_steps().await;
    });

    restarts.life(async |_, client| {
        let (_, child) = restarts.slow_pids()[1];
        until(|| !alive(child)).await;
        client.subscribe(run, None).await;
        client.refresh().await;
        client.until("the Run to end", |c| ended(c, run)).await;

        let StepStatus::Settled {
            verdict, reason, ..
        } = slow_status(client, run)
        else {
            panic!("slow settled");
        };
        assert_eq!(verdict, Verdict::Error);
        let reason = reason.unwrap_or_default();
        assert!(reason.starts_with("error(daemon_restart)"), "{reason}");
        assert_eq!(client.run(run).end, Some(EndReason::NotShippable));
    });

    assert_eq!(restarts.slow_pids().len(), 2, "no third start");
    let store = Store::open(&restarts.data.path().join("state.db")).unwrap();
    let open = store.open_entries().unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    let (entry, _) = &open[0];
    assert_eq!(entry.title, "Not shippable");
    assert!(
        entry
            .reasons
            .iter()
            .any(|r| r.contains("error(daemon_restart)")),
        "the PR entry names it: {entry:?}"
    );
}

#[test]
fn a_run_whose_pipeline_stopped_loading_ends_not_shippable_and_its_steps_die() {
    let restarts = Restarts::new();
    let steps = restarts.data.path().join("steps");
    std::fs::create_dir_all(&steps).unwrap();
    std::fs::write(steps.join("slow.yml"), "uses: ci\n").unwrap();
    restarts.github.set_pipeline(
        &repo(),
        "main",
        "version: 1\nsteps:\n  ci: { uses: ci }\n  slow: { uses: lib/slow }\ngate: [ci, slow]\n",
    );
    let run = restarts.life(async |_, client| crash_mid_run(&restarts, client).await);
    let (_, child) = restarts.slow_pids()[0];
    std::fs::remove_file(steps.join("slow.yml")).unwrap();

    restarts.life(async |_, client| {
        until(|| !alive(child)).await;
        client.subscribe(run, None).await;

        assert_eq!(client.run(run).end, Some(EndReason::NotShippable));
        let StepStatus::Settled {
            verdict, reason, ..
        } = slow_status(client, run)
        else {
            panic!("slow settled");
        };
        assert_eq!(verdict, Verdict::Error);
        let reason = reason.unwrap_or_default();
        assert!(reason.contains("no longer loads"), "{reason}");
    });
}

/// CI plus a second Step, `lint`, configured with `name`.
fn ci_and_lint(name: &str) -> String {
    format!(
        "version: 1\nsteps:\n  ci: {{ uses: ci }}\n  lint: {{ uses: ci, with: {{ name: {name} }} }}\ngate: [ci, lint]\n"
    )
}

fn pass_checks(harness: &Harness, number: u64) {
    harness.github.set_checks(
        &repo(),
        number,
        checks(ChecksState::Success, CheckState::Success),
    );
}

impl Client {
    fn history_of(&self, number: u64) -> Vec<RunId> {
        self.prs
            .pr(&repo(), number)
            .map(|pr| pr.runs.iter().map(|run| run.id).collect())
            .unwrap_or_default()
    }

    /// The Step's Verdict and the Run its Outcome was reused from.
    fn settled(&self, run: RunId, step: &str) -> (Verdict, Option<RunId>) {
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

    /// Waits for PR `number` to get a Run after `after`, subscribes to it
    /// and waits for it to end.
    async fn next_run_ends(&mut self, number: u64, after: RunId) -> RunId {
        self.until("a new Run", |c| {
            c.history_of(number)
                .first()
                .is_some_and(|&run| run != after)
        })
        .await;
        let run = self.history_of(number)[0];
        self.subscribe(run, None).await;
        self.until("the new Run to end", |c| ended(c, run)).await;
        run
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

/// Ends the first Run on PR 1 as shippable, with CI passed.
async fn first_run_passes(harness: &Harness) -> (Client, RunId) {
    let (mut client, run) = first_run(harness).await;
    pass_checks(harness, 1);
    client.refresh().await;
    client
        .until("the first Run to end", |c| ended(c, run))
        .await;
    assert_eq!(client.run(run).end, Some(EndReason::Shippable));
    (client, run)
}

#[tokio::test]
async fn a_pipeline_change_that_adds_a_step_runs_only_that_step_on_every_ended_pr() {
    let harness = harness();
    harness.github.open_pr(&repo(), 2, "me", "Another thing");
    harness.github.label_on_github(&repo(), 2, true);
    pass_checks(&harness, 1);
    pass_checks(&harness, 2);
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let firsts: Vec<RunId> = [1, 2].map(|n| client.history_of(n)[0]).to_vec();
    for &run in &firsts {
        client.subscribe(run, None).await;
    }
    client.refresh().await;
    client
        .until("both first Runs to end", |c| {
            firsts.iter().all(|&run| ended(c, run))
        })
        .await;

    harness
        .github
        .set_pipeline(&repo(), "main", &ci_and_lint("lint"));
    client.refresh().await;

    for (number, first) in [1, 2].into_iter().zip(firsts) {
        let second = client.next_run_ends(number, first).await;
        let view = client.run(second);
        assert_eq!(view.head_sha, client.run(first).head_sha, "the same SHA");
        assert_eq!(view.base_sha, harness.github.branch_sha(&repo(), "main"));
        assert_eq!(
            client.settled(second, "ci"),
            (Verdict::Pass, Some(first)),
            "ci is reused"
        );
        assert_eq!(client.settled(second, "lint"), (Verdict::Pass, None));
        assert_eq!(started_steps(&harness.store, second), ["lint"]);
        assert_eq!(view.end, Some(EndReason::Shippable));
    }
}

#[tokio::test]
async fn changing_a_steps_with_reruns_only_that_step() {
    let harness = harness();
    harness
        .github
        .set_pipeline(&repo(), "main", &ci_and_lint("one"));
    let (mut client, first) = first_run_passes(&harness).await;
    assert_eq!(started_steps(&harness.store, first), ["ci", "lint"]);

    harness
        .github
        .set_pipeline(&repo(), "main", &ci_and_lint("two"));
    client.refresh().await;
    let second = client.next_run_ends(1, first).await;

    assert_eq!(client.settled(second, "ci"), (Verdict::Pass, Some(first)));
    assert_eq!(client.settled(second, "lint"), (Verdict::Pass, None));
    assert_eq!(started_steps(&harness.store, second), ["lint"]);
}

#[tokio::test]
async fn a_base_commit_that_leaves_the_pipeline_alone_starts_no_run() {
    let harness = harness();
    let (mut client, first) = first_run_passes(&harness).await;

    let moved =
        harness
            .github
            .set_pipeline(&repo(), "main", slopwatch_daemon::github::fake::CI_PIPELINE);
    assert_ne!(moved, client.run(first).base_sha);
    client.refresh().await;
    client.refresh().await;

    assert_eq!(client.history(), [first]);
}

#[tokio::test]
async fn a_pipeline_broken_and_restored_on_the_base_blocks_and_then_unblocks() {
    let harness = harness();
    let (mut client, first) = first_run_passes(&harness).await;

    harness.github.set_pipeline(
        &repo(),
        "main",
        "version: 1\nsteps:\n  review: { uses: nobody }\ngate: [review]\n",
    );
    client.refresh().await;
    client
        .until("the PR to show the block", |c| c.pr().blocked.is_some())
        .await;
    assert_eq!(client.history(), [first]);

    harness.github.add_pipeline(&repo(), "main");
    client.refresh().await;
    client
        .until("the block to clear", |c| c.pr().blocked.is_none())
        .await;
    assert_eq!(
        client.history(),
        [first],
        "the Pipeline is the one the latest Run read"
    );
}

#[tokio::test]
async fn an_errored_outcome_runs_again_in_the_next_run() {
    let harness = harness();
    let (mut client, first) = first_run(&harness).await;
    let active = harness.store.active_runs().unwrap();
    let ci = &active[0].steps[0];
    let slopwatch_daemon::store::StepRowState::Running { pgid, .. } = ci.state else {
        panic!("ci runs: {ci:?}");
    };
    std::process::Command::new("kill")
        .args(["-KILL", &pgid.to_string()])
        .status()
        .unwrap();
    client
        .until("the first Run to end", |c| ended(c, first))
        .await;
    assert_eq!(client.settled(first, "ci").0, Verdict::Error);

    pass_checks(&harness, 1);
    harness
        .github
        .set_pipeline(&repo(), "main", &ci_and_lint("lint"));
    client.refresh().await;
    let second = client.next_run_ends(1, first).await;

    assert_eq!(client.settled(second, "ci"), (Verdict::Pass, None));
    assert_eq!(started_steps(&harness.store, second), ["ci", "lint"]);
}

#[tokio::test]
async fn a_pr_waiting_for_a_pipeline_starts_its_first_run_once_it_lands() {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    github.open_pr(&repo(), 1, "me", "Add the thing");
    github.label_on_github(&repo(), 1, true);
    let store = Store::in_memory();
    let data = tempfile::tempdir().unwrap();
    let daemon = daemon(&github, &store, &data);
    let mut client = Client::connect(&daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    assert!(client.history().is_empty());

    github.add_pipeline(&repo(), "main");
    client.refresh().await;

    client
        .until("the first Run", |c| c.history().len() == 1)
        .await;
    let run = client.history()[0];
    client.subscribe(run, None).await;
    assert_eq!(*client.ci(run), StepStatus::Running);
}

#[tokio::test]
async fn a_run_still_going_isnt_interrupted_by_a_pipeline_change() {
    let harness = harness();
    let (mut client, first) = first_run(&harness).await;

    harness
        .github
        .set_pipeline(&repo(), "main", &ci_and_lint("lint"));
    client.refresh().await;

    assert_eq!(client.history(), [first]);
    assert_eq!(*client.ci(first), StepStatus::Running);
    let steps: Vec<&str> = client
        .run(first)
        .steps
        .iter()
        .map(|s| s.info.id.as_str())
        .collect();
    assert_eq!(steps, ["ci"], "the Run keeps the Pipeline it started with");

    // Once it ends, the new Pipeline judges the PR, reusing what it can.
    pass_checks(&harness, 1);
    client.refresh().await;
    client
        .until("the first Run to end", |c| ended(c, first))
        .await;
    assert_eq!(client.run(first).end, Some(EndReason::Shippable));
    client.refresh().await;
    let second = client.next_run_ends(1, first).await;
    assert_eq!(client.settled(second, "ci"), (Verdict::Pass, Some(first)));
    assert_eq!(started_steps(&harness.store, second), ["lint"]);
}
