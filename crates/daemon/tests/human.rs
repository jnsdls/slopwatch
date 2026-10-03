//! The Human Step: the built-in `human` Plugin asks the developer through
//! the Inbox, and their answer passes or fails the Step. The daemon runs
//! over the in-process transport against a fake GitHub, with the real
//! `human` Plugin from the `slopwatchd` binary and a shell script Plugin
//! for the Steps around it.
//!
//! Each life of the daemon gets a tokio runtime of its own, and dropping
//! it drops every task at once, the way a SIGKILL would.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

mod support;

use serde_json::json;
use slopwatch_core::{EndReason, Verdict, Workspace};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Manifest, STEP_DIALECT};
use slopwatch_protocol::{
    Actor, Answer, ClientFrame, ClientHello, Closing, Command, EntryId, ErrorCode, Inbox,
    InboxEntry, InboxUpdate, Reply, RepoName, Request, RequestId, ResponseBody, RunId, RunView,
    Scope, ServerFrame, StepStatus, Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(20);

/// The `script` Plugin. It saves the `start` it got in the control dir as
/// `<run>-<step>.start`, then acts on `with: { act: ... }`: `pass` and
/// `fail` report at once, `ask` asks the developer as only `human` may, and
/// anything else waits until the test writes `<run>-<step>`, then reports
/// the Verdict it holds, or exits without one for `crash`. Every wait gives
/// up once the control dir is gone or after about two minutes, so no Step
/// outlives its test.
const SCRIPT: &str = r#"
dir=$1
at="$dir/$SLOPWATCH_RUN-$SLOPWATCH_STEP"
idle() {
  i=0
  until [ -n "$1" ] && [ -f "$1" ]; do
    [ -d "$dir" ] && [ "$i" -lt 2400 ] || exit 1
    sleep 0.05; i=$((i + 1))
  done
}
read -r start
printf '%s\n' "$start" > "$at.start"
act=$(printf '%s' "$start" | sed -n 's/.*"act":"\([a-z]*\)".*/\1/p')
outcome() { printf '{"type":"outcome","verdict":"%s"}\n' "$1"; }
case $act in
  pass|fail) outcome "$act" ;;
  ask) printf '{"type":"ask","prompt":"Me too?"}\n'; idle "" ;;
  *)
    idle "$at"
    verdict=$(cat "$at")
    [ "$verdict" = crash ] && exit 3
    outcome "$verdict" ;;
esac
"#;

/// A Human Step, and a Step after it that reads its Outcome.
const ASKS: &str = r#"version: 1
steps:
  sign-off: { uses: human, with: { prompt: "Ship it?" } }
  after: { uses: script, needs: [sign-off], with: { act: pass } }
gate: [sign-off, after]
"#;

/// A Human Step next to a Step the test can crash.
const ASKS_BESIDE: &str = r#"version: 1
steps:
  sign-off: { uses: human }
  held: { uses: script }
gate: [sign-off, held]
"#;

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

fn developer() -> Actor {
    Actor::Developer {
        via: "in_process".into(),
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
        budget_usd: None,
        concurrency: None,
    }
}

/// My labelled PR 1 in `jnsdls/app` with a Pipeline on main, and a data
/// dir that outlives each daemon.
struct Harness {
    /// First, so the Steps die before their control dir goes.
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    data: tempfile::TempDir,
    control: tempfile::TempDir,
}

impl Harness {
    fn new(pipeline: &str) -> Self {
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        github.open_pr(&repo(), 1, "me", "PR 1");
        github.label_on_github(&repo(), 1, true);
        github.set_pipeline(&repo(), "main", pipeline);
        let control = tempfile::tempdir().unwrap();
        Harness {
            _reaper: support::Reaper::new(control.path()),
            github,
            data: tempfile::tempdir().unwrap(),
            control,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.data.path().join("state.db")).unwrap()
    }

    /// Runs one life of the daemon: `life` drives it, and then the daemon
    /// dies with no warning.
    fn life<T>(&self, life: impl AsyncFnOnce(&mut Client) -> T) -> T {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let daemon = daemon(
                &self.github,
                &self.store(),
                self.data.path(),
                self.control(),
            );
            let mut client = Client::connect(&daemon).await;
            life(&mut client).await
        });
        drop(runtime);
        result
    }

    fn control(&self) -> &Path {
        self.control.path()
    }

    /// Lets the script Step `step` in `run` report `verdict`.
    fn release(&self, run: RunId, step: &str, verdict: &str) {
        std::fs::write(self.control().join(format!("{run}-{step}")), verdict).unwrap();
    }

    /// The `start` message the script Step `step` got in `run`.
    fn start_of(&self, run: RunId, step: &str) -> serde_json::Value {
        let text = std::fs::read_to_string(self.control().join(format!("{run}-{step}.start")))
            .unwrap_or_else(|error| panic!("`{step}` never started in Run {run}: {error}"));
        serde_json::from_str(&text).unwrap()
    }
}

fn daemon(github: &Arc<FakeGitHub>, store: &Store, data: &Path, control: &Path) -> Arc<Daemon> {
    let dyn_github: Arc<dyn GitHub> = Arc::clone(github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
    let library = Arc::new(Library::open(data.join("steps")).unwrap());
    let args = vec![
        "-c".to_owned(),
        SCRIPT.to_owned(),
        "script".to_owned(),
        control.to_str().unwrap().to_owned(),
    ];
    store
        .put_approval(&slopwatch_daemon::approvals::Approval::granting(
            &manifest(),
            slopwatch_protocol::Actor::Developer { via: "test".into() },
            0,
        ))
        .unwrap();
    let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library)).with_plugin(
        manifest(),
        PathBuf::from("/bin/sh"),
        args,
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
            keychain: Arc::new(MemoryKeychain::default()),
        },
    )
    .unwrap();
    Arc::new(Daemon::with_build_id("test", watching, library).with_runs(runs))
}

/// A client that keeps its own copy of `watched_prs`, the Inbox, and every
/// Run it subscribed to.
struct Client {
    connection: InProcessClient,
    prs: WatchedPrs,
    inbox: Inbox,
    runs: HashMap<RunId, RunView>,
    next_id: u64,
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
            next_id: 1000,
        };
        for topic in [Topic::WatchedPrs, Topic::Inbox] {
            client.ok(Command::Subscribe { topic, since: None }).await;
        }
        client
    }

    async fn send(&mut self, command: Command) -> ResponseBody {
        let frame = self.connection.request(command).await.unwrap();
        self.response(frame).await
    }

    /// Sends `command` as `actor`, the way another client would.
    async fn send_as(&mut self, actor: Actor, command: Command) -> ResponseBody {
        self.next_id += 1;
        let request = Request {
            id: RequestId(self.next_id),
            actor,
            command,
        };
        self.connection
            .send(&ClientFrame::Request(request))
            .await
            .unwrap();
        let frame = self.connection.recv().await.unwrap();
        self.response(frame).await
    }

    async fn response(&mut self, mut frame: Option<ServerFrame>) -> ResponseBody {
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

    /// PR 1's Runs, newest first.
    fn history(&self) -> Vec<RunId> {
        self.prs
            .pr(&repo(), 1)
            .map(|pr| pr.runs.iter().map(|run| run.id).collect())
            .unwrap_or_default()
    }

    fn end(&self, run: RunId) -> Option<EndReason> {
        self.runs.get(&run).and_then(|view| view.end)
    }

    fn status(&self, run: RunId, step: &str) -> Option<StepStatus> {
        Some(self.runs.get(&run)?.step(step)?.status.clone())
    }

    /// The open Human Step for `step` in `run`.
    fn asking(&self, run: RunId, step: &str) -> Option<&InboxEntry> {
        self.inbox.entries.iter().find(|entry| {
            entry.scope
                == Scope::Human {
                    run,
                    step: step.to_owned(),
                }
        })
    }

    /// How the entry `id` closed, as Run `run`'s record keeps it.
    fn closed_in(&self, run: RunId, id: EntryId) -> Option<Closing> {
        let view = self.runs.get(&run)?;
        let entry = view.inbox.iter().find(|entry| entry.id == id)?;
        entry.closed.as_ref().map(|closed| closed.how.clone())
    }

    /// Adds the repo and waits for PR 1's first Run to ask `step`.
    async fn first_ask(&mut self, step: &str) -> (RunId, InboxEntry) {
        self.ok(Command::AddRepo { repo: repo() }).await;
        let run = self.history()[0];
        self.subscribe(run).await;
        self.until("the Human Step", |c| c.asking(run, step).is_some())
            .await;
        (run, self.asking(run, step).unwrap().clone())
    }
}

fn answer(run: RunId, answer: Answer, note: Option<&str>) -> Command {
    Command::AnswerStep {
        run,
        step: "sign-off".into(),
        answer,
        note: note.map(str::to_owned),
    }
}

#[test]
fn approving_with_a_note_passes_the_step_and_the_next_step_reads_the_note() {
    let harness = Harness::new(ASKS);
    harness.life(async |client| {
        let (run, entry) = client.first_ask("sign-off").await;
        assert_eq!(entry.title, "Ship it?", "the prompt");
        assert_eq!(entry.prs.len(), 1);
        assert!(!entry.dismissable());
        let (code, _) = client
            .refused(Command::DismissEntry { entry: entry.id })
            .await;
        assert_eq!(code, ErrorCode::Invalid, "a Human Step can't be dismissed");
        assert_eq!(
            client.status(run, "after"),
            Some(StepStatus::Pending),
            "the Step after it waits"
        );

        client
            .ok(answer(run, Answer::Approve, Some(" rename the flag ")))
            .await;

        client
            .until("the answer to close it", |c| c.inbox.count() == 0)
            .await;
        client
            .until("the Run to end", |c| c.end(run).is_some())
            .await;
        assert_eq!(client.end(run), Some(EndReason::Shippable));
        let Some(StepStatus::Settled {
            verdict, outputs, ..
        }) = client.status(run, "sign-off")
        else {
            panic!("sign-off settled");
        };
        assert_eq!(verdict, Verdict::Pass);
        assert_eq!(outputs.note.as_deref(), Some("rename the flag"));
        assert_eq!(
            client.closed_in(run, entry.id),
            Some(Closing::Answered {
                action: "approve".into(),
                actor: developer(),
                note: Some("rename the flag".into()),
            }),
            "the Run's record keeps the answer"
        );
        run
    });

    let run = harness.store().latest_run_id(&repo(), 1).unwrap().unwrap();
    let start = harness.start_of(run, "after");
    assert_eq!(start["upstream"]["sign-off"]["verdict"], "pass");
    assert_eq!(
        start["upstream"]["sign-off"]["outputs"]["note"],
        "rename the flag"
    );
}

#[test]
fn rejecting_fails_the_step_and_raises_no_escalation() {
    let harness = Harness::new(ASKS);
    let run = harness.life(async |client| {
        let (run, entry) = client.first_ask("sign-off").await;

        client.ok(answer(run, Answer::Reject, None)).await;
        client
            .until("the Run to end", |c| c.end(run).is_some())
            .await;
        // A poll takes the engine's turn after the Run's end, so anything
        // its end raised is in by now.
        client.ok(Command::Refresh).await;

        assert_eq!(client.end(run), Some(EndReason::NotShippable));
        assert!(matches!(
            client.status(run, "sign-off"),
            Some(StepStatus::Settled {
                verdict: Verdict::Fail,
                ..
            })
        ));
        assert!(matches!(
            client.status(run, "after"),
            Some(StepStatus::Settled {
                verdict: Verdict::Skipped,
                ..
            })
        ));
        assert_eq!(client.inbox.count(), 0, "{:#?}", client.inbox);
        assert_eq!(
            client.closed_in(run, entry.id),
            Some(Closing::Answered {
                action: "reject".into(),
                actor: developer(),
                note: None,
            })
        );
        run
    });

    let record = harness.store().run_entries(run).unwrap();
    assert_eq!(record.len(), 1, "only the Human Step: {record:?}");
}

#[test]
fn a_push_while_the_step_waits_ends_the_run_and_the_new_run_asks_again() {
    let harness = Harness::new(ASKS);
    harness.life(async |client| {
        let (first, entry) = client.first_ask("sign-off").await;

        harness.github.push(&repo(), 1);
        client.ok(Command::Refresh).await;
        client
            .until("the first Run to end", |c| c.end(first).is_some())
            .await;
        assert_eq!(client.end(first), Some(EndReason::Superseded));
        client
            .until("the record to show it closed", |c| {
                c.closed_in(first, entry.id).is_some()
            })
            .await;
        assert_eq!(client.closed_in(first, entry.id), Some(Closing::RunEnded));

        let second = client.history()[0];
        assert_ne!(second, first);
        client
            .until("the new Run to ask", |c| {
                c.asking(second, "sign-off").is_some()
            })
            .await;
        assert_eq!(client.inbox.count(), 1, "{:#?}", client.inbox);
        let (code, _) = client.refused(answer(first, Answer::Approve, None)).await;
        assert_eq!(code, ErrorCode::Invalid, "the first Run has ended");
    });
}

#[test]
fn any_client_can_answer_and_the_record_names_who_did() {
    let harness = Harness::new(ASKS);
    harness.life(async |client| {
        let (run, entry) = client.first_ask("sign-off").await;
        let cli = Actor::Developer { via: "cli".into() };

        let response = client
            .send_as(cli.clone(), answer(run, Answer::Approve, Some("lgtm")))
            .await;
        assert!(
            matches!(response, ResponseBody::Ok(Reply::Done)),
            "{response:?}"
        );

        client
            .until("the record to show it", |c| {
                c.closed_in(run, entry.id).is_some()
            })
            .await;
        assert_eq!(
            client.closed_in(run, entry.id),
            Some(Closing::Answered {
                action: "approve".into(),
                actor: cli,
                note: Some("lgtm".into()),
            })
        );
        let (code, message) = client.refused(answer(run, Answer::Reject, None)).await;
        assert!(
            code == ErrorCode::Invalid,
            "a second answer is refused: {message}"
        );
        let (code, _) = client
            .refused(Command::AnswerStep {
                run,
                step: "nobody".into(),
                answer: Answer::Approve,
                note: None,
            })
            .await;
        assert_eq!(code, ErrorCode::NotFound);
    });
}

#[test]
fn a_human_step_sits_in_the_inbox_with_escalations_oldest_first() {
    let harness = Harness::new(ASKS_BESIDE);
    harness.life(async |client| {
        let (run, asked) = client.first_ask("sign-off").await;

        harness.release(run, "held", "crash");
        client
            .until("the Run entry", |c| c.inbox.count() == 2)
            .await;

        let scopes: Vec<&Scope> = client.inbox.entries.iter().map(|e| &e.scope).collect();
        assert_eq!(
            scopes,
            [
                &Scope::Human {
                    run,
                    step: "sign-off".into()
                },
                &Scope::Run {
                    run,
                    step: Some("held".into())
                },
            ]
        );
        assert_eq!(client.inbox.entries[0].id, asked.id);
        assert!(client.inbox.entries[0].raised_at <= client.inbox.entries[1].raised_at);
        let (code, _) = client
            .refused(Command::AnswerStep {
                run,
                step: "held".into(),
                answer: Answer::Approve,
                note: None,
            })
            .await;
        assert_eq!(
            code,
            ErrorCode::Invalid,
            "only a Step that asks takes an answer"
        );
    });
}

#[test]
fn waiting_human_steps_leave_the_step_slots_to_others() {
    let steps: String = (1..=9)
        .map(|n| format!("  ask-{n}: {{ uses: human }}\n"))
        .collect();
    let gate: Vec<String> = (1..=9).map(|n| format!("ask-{n}")).collect();
    let pipeline = format!(
        "version: 1\nsteps:\n{steps}  check: {{ uses: script, with: {{ act: pass }} }}\ngate: [{}, check]\n",
        gate.join(", ")
    );
    let harness = Harness::new(&pipeline);
    harness.life(async |client| {
        client.ok(Command::AddRepo { repo: repo() }).await;
        let run = client.history()[0];
        client.subscribe(run).await;

        client
            .until("every Human Step to ask and check to pass", |c| {
                c.inbox.count() == 9
                    && matches!(
                        c.status(run, "check"),
                        Some(StepStatus::Settled {
                            verdict: Verdict::Pass,
                            ..
                        })
                    )
            })
            .await;
    });
}

#[test]
fn a_waiting_step_keeps_its_entry_across_restarts_and_still_takes_an_answer() {
    let harness = Harness::new(ASKS);
    let (run, entry) = harness.life(async |client| client.first_ask("sign-off").await);

    // Two restarts in a row would end a Step in error(daemon_restart), but
    // one that asked only waits.
    for _ in 0..2 {
        harness.life(async |client| {
            // Nothing starts again before the first poll.
            client.subscribe(run).await;
            let before = client.runs[&run].step("sign-off").unwrap().attempt;
            client.ok(Command::Refresh).await;
            client
                .until("sign-off to start again and ask", |c| {
                    let step = c.runs[&run].step("sign-off").unwrap();
                    step.attempt > before && step.progress.as_deref() == Some("waiting for you")
                })
                .await;
            assert_eq!(client.inbox.count(), 1);
            assert_eq!(client.inbox.entries[0].id, entry.id, "the same entry");
        });
    }

    harness.life(async |client| {
        client.ok(Command::Refresh).await;
        client.subscribe(run).await;
        // It asks again once it has started again.
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            match client.send(answer(run, Answer::Approve, None)).await {
                ResponseBody::Ok(_) => break,
                ResponseBody::Error(error) => {
                    assert!(tokio::time::Instant::now() < deadline, "{error:?}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        client
            .until("the Run to end", |c| c.end(run).is_some())
            .await;
        assert_eq!(client.end(run), Some(EndReason::Shippable));
        assert_eq!(client.inbox.count(), 0);
    });
}

#[test]
fn only_the_human_plugin_may_ask() {
    let harness = Harness::new(
        "version: 1\nsteps:\n  rogue: { uses: script, with: { act: ask } }\ngate: [rogue]\n",
    );
    harness.life(async |client| {
        client.ok(Command::AddRepo { repo: repo() }).await;
        let run = client.history()[0];
        client.subscribe(run).await;
        client
            .until("the Run to end", |c| c.end(run).is_some())
            .await;

        let Some(StepStatus::Settled {
            verdict, reason, ..
        }) = client.status(run, "rogue")
        else {
            panic!("rogue settled");
        };
        assert_eq!(verdict, Verdict::Error);
        assert!(
            reason
                .as_deref()
                .unwrap_or_default()
                .starts_with("error(protocol)"),
            "{reason:?}"
        );
        assert!(
            client
                .inbox
                .entries
                .iter()
                .all(|entry| !matches!(entry.scope, Scope::Human { .. })),
            "{:#?}",
            client.inbox
        );
    });
}
