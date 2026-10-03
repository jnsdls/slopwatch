//! What a Run hands its Steps beyond the poll: the PR's diff for a Plugin
//! that asks for it, the issues the PR links, and `linked_issue:`
//! Conditions. Also the `usage` a Step reports, which adds up to the
//! Step's cost. The daemon runs over the in-process transport against a
//! fake GitHub, with a shell script Plugin that records what it was sent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

mod support;

use serde_json::{Value, json};
use slopwatch_core::{EndReason, Verdict, WaiverCategory, Workspace};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{LinkedIssue, Manifest, PR_DIFF, STEP_DIALECT};
use slopwatch_protocol::{
    ClientFrame, ClientHello, Command, Cost, Reply, RepoName, ResponseBody, RunId, RunView,
    ServerFrame, StepStatus, Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(60);

/// The `judge` and `plain` Plugins. Each writes the `start` it got to
/// `<run>-<step>.start` in the control dir, then acts on its `with: { act:
/// ... }`: `fail` reports fail, `crash-once` exits without an Outcome the
/// first time that Step id runs, and anything else reports two calls'
/// usage, one priced and one not, and passes.
const SCRIPT: &str = r#"
dir=$1
read -r start
printf '%s\n' "$start" > "$dir/$SLOPWATCH_RUN-$SLOPWATCH_STEP.start"
n=$(( $(cat "$dir/$SLOPWATCH_STEP.attempts" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$dir/$SLOPWATCH_STEP.attempts"
act=$(printf '%s' "$start" | sed -n 's/.*"act":"\([a-z-]*\)".*/\1/p')
case $act in
  fail) printf '{"type":"outcome","verdict":"fail"}\n'; exit 0 ;;
  crash-once) [ "$n" = 1 ] && exit 3 ;;
esac
printf '{"type":"usage","model":"m","input_tokens":300,"usd":0.0004}\n'
printf '{"type":"usage","model":"m","input_tokens":200}\n'
printf '{"type":"outcome","verdict":"pass"}\n'
"#;

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

fn manifest(id: &str, features: Vec<String>) -> Manifest {
    Manifest {
        id: id.into(),
        version: "1.0.0".into(),
        dialect: STEP_DIALECT,
        features,
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
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    daemon: Arc<Daemon>,
    control: PathBuf,
    data: PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Harness {
    /// PR 1 in `jnsdls/app`, labelled, with `pipeline` on main and a
    /// second file pushed to its head.
    fn new(pipeline: &str) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        github.open_pr(&repo(), 1, "me", "Add the thing");
        github.push_file(&repo(), 1, "src/thing.rs", "fn thing() {}\n");
        github.label_on_github(&repo(), 1, true);
        github.set_pipeline(&repo(), "main", pipeline);
        let data = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        let store = Store::in_memory();
        let dyn_github: Arc<dyn GitHub> = Arc::clone(&github) as Arc<dyn GitHub>;
        let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
        let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
        let script = |id: &str, features| {
            (
                manifest(id, features),
                PathBuf::from("/bin/sh"),
                vec![
                    "-c".to_owned(),
                    SCRIPT.to_owned(),
                    id.to_owned(),
                    control.path().to_str().unwrap().to_owned(),
                ],
            )
        };
        let (m, p, a) = script("judge", vec![PR_DIFF.to_owned()]);
        let (plain_m, plain_p, plain_a) = script("plain", vec![]);
        let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library))
            .with_plugin(m, p, a)
            .with_plugin(plain_m, plain_p, plain_a);
        let runs = Runs::start(
            store,
            dyn_github,
            Arc::clone(&watching),
            RunsConfig {
                data_dir: data.path().to_owned(),
                plugins,
                login_path: None,
                retention: Retention::default(),
                keychain: Arc::new(MemoryKeychain::default()),
            },
        )
        .unwrap();
        let daemon = Arc::new(Daemon::with_build_id("test", watching, library).with_runs(runs));
        Harness {
            _reaper: support::Reaper::new(control.path()),
            github,
            daemon,
            control: control.path().to_owned(),
            data: data.path().to_owned(),
            _dirs: (data, control),
        }
    }

    /// The `start` message `step` got in `run`.
    fn start(&self, run: RunId, step: &str) -> Value {
        let path = self.control.join(format!("{run}-{step}.start"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_str(&text).unwrap()
    }
}

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
            TopicUpdate::WatchedPrs { update, .. } => match update {
                WatchedPrsUpdate::Snapshot(snapshot) => self.prs = snapshot,
                WatchedPrsUpdate::Delta(delta) => self.prs.apply(delta),
            },
            TopicUpdate::Run { id, seq, event, .. } => {
                self.runs.entry(id).or_default().apply(seq, event);
            }
            _ => {}
        }
    }

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

    fn newest(&self, number: u64) -> RunId {
        self.prs.pr(&repo(), number).expect("the PR is listed").runs[0].id
    }

    async fn subscribe(&mut self, run: RunId) {
        self.ok(Command::Subscribe {
            topic: Topic::Run(run),
            since: None,
        })
        .await;
    }

    fn end(&self, run: RunId) -> Option<EndReason> {
        self.runs.get(&run).and_then(|view| view.end)
    }

    fn settled(&self, run: RunId, step: &str) -> (Verdict, Option<String>) {
        match &self.runs[&run].step(step).unwrap().status {
            StepStatus::Settled {
                verdict, reason, ..
            } => (*verdict, reason.clone()),
            other => panic!("{step} hasn't settled: {other:?}"),
        }
    }
}

/// Adds the repo, which starts PR 1's Run, and waits for it to end.
async fn first_run(harness: &Harness) -> (Client, RunId) {
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.newest(1);
    client.subscribe(run).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;
    (client, run)
}

fn issue(number: u64) -> LinkedIssue {
    LinkedIssue {
        repo: repo(),
        number,
        title: "The thing is missing".into(),
        body: "Add the thing.".into(),
        url: format!("https://github.com/jnsdls/app/issues/{number}"),
    }
}

#[tokio::test]
async fn a_step_that_asks_for_the_diff_gets_it_with_the_linked_issues_and_its_usage_is_its_cost() {
    let harness = Harness::new(
        "version: 1
steps:
  judge: { uses: judge }
gate: [judge]
",
    );
    harness.github.link_issue(&repo(), 1, issue(5));

    let (client, run) = first_run(&harness).await;

    assert_eq!(client.end(run), Some(EndReason::Shippable));
    let snapshot = &harness.start(run, "judge")["snapshot"];
    assert_eq!(snapshot["linked_issues"][0]["number"], 5);
    assert_eq!(
        snapshot["linked_issues"][0]["title"],
        "The thing is missing"
    );
    let path = PathBuf::from(snapshot["diff"].as_str().expect("a diff file"));
    assert!(path.starts_with(&harness.data), "{}", path.display());
    let diff = std::fs::read_to_string(&path).unwrap();
    assert!(
        diff.contains("diff --git a/src/thing.rs b/src/thing.rs"),
        "{diff}"
    );
    assert!(diff.contains("+fn thing() {}"), "{diff}");
    assert!(diff.contains("+A change."), "{diff}");
    assert!(
        !diff.contains("pipeline.yml"),
        "the base's own commits aren't the PR's"
    );

    let cost = client.runs[&run].step("judge").unwrap().cost;
    assert_eq!(
        cost,
        Some(Cost {
            usd: 0.0004,
            unknown: true
        })
    );
    assert_eq!(cost.unwrap().to_string(), "$0.0004 +?");
}

#[tokio::test]
async fn a_pipeline_whose_steps_dont_ask_for_the_diff_gets_none() {
    let harness = Harness::new(
        "version: 1
steps:
  plain: { uses: plain }
gate: [plain]
",
    );

    let (client, run) = first_run(&harness).await;

    assert_eq!(client.end(run), Some(EndReason::Shippable));
    let snapshot = &harness.start(run, "plain")["snapshot"];
    assert!(snapshot.get("diff").is_none(), "{snapshot}");
    assert!(snapshot.get("linked_issues").is_none(), "no issue linked");
    assert!(!harness.data.join("diffs").exists());
}

#[tokio::test]
async fn a_linked_issue_condition_skips_a_pr_that_links_none() {
    let pipeline = "version: 1
steps:
  issue: { uses: plain, when: { linked_issue: true } }
gate: [{ issue: [pass, skipped] }]
";
    let harness = Harness::new(pipeline);

    let (client, run) = first_run(&harness).await;

    assert_eq!(
        client.settled(run, "issue"),
        (
            Verdict::Skipped,
            Some("its Condition `{linked_issue: true}` is false".into())
        )
    );
    assert_eq!(client.end(run), Some(EndReason::Shippable));

    let harness = Harness::new(pipeline);
    harness.github.link_issue(&repo(), 1, issue(9));
    let (client, run) = first_run(&harness).await;
    assert_eq!(client.settled(run, "issue").0, Verdict::Pass);
}

#[tokio::test]
async fn a_same_sha_run_reads_the_diff_its_predecessor_kept() {
    let harness = Harness::new(
        "version: 1
steps:
  checked: { uses: plain, with: { act: fail } }
  judge: { uses: judge, with: { act: crash-once } }
gate: [checked, judge]
",
    );
    harness.github.link_issue(&repo(), 1, issue(5));
    let (mut client, first) = first_run(&harness).await;
    assert_eq!(client.end(first), Some(EndReason::NotShippable));
    assert_eq!(client.settled(first, "judge").0, Verdict::Error);

    // Waiving `checked` starts a Run on the same SHA, and the errored
    // `judge` runs again in it.
    client
        .ok(Command::WaiveStep {
            run: first,
            step: "checked".into(),
            category: WaiverCategory::FalsePositive,
            reason: "It checks the wrong thing".into(),
        })
        .await;
    client.until("a second Run", |c| c.newest(1) != first).await;
    let second = client.newest(1);
    client.subscribe(second).await;
    client
        .until("the second Run to end", |c| c.end(second).is_some())
        .await;

    assert_eq!(client.end(second), Some(EndReason::Shippable));
    let snapshot = &harness.start(second, "judge")["snapshot"];
    assert_eq!(snapshot["linked_issues"][0]["number"], 5);
    let diff = std::fs::read_to_string(snapshot["diff"].as_str().unwrap()).unwrap();
    assert!(diff.contains("+fn thing() {}"), "{diff}");
}

#[tokio::test]
async fn the_shipped_jev_presets_need_the_gateway_key() {
    let harness = Harness::new(
        "version: 1
steps:
  desc: { uses: lib/desc-matches-diff }
  issue: { uses: lib/resolves-issue, when: { linked_issue: true } }
gate: [desc, { issue: [pass, skipped] }]
",
    );

    let (client, run) = first_run(&harness).await;

    let (verdict, reason) = client.settled(run, "desc");
    assert_eq!(verdict, Verdict::Error);
    let reason = reason.unwrap();
    assert!(reason.contains("AI_GATEWAY_API_KEY"), "{reason}");
    assert_eq!(client.settled(run, "issue").0, Verdict::Skipped);
}
