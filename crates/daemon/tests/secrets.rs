//! Secrets: set and rotated through a write-only command, handed to a Step
//! only under its Plugin's Approval, masked out of everything the Step
//! writes, and a missing one held as one shared Inbox entry. The daemon
//! runs over the in-process transport against a fake GitHub and an
//! in-memory Keychain, with shell script Plugins.

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use slopwatch_core::{EndReason, Verdict, Workspace};
use slopwatch_daemon::approvals::{Approval, Grant};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Manifest, STEP_DIALECT, SecretSpec};
use slopwatch_protocol::{
    Cause, ClientFrame, ClientHello, Closing, Command, ErrorCode, Inbox, InboxUpdate, PrRef, Reply,
    RepoName, ResponseBody, RunId, RunView, Scope, SecretInfo, SecretValue, ServerFrame,
    StepStatus, Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(30);

const NAME: &str = "TEST_SECRET";
const VALUE: &str = "sk-test-70-abcdef0123456789";
const ROTATED: &str = "sk-test-70-rotated-9876543210";

/// Both Plugins run this. Each attempt writes the `TEST_SECRET` it got to
/// `<run>-<step>.secret.<attempt>` in the control dir, then acts on
/// `with: { act: ... }`: `pass` passes, `echo` writes the value everywhere
/// a Step can write and fails, `junk` writes a non-protocol line holding
/// it, and `wait` waits for the test to write `<run>-<step>.<attempt>`.
/// The wait gives up once the control dir is gone or after about two
/// minutes, so no Step outlives its test.
const SCRIPT: &str = r#"
dir=$1
at="$dir/$SLOPWATCH_RUN-$SLOPWATCH_STEP"
read -r start
act=$(printf '%s' "$start" | sed -n 's/.*"act":"\([a-z]*\)".*/\1/p')
n=$(( $(cat "$at.attempts" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$at.attempts"
printf '%s' "$TEST_SECRET" > "$at.secret.$n"
outcome() { printf '{"type":"outcome","verdict":"%s"}\n' "$1"; }
case $act in
  pass) outcome pass ;;
  echo)
    echo "stderr says $TEST_SECRET" >&2
    # A line past the 64 KB record limit, with the value across the cut.
    head -c 65530 /dev/zero | tr '\0' x >&2
    printf '%s tail\n' "$TEST_SECRET" >&2
    printf '{"type":"log","message":"log says %s"}\n' "$TEST_SECRET"
    printf '{"type":"progress","message":"progress says %s"}\n' "$TEST_SECRET"
    printf '{"type":"outcome","verdict":"fail","outputs":{"note":"note says %s","findings":[{"severity":"error","message":"finding says %s"}]}}\n' "$TEST_SECRET" "$TEST_SECRET"
    ;;
  junk) echo "junk says $TEST_SECRET"; sleep 5 ;;
  *)
    i=0
    until [ -f "$at.$n" ]; do
      [ -d "$dir" ] && [ "$i" -lt 2400 ] || exit 1
      sleep 0.05; i=$((i + 1))
    done
    outcome pass ;;
esac
"#;

fn pipeline(steps: &str, gate: &str) -> String {
    format!("version: 1\nsteps:\n{steps}gate: [{gate}]\n")
}

/// One Step that needs the Secret and passes.
fn needs_secret() -> String {
    pipeline("  check: { uses: script, with: { act: pass } }\n", "check")
}

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

fn pr(number: u64) -> PrRef {
    PrRef {
        repo: repo(),
        number,
    }
}

fn manifest(id: &str, secrets: Vec<SecretSpec>) -> Manifest {
    Manifest {
        id: id.into(),
        version: "1.0.0".into(),
        dialect: STEP_DIALECT,
        features: vec![],
        config_schema: json!({ "type": "object" }),
        workspace: Workspace::None,
        effects: vec![],
        secrets,
        timeout: None,
        stall_after: None,
        concurrency: None,
    }
}

struct Harness {
    /// First, so the Steps die before their control dir goes.
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    daemon: Arc<Daemon>,
    keychain: Arc<MemoryKeychain>,
    control: PathBuf,
    data: tempfile::TempDir,
    _control: tempfile::TempDir,
}

impl Harness {
    /// My labelled PRs `prs` in `jnsdls/app`, with `pipeline` on main. The
    /// `script` Plugin requires `TEST_SECRET`, approved when `approved`;
    /// `plain` asks for no Secret.
    fn new(pipeline: &str, prs: &[u64], approved: bool) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        for &number in prs {
            github.open_pr(&repo(), number, "me", &format!("PR {number}"));
            github.label_on_github(&repo(), number, true);
        }
        github.set_pipeline(&repo(), "main", pipeline);
        let data = tempfile::tempdir().unwrap();
        let control = tempfile::tempdir().unwrap();
        let store = Store::open(&data.path().join("state.db")).unwrap();
        if approved {
            store
                .put_approval(&Approval {
                    plugin: "script".into(),
                    grant: Grant {
                        workspace: Workspace::None,
                        effects: vec![],
                        secrets: vec![NAME.into()],
                    },
                    actor: None,
                    approved_at: 0,
                })
                .unwrap();
        }
        let keychain = Arc::new(MemoryKeychain::default());
        let daemon = daemon(&github, &store, &keychain, data.path(), control.path());
        Harness {
            _reaper: support::Reaper::new(control.path()),
            github,
            daemon,
            keychain,
            control: control.path().to_owned(),
            data,
            _control: control,
        }
    }

    /// What attempt `attempt` of `step` in `run` found in `TEST_SECRET`.
    fn received(&self, run: RunId, step: &str, attempt: u32) -> String {
        std::fs::read_to_string(self.control.join(format!("{run}-{step}.secret.{attempt}")))
            .unwrap()
    }

    fn release(&self, run: RunId, step: &str, attempt: u32) {
        std::fs::write(self.control.join(format!("{run}-{step}.{attempt}")), "").unwrap();
    }
}

fn daemon(
    github: &Arc<FakeGitHub>,
    store: &Store,
    keychain: &Arc<MemoryKeychain>,
    data: &Path,
    control: &Path,
) -> Arc<Daemon> {
    let dyn_github: Arc<dyn GitHub> = Arc::clone(github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
    let library = Arc::new(Library::open(data.join("steps")).unwrap());
    let args = |name: &str| {
        vec![
            "-c".to_owned(),
            SCRIPT.to_owned(),
            name.to_owned(),
            control.to_str().unwrap().to_owned(),
        ]
    };
    let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library))
        .with_plugin(
            manifest("script", vec![SecretSpec::required(NAME)]),
            PathBuf::from("/bin/sh"),
            args("script"),
        )
        .with_plugin(
            manifest("plain", vec![]),
            PathBuf::from("/bin/sh"),
            args("plain"),
        );
    let runs = Runs::start(
        store.clone(),
        dyn_github,
        Arc::clone(&watching),
        RunsConfig {
            data_dir: data.to_owned(),
            plugins,
            login_path: None,
            retention: Retention::default(),
            keychain: Arc::clone(keychain) as _,
        },
    )
    .unwrap();
    Arc::new(Daemon::with_build_id("test", watching, library).with_runs(runs))
}

/// A client that keeps its copy of `watched_prs`, the Inbox and its Runs,
/// and every frame it got as JSON, to check no value ever crossed.
struct Client {
    connection: InProcessClient,
    prs: WatchedPrs,
    inbox: Inbox,
    runs: HashMap<RunId, RunView>,
    seen: String,
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
            inbox: Inbox::default(),
            runs: HashMap::new(),
            seen: String::new(),
        };
        for topic in [Topic::WatchedPrs, Topic::Inbox] {
            client.ok(Command::Subscribe { topic, since: None }).await;
        }
        client
    }

    fn saw(&mut self, frame: &ServerFrame) {
        self.seen.push_str(&serde_json::to_string(frame).unwrap());
        self.seen.push('\n');
    }

    async fn send(&mut self, command: Command) -> ResponseBody {
        let mut frame = self.connection.request(command).await.unwrap();
        loop {
            if let Some(frame) = &frame {
                self.saw(frame);
            }
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

    async fn set(&mut self, value: &str) {
        self.ok(Command::SetSecret {
            secret: NAME.into(),
            value: SecretValue::new(value),
        })
        .await;
    }

    async fn secrets(&mut self) -> Vec<SecretInfo> {
        match self.ok(Command::ListSecrets).await {
            Reply::Secrets { secrets } => secrets,
            other => panic!("expected Secrets, got {other:?}"),
        }
    }

    fn apply(&mut self, update: TopicUpdate) {
        match update {
            TopicUpdate::WatchedPrs { update, .. } => match update {
                WatchedPrsUpdate::Snapshot(snapshot) => self.prs = snapshot,
                WatchedPrsUpdate::Delta(delta) => self.prs.apply(delta),
            },
            TopicUpdate::Inbox { update, .. } => match update {
                InboxUpdate::Snapshot(snapshot) => self.inbox = snapshot,
                InboxUpdate::Delta(delta) => self.inbox.apply(delta),
            },
            TopicUpdate::Run { id, seq, event, .. } => {
                self.runs.entry(id).or_default().apply(seq, event);
            }
            TopicUpdate::StepLog { .. }
            | TopicUpdate::Notifications { .. }
            | TopicUpdate::Pipeline { .. } => {}
        }
    }

    async fn until(&mut self, what: &str, done: impl Fn(&Client) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            let frame = tokio::time::timeout_at(deadline, self.connection.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for {what}: {:#?} {:#?}",
                        self.inbox, self.runs
                    )
                })
                .unwrap();
            if let Some(frame) = &frame {
                self.saw(frame);
            }
            match frame {
                Some(ServerFrame::Topic(update)) => self.apply(update),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    async fn subscribe(&mut self, run: RunId) {
        self.ok(Command::Subscribe {
            topic: Topic::Run(run),
            since: None,
        })
        .await;
    }

    /// PR `number`'s Runs, newest first.
    fn history(&self, number: u64) -> Vec<RunId> {
        self.prs
            .pr(&repo(), number)
            .map(|pr| pr.runs.iter().map(|run| run.id).collect())
            .unwrap_or_default()
    }

    fn end(&self, run: RunId) -> Option<EndReason> {
        self.runs.get(&run).and_then(|view| view.end)
    }

    fn step(&self, run: RunId, step: &str) -> Option<&StepStatus> {
        let view = self.runs.get(&run)?;
        view.steps
            .iter()
            .find(|view| view.info.id == step)
            .map(|view| &view.status)
    }

    fn reason(&self, run: RunId, step: &str) -> Option<String> {
        match self.step(run, step)? {
            StepStatus::Settled { reason, .. } => reason.clone(),
            _ => None,
        }
    }

    /// Whether no frame so far carried either value.
    fn clean(&self) -> bool {
        !self.seen.contains(VALUE) && !self.seen.contains(ROTATED)
    }
}

async fn added(harness: &Harness) -> Client {
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client
}

/// Every file under `dir`, the database and Step logs included, read as
/// bytes, holds neither value.
fn no_value_on_disk(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            no_value_on_disk(&path);
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        for value in [VALUE, ROTATED] {
            assert!(
                !bytes.windows(value.len()).any(|w| w == value.as_bytes()),
                "{} holds a Secret value",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn a_missing_secret_holds_every_pr_in_one_entry_and_setting_it_starts_them_again() {
    let harness = Harness::new(&needs_secret(), &[1, 2], true);
    let mut client = added(&harness).await;
    let (first1, first2) = (client.history(1)[0], client.history(2)[0]);
    client.subscribe(first1).await;
    client.subscribe(first2).await;
    client
        .until("both Runs to end", |c| {
            c.end(first1).is_some() && c.end(first2).is_some()
        })
        .await;

    assert_eq!(client.end(first1), Some(EndReason::NotShippable));
    let reason = client.reason(first1, "check").unwrap();
    assert_eq!(reason, "error(secret missing): `TEST_SECRET` isn't set");
    assert_eq!(
        client.inbox.count(),
        1,
        "one shared entry, and no PR or Run entry the cause explains: {:#?}",
        client.inbox
    );
    let entry = client.inbox.entries[0].clone();
    assert_eq!(
        entry.scope,
        Scope::Cause {
            cause: Cause::MissingSecret { name: NAME.into() }
        }
    );
    assert_eq!(entry.title, "Secret `TEST_SECRET` isn't set");
    assert_eq!(entry.prs, [pr(1), pr(2)]);
    assert!(!entry.dismissable());

    client.set(VALUE).await;

    assert_eq!(client.inbox.count(), 0, "setting it cleared the entry");
    let (second1, second2) = (client.history(1)[0], client.history(2)[0]);
    assert_ne!(second1, first1, "PR 1 got a new Run");
    assert_ne!(second2, first2, "PR 2 got a new Run");
    client.subscribe(second1).await;
    client.subscribe(second2).await;
    client
        .until("both new Runs to end", |c| {
            c.end(second1).is_some() && c.end(second2).is_some()
        })
        .await;
    assert_eq!(client.end(second1), Some(EndReason::Shippable));
    assert_eq!(client.end(second2), Some(EndReason::Shippable));
    assert_eq!(
        client.runs[&second1].head_sha, client.runs[&first1].head_sha,
        "the same SHA"
    );
    assert!(
        harness.received(second1, "check", 1) == VALUE,
        "the Step got it"
    );
    let closed = client.runs[&first1]
        .inbox
        .iter()
        .find(|kept| kept.id == entry.id)
        .and_then(|kept| kept.closed.clone())
        .map(|closed| closed.how);
    assert_eq!(closed, Some(Closing::CauseCleared));
    assert!(client.clean());
    no_value_on_disk(harness.data.path());
}

#[tokio::test]
async fn a_secret_set_while_the_run_goes_on_reruns_the_step_in_place() {
    let harness = Harness::new(
        &pipeline(
            "  check: { uses: script, with: { act: pass } }\n  slow: { uses: plain }\n",
            "check, slow",
        ),
        &[1],
        true,
    );
    let mut client = added(&harness).await;
    let run = client.history(1)[0];
    client.subscribe(run).await;
    client
        .until("check to error", |c| {
            matches!(
                c.step(run, "check"),
                Some(StepStatus::Settled {
                    verdict: Verdict::Error,
                    ..
                })
            )
        })
        .await;
    assert_eq!(
        client.inbox.count(),
        1,
        "only the cause: {:#?}",
        client.inbox
    );

    client.set(VALUE).await;
    client
        .until("check to pass", |c| {
            matches!(
                c.step(run, "check"),
                Some(StepStatus::Settled {
                    verdict: Verdict::Pass,
                    ..
                })
            )
        })
        .await;
    harness.release(run, "slow", 1);
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    assert_eq!(client.end(run), Some(EndReason::Shippable));
    assert_eq!(client.history(1), [run], "no new Run");
    assert_eq!(client.inbox.count(), 0);
    assert!(client.clean());
}

#[tokio::test]
async fn rotating_takes_effect_at_the_next_spawn_and_the_list_shows_no_value() {
    let harness = Harness::new(&needs_secret(), &[1], true);
    let mut client = Client::connect(&harness.daemon).await;
    client.set(VALUE).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let first = client.history(1)[0];
    client.subscribe(first).await;
    client
        .until("the first Run to end", |c| c.end(first).is_some())
        .await;
    assert!(harness.received(first, "check", 1) == VALUE);

    client.set(&format!("  {ROTATED}\n")).await;
    let listed = client.secrets().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, NAME);
    assert!(listed[0].is_set());
    assert_eq!(listed[0].granted_to, ["script"]);

    harness.github.push(&repo(), 1);
    client.ok(Command::Refresh).await;
    let second = client.history(1)[0];
    assert_ne!(second, first);
    client.subscribe(second).await;
    client
        .until("the second Run to end", |c| c.end(second).is_some())
        .await;
    assert!(
        harness.received(second, "check", 1) == ROTATED,
        "the next spawn got the rotated value, trimmed"
    );
    assert!(client.clean());
    no_value_on_disk(harness.data.path());
}

#[tokio::test]
async fn a_steps_log_and_outcome_never_hold_a_value_it_received_even_when_it_echoes_it() {
    let harness = Harness::new(
        &pipeline(
            "  echo: { uses: script, with: { act: echo } }\n  junk: { uses: script, with: { act: junk } }\n",
            "echo, junk",
        ),
        &[1],
        true,
    );
    let mut client = Client::connect(&harness.daemon).await;
    client.set(VALUE).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history(1)[0];
    client.subscribe(run).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;
    assert!(
        harness.received(run, "echo", 1) == VALUE,
        "the Step got the value"
    );

    let Some(StepStatus::Settled {
        verdict, outputs, ..
    }) = client.step(run, "echo").cloned()
    else {
        panic!("echo settled");
    };
    assert_eq!(verdict, Verdict::Fail);
    assert_eq!(outputs.note.as_deref(), Some("note says ***"));
    assert_eq!(outputs.findings[0].message, "finding says ***");
    let junk = client.reason(run, "junk").unwrap();
    assert!(junk.starts_with("error(protocol)"), "{junk}");
    assert!(junk.contains("junk says ***"), "{junk}");

    let mut text = String::new();
    let mut before = None;
    loop {
        let page = match client
            .ok(Command::ReadStepLog {
                key: slopwatch_protocol::LogKey {
                    run,
                    step: "echo".into(),
                    attempt: 1,
                },
                page: slopwatch_protocol::LogPage {
                    before,
                    ..Default::default()
                },
                filter: slopwatch_protocol::LogFilter::default(),
            })
            .await
        {
            Reply::StepLog(page) => page,
            other => panic!("expected a log page, got {other:?}"),
        };
        for record in page.records.iter().rev() {
            text.insert_str(0, &format!("{}\n", record.text));
        }
        match (page.more_before, page.records.first()) {
            (true, Some(first)) => before = Some(first.seq),
            _ => break,
        }
    }
    for expected in ["stderr says ***", "log says ***", "*** tail"] {
        assert!(text.contains(expected), "the log reads {expected}");
    }
    assert!(
        !text.contains("sk-test-70"),
        "no part of the value is in the log"
    );
    assert!(client.clean());
    no_value_on_disk(harness.data.path());
}

#[tokio::test]
async fn a_secret_the_plugins_approval_doesnt_cover_errors_the_step_without_a_cause() {
    let harness = Harness::new(&needs_secret(), &[1], false);
    let mut client = Client::connect(&harness.daemon).await;
    client.set(VALUE).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history(1)[0];
    client.subscribe(run).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    assert_eq!(
        client.reason(run, "check").as_deref(),
        Some("error(secret ungranted): Plugin `script` has no Approval for `TEST_SECRET`")
    );
    assert_eq!(client.inbox.count(), 1);
    assert_eq!(client.inbox.entries[0].scope, Scope::Pr);
    assert!(
        !std::fs::exists(harness.control.join(format!("{run}-check.secret.1"))).unwrap(),
        "the Step never spawned"
    );
}

#[tokio::test]
async fn set_and_delete_refuse_bad_input_without_echoing_a_value() {
    let harness = Harness::new(&needs_secret(), &[], true);
    let mut client = Client::connect(&harness.daemon).await;

    let (code, _) = client
        .refused(Command::SetSecret {
            secret: "lower-case".into(),
            value: SecretValue::new(VALUE),
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid);
    let (code, _) = client
        .refused(Command::SetSecret {
            secret: NAME.into(),
            value: SecretValue::new("short"),
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid);
    let (code, _) = client
        .refused(Command::DeleteSecret {
            secret: NAME.into(),
        })
        .await;
    assert_eq!(code, ErrorCode::NotFound);

    // A malformed request whose value serde would quote back.
    client
        .connection
        .send_text(format!(
            r#"{{"type":"request","id":99,"actor":{{"kind":"developer","via":"gui"}},"command":{{"name":"set_secret","secret":7,"value":"{VALUE}"}}}}"#
        ))
        .await
        .unwrap();
    let frame = client.connection.recv().await.unwrap().unwrap();
    client.saw(&frame);
    assert!(matches!(frame, ServerFrame::Response(_)), "{frame:?}");

    client.set(VALUE).await;
    assert_eq!(client.secrets().await[0].name, NAME);
    client
        .ok(Command::DeleteSecret {
            secret: NAME.into(),
        })
        .await;
    assert!(!client.secrets().await[0].is_set(), "still listed, unset");
    assert_eq!(
        harness.keychain.reads(NAME),
        0,
        "set and delete never read it back"
    );
    assert!(client.clean());
}
