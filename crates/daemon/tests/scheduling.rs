//! Scheduling within and across Runs: parallel Steps, Conditions and skip
//! cascades, the Step caps, timeouts and stalls, cancel and retry. The
//! daemon runs over the in-process transport against a fake GitHub, and
//! its Steps are a shell script Plugin each test drives through files.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

mod support;

use serde_json::json;
use slopwatch_core::{EndReason, GateState, SkipReason, Verdict, Workspace};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, STEP_CAP, Watching};
use slopwatch_protocol::step::{Manifest, STEP_DIALECT};
use slopwatch_protocol::{
    ClientFrame, ClientHello, Command, ErrorCode, Reply, RepoName, ResponseBody, RunId, RunView,
    ServerFrame, StepStatus, Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

/// Generous, since a cancel waits out the daemon's 10 s grace before
/// SIGTERM, and a loaded machine stretches every step.
const WAIT: Duration = Duration::from_secs(60);

/// The `script` and `solo` Plugins. Each attempt records itself in the
/// control dir under `<run>-<step>`, then acts on the Step's
/// `with: { act: ... }`:
///
/// - `pass`, `fail`: report that Verdict at once.
/// - `silent`: write nothing, ever.
/// - `hold`: start a child in its group, then exit on `cancel`.
/// - anything else: wait until the test writes `<run>-<step>.<attempt>`,
///   then report the Verdict it holds, or exit without one for `crash`.
///
/// Every wait gives up once the control dir is gone or after about two
/// minutes, so no Step outlives its test.
const SCRIPT: &str = r#"
dir=$1
at="$dir/$SLOPWATCH_RUN-$SLOPWATCH_STEP"
await() {
  i=0
  until "$@"; do
    [ -d "$dir" ] && [ "$i" -lt 2400 ] || exit 1
    sleep 0.05; i=$((i + 1))
  done
}
read -r start
act=$(printf '%s' "$start" | sed -n 's/.*"act":"\([a-z]*\)".*/\1/p')
n=$(( $(cat "$at.attempts" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$at.attempts"
echo "$$" > "$at.pid"
echo "$PATH" > "$at.path"
outcome() { printf '{"type":"outcome","verdict":"%s"}\n' "$1"; }
case $act in
  pass|fail) outcome "$act" ;;
  silent) await false ;;
  hold)
    ( await false ) &
    echo "$!" > "$at.child"
    while read -r line; do case $line in *cancel*) exit 0 ;; esac; done ;;
  *)
    await [ -f "$at.$n" ]
    verdict=$(cat "$at.$n")
    [ "$verdict" = crash ] && exit 3
    outcome "$verdict" ;;
esac
"#;

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

fn manifest(id: &str, concurrency: Option<u32>) -> Manifest {
    Manifest {
        id: id.into(),
        version: "1.0.0".into(),
        dialect: STEP_DIALECT,
        features: vec![],
        config_schema: json!({ "type": "object" }),
        workspace: Workspace::None,
        effects: vec![],
        secrets: vec![],
        timeout: None,
        stall_after: None,
        concurrency,
    }
}

struct Harness {
    /// First, so the Steps die before their control dir goes.
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    daemon: Arc<Daemon>,
    /// Where the script Steps record themselves and read their Verdicts.
    control: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Harness {
    /// PR 1 in `jnsdls/app`, labelled, with `pipeline` on main.
    fn new(pipeline: &str) -> Self {
        Self::with_login_path(pipeline, None)
    }

    fn with_login_path(pipeline: &str, login_path: Option<&str>) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        github.open_pr(&repo(), 1, "me", "Add the thing");
        github.label_on_github(&repo(), 1, true);
        github.set_pipeline(&repo(), "main", pipeline);
        let data = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        let store = Store::in_memory();
        let dyn_github: Arc<dyn GitHub> = Arc::clone(&github) as Arc<dyn GitHub>;
        let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
        let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
        let script = |id: &str, cap| {
            (
                manifest(id, cap),
                PathBuf::from("/bin/sh"),
                vec![
                    "-c".to_owned(),
                    SCRIPT.to_owned(),
                    id.to_owned(),
                    control.path().to_str().unwrap().to_owned(),
                ],
            )
        };
        let (m, p, a) = script("script", None);
        let (solo_m, solo_p, solo_a) = script("solo", Some(1));
        let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library))
            .with_plugin(m, p, a)
            .with_plugin(solo_m, solo_p, solo_a);
        let runs = Runs::start(
            store,
            dyn_github,
            Arc::clone(&watching),
            RunsConfig {
                data_dir: data.path().to_owned(),
                plugins,
                login_path: login_path.map(str::to_owned),
                retention: Retention::default(),
            },
        )
        .unwrap();
        let daemon = Arc::new(Daemon::with_build_id("test", watching, library).with_runs(runs));
        Harness {
            _reaper: support::Reaper::new(control.path()),
            github,
            daemon,
            control: control.path().to_owned(),
            _dirs: (data, control),
        }
    }

    /// The control file `what` of `step` in `run`.
    fn file(&self, run: RunId, step: &str, what: &str) -> PathBuf {
        self.control.join(format!("{run}-{step}.{what}"))
    }

    /// Lets attempt `attempt` of `step` in `run` report `verdict`.
    fn release(&self, run: RunId, step: &str, attempt: u32, verdict: &str) {
        std::fs::write(self.file(run, step, &attempt.to_string()), verdict).unwrap();
    }

    fn attempts(&self, run: RunId, step: &str) -> u32 {
        std::fs::read_to_string(self.file(run, step, "attempts"))
            .map(|text| text.trim().parse().unwrap())
            .unwrap_or(0)
    }

    /// The pid the script wrote. The shell creates the file before it
    /// writes the number, so a read can catch it empty.
    fn pid(&self, run: RunId, step: &str, what: &str) -> i32 {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let text = std::fs::read_to_string(self.file(run, step, what)).unwrap_or_default();
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
            assert!(std::time::Instant::now() < deadline, "no pid in `{what}`");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
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
                let view = self.runs.entry(id).or_default();
                assert_eq!(seq, view.seq + 1, "Run events arrive in order, once each");
                view.apply(seq, event);
            }
            TopicUpdate::StepLog { .. } | TopicUpdate::Notifications { .. } => {}
        }
    }

    /// Applies frames until `done` holds.
    async fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            let frame = tokio::time::timeout_at(deadline, self.connection.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}: {:#?}", self.runs))
                .unwrap();
            match frame {
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    /// The newest Run of PR `number`.
    fn newest(&self, number: u64) -> RunId {
        self.prs.pr(&repo(), number).expect("the PR is listed").runs[0].id
    }

    fn status(&self, run: RunId, step: &str) -> &StepStatus {
        &self.runs[&run]
            .step(step)
            .unwrap_or_else(|| panic!("the Run lists {step}"))
            .status
    }

    fn verdict(&self, run: RunId, step: &str) -> Option<(Verdict, Option<String>)> {
        match self.status(run, step) {
            StepStatus::Settled {
                verdict, reason, ..
            } => Some((*verdict, reason.clone())),
            _ => None,
        }
    }

    fn running(&self, run: RunId) -> Vec<&str> {
        self.runs[&run]
            .steps
            .iter()
            .filter(|step| step.status == StepStatus::Running)
            .map(|step| step.info.id.as_str())
            .collect()
    }

    fn end(&self, run: RunId) -> Option<EndReason> {
        self.runs.get(&run).and_then(|view| view.end)
    }
}

/// Adds the repo, which starts PR 1's Run, and subscribes to it.
async fn first_run(harness: &Harness) -> (Client, RunId) {
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.newest(1);
    client
        .ok(Command::Subscribe {
            topic: Topic::Run(run),
            since: None,
        })
        .await;
    (client, run)
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn until_dead(pid: i32) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while alive(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "process {pid} still runs"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn steps_with_no_edge_between_them_run_together_and_their_dependent_waits_for_both() {
    let harness = Harness::new(
        "version: 1
steps:
  a: { uses: script }
  b: { uses: script }
  both: { uses: script, needs: [a, b] }
gate: [both]
",
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("a and b to run", |c| c.running(run) == ["a", "b"])
        .await;
    assert_eq!(*client.status(run, "both"), StepStatus::Pending);

    harness.release(run, "a", 1, "pass");
    client
        .until("a to pass", |c| c.verdict(run, "a").is_some())
        .await;
    assert_eq!(client.running(run), ["b"], "both waits for b");
    assert_eq!(*client.status(run, "both"), StepStatus::Pending);

    harness.release(run, "b", 1, "pass");
    client
        .until("both to start", |c| c.running(run) == ["both"])
        .await;
    harness.release(run, "both", 1, "pass");
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    assert_eq!(client.runs[&run].gate, Some(GateState::Pass));
    assert_eq!(client.end(run), Some(EndReason::Shippable));
}

#[tokio::test]
async fn a_determined_gate_leaves_running_steps_alone_and_the_run_ends_once_they_settle() {
    let harness = Harness::new(
        "version: 1
steps:
  judged: { uses: script }
  advisory: { uses: script }
gate: [judged]
",
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("both to run", |c| c.running(run).len() == 2)
        .await;

    harness.release(run, "judged", 1, "fail");
    client
        .until("the Gate to fail", |c| {
            c.runs[&run].gate == Some(GateState::Fail)
        })
        .await;
    assert_eq!(client.running(run), ["advisory"]);
    assert_eq!(client.end(run), None, "the Run waits for every Step");

    harness.release(run, "advisory", 1, "pass");
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;
    assert_eq!(client.end(run), Some(EndReason::NotShippable));
}

#[tokio::test]
async fn skipped_steps_carry_the_reason_core_gives_and_files_conditions_see_the_prs_files() {
    let harness = Harness::new(
        r#"version: 1
steps:
  lint: { uses: script, with: { act: fail } }
  build: { uses: script, needs: [lint] }
  deploy: { uses: script, needs: [build] }
  report: { uses: script, needs: [lint], when: always, with: { act: pass } }
  docs: { uses: script, when: { files: "docs/**" }, with: { act: pass } }
  changes: { uses: script, when: { files: "change-*.txt" }, with: { act: pass } }
gate: [report, changes]
"#,
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    let skipped = |reason: SkipReason| Some((Verdict::Skipped, Some(reason.to_string())));
    assert_eq!(
        client.verdict(run, "build"),
        skipped(SkipReason::Upstream {
            step: "lint".into(),
            verdict: Verdict::Fail,
        })
    );
    assert_eq!(
        client.verdict(run, "deploy"),
        skipped(SkipReason::Upstream {
            step: "build".into(),
            verdict: Verdict::Skipped,
        }),
        "the skip cascades"
    );
    assert_eq!(
        client.verdict(run, "docs"),
        skipped(SkipReason::Condition("{files: [docs/**]}".into())),
    );
    assert_eq!(
        client.verdict(run, "changes"),
        Some((Verdict::Pass, None)),
        "the PR changes change-1.txt"
    );
    assert_eq!(client.verdict(run, "report"), Some((Verdict::Pass, None)));
    assert_eq!(client.end(run), Some(EndReason::Shippable));
}

#[tokio::test]
async fn a_label_added_mid_run_turns_a_waiting_steps_condition() {
    let harness = Harness::new(
        r#"version: 1
steps:
  first: { uses: script }
  then: { uses: script, needs: [first], when: { not: [{ labels: [hold] }] } }
gate: [first]
"#,
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("first to run", |c| c.running(run) == ["first"])
        .await;

    harness.github.set_label(&repo(), 1, "hold", true);
    client.ok(Command::Refresh).await;
    client
        .until("then to be skipped", |c| c.verdict(run, "then").is_some())
        .await;

    assert_eq!(
        client.verdict(run, "then"),
        Some((
            Verdict::Skipped,
            Some(SkipReason::Condition("{not: [{labels: [hold]}]}".into()).to_string())
        ))
    );
    assert_eq!(client.running(run), ["first"], "first goes on");
}

#[tokio::test]
async fn cancelling_a_run_kills_its_step_groups_and_ends_it_as_cancelled() {
    let harness = Harness::new(
        r#"version: 1
steps:
  held: { uses: script, with: { act: hold } }
  waiting: { uses: script }
gate: [held, waiting]
"#,
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("both to run", |c| c.running(run).len() == 2)
        .await;
    let deadline = tokio::time::Instant::now() + WAIT;
    // A Step shows as running before its script has written its pid.
    while !harness.file(run, "held", "child").exists()
        || !harness.file(run, "waiting", "pid").exists()
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pids = [
        harness.pid(run, "held", "pid"),
        harness.pid(run, "held", "child"),
        harness.pid(run, "waiting", "pid"),
    ];

    client.ok(Command::CancelRun { run }).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    assert_eq!(client.end(run), Some(EndReason::Cancelled));
    for step in ["held", "waiting"] {
        assert_eq!(
            client.verdict(run, step).map(|(verdict, _)| verdict),
            Some(Verdict::Cancelled)
        );
    }
    for pid in pids {
        until_dead(pid).await;
    }
    let (code, message) = client.refused(Command::CancelRun { run }).await;
    assert_eq!(code, ErrorCode::Invalid);
    assert!(message.contains("already ended"), "{message}");
    let (code, _) = client.refused(Command::CancelRun { run: RunId(999) }).await;
    assert_eq!(code, ErrorCode::NotFound);
    client.ok(Command::Refresh).await;
    assert_eq!(
        client.newest(1),
        run,
        "a cancelled Run doesn't start again on the same head"
    );
}

#[tokio::test]
async fn retrying_an_errored_step_reruns_it_and_whatever_follows_in_the_same_run() {
    let harness = Harness::new(
        "version: 1
steps:
  flaky: { uses: script }
  after: { uses: script, needs: [flaky] }
  aside: { uses: script }
gate: [after, aside]
",
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("flaky and aside to run", |c| c.running(run).len() == 2)
        .await;

    harness.release(run, "flaky", 1, "crash");
    client
        .until("after to be skipped", |c| c.verdict(run, "after").is_some())
        .await;
    assert_eq!(
        client.verdict(run, "flaky").map(|(verdict, _)| verdict),
        Some(Verdict::Error)
    );
    let (code, message) = client
        .refused(Command::RetryStep {
            run,
            step: "after".into(),
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid, "{message}");

    client
        .ok(Command::RetryStep {
            run,
            step: "flaky".into(),
        })
        .await;
    client
        .until("flaky to run again", |c| {
            c.status(run, "flaky") == &StepStatus::Running
        })
        .await;
    assert_eq!(*client.status(run, "after"), StepStatus::Pending);
    assert_eq!(*client.status(run, "aside"), StepStatus::Running);

    harness.release(run, "flaky", 2, "pass");
    client
        .until("after to run", |c| {
            c.status(run, "after") == &StepStatus::Running
        })
        .await;
    harness.release(run, "after", 1, "pass");
    harness.release(run, "aside", 1, "pass");
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    assert_eq!(client.end(run), Some(EndReason::Shippable));
    assert_eq!(client.newest(1), run, "the same Run");
    assert_eq!(harness.attempts(run, "flaky"), 2);
    assert_eq!(
        harness.attempts(run, "aside"),
        1,
        "only flaky and after reran"
    );
    let (code, _) = client
        .refused(Command::RetryStep {
            run,
            step: "flaky".into(),
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid, "an ended Run can't retry");
}

#[tokio::test]
async fn a_step_past_its_timeout_or_stall_after_errors_and_is_killed() {
    let harness = Harness::new(
        r#"version: 1
steps:
  slow: { uses: script, with: { act: silent }, timeout: 1s }
  quiet: { uses: script, with: { act: silent }, stall_after: 1s }
gate: [slow, quiet]
"#,
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    assert_eq!(
        client.verdict(run, "slow"),
        Some((
            Verdict::Error,
            Some("error(timeout): ran past its 1s timeout".into())
        ))
    );
    assert_eq!(
        client.verdict(run, "quiet"),
        Some((
            Verdict::Error,
            Some("error(stall): wrote nothing for 1s".into())
        ))
    );
    until_dead(harness.pid(run, "slow", "pid")).await;
    until_dead(harness.pid(run, "quiet", "pid")).await;
}

#[tokio::test]
async fn no_more_than_the_cap_of_steps_run_at_once_across_runs() {
    let steps: String = (0..5)
        .map(|i| format!("  s{i}: {{ uses: script }}\n"))
        .collect();
    let harness = Harness::new(&format!("version: 1\nsteps:\n{steps}gate: [s0]\n"));
    harness.github.open_pr(&repo(), 2, "me", "Another thing");
    harness.github.label_on_github(&repo(), 2, true);
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let runs = [client.newest(1), client.newest(2)];
    for run in runs {
        client
            .ok(Command::Subscribe {
                topic: Topic::Run(run),
                since: None,
            })
            .await;
    }
    let running = |c: &Client| runs.iter().map(|run| c.running(*run).len()).sum::<usize>();
    client
        .until("the cap to fill", |c| running(c) == STEP_CAP)
        .await;
    // Give a ninth Step the chance to start, which it mustn't.
    tokio::time::sleep(Duration::from_millis(500)).await;
    client.ok(Command::Ping).await;
    assert_eq!(running(&client), STEP_CAP);
    assert_eq!(STEP_CAP, 8);

    let first = client.running(runs[0])[0].to_owned();
    harness.release(runs[0], &first, 1, "pass");
    client
        .until("a ninth Step to take the slot", |c| {
            runs.iter()
                .map(|run| {
                    c.runs[run]
                        .steps
                        .iter()
                        .filter(|step| step.status != StepStatus::Pending)
                        .count()
                })
                .sum::<usize>()
                > STEP_CAP
        })
        .await;
    assert!(running(&client) <= STEP_CAP);
}

#[tokio::test]
async fn a_plugin_at_its_own_cap_holds_its_steps_back() {
    let harness = Harness::new(
        "version: 1
steps:
  one: { uses: solo }
  two: { uses: solo }
  free: { uses: script }
gate: [one, two]
",
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("one solo Step and the free one to run", |c| {
            c.running(run).len() == 2
        })
        .await;
    let running: Vec<String> = client.running(run).iter().map(|s| s.to_string()).collect();
    assert!(running.contains(&"free".to_owned()), "{running:?}");
    let first = running.iter().find(|s| *s != "free").unwrap().clone();
    let second = if first == "one" { "two" } else { "one" };
    assert_eq!(*client.status(run, second), StepStatus::Pending);

    harness.release(run, &first, 1, "pass");
    client
        .until("the other solo Step to run", |c| {
            c.status(run, second) == &StepStatus::Running
        })
        .await;
}

#[tokio::test]
async fn steps_get_the_login_shells_path_after_the_daemons_own() {
    let harness = Harness::with_login_path(
        r#"version: 1
steps:
  env: { uses: script, with: { act: pass } }
gate: [env]
"#,
        Some("/from/the/login/shell"),
    );
    let (mut client, run) = first_run(&harness).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    let path = std::fs::read_to_string(harness.file(run, "env", "path")).unwrap();
    let own = std::env::var("PATH").unwrap();
    assert!(path.trim().ends_with(":/from/the/login/shell"), "{path}");
    let first_own = own.split(':').next().unwrap();
    assert!(path.starts_with(first_own), "{path}");
}
