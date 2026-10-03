//! The Inbox: which Escalations open, what each belongs to, and what closes
//! them. The daemon runs over the in-process transport against a fake
//! GitHub, with a shell script Plugin each test drives through files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

mod support;

use serde_json::json;
use slopwatch_core::{EndReason, Workspace};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Manifest, STEP_DIALECT};
use slopwatch_protocol::{
    Actor, Cause, ClientFrame, ClientHello, Closing, Command, EntryId, ErrorCode, Inbox,
    InboxEntry, InboxUpdate, PrRef, Reply, RepoName, ResponseBody, RunId, RunView, Scope,
    ServerFrame, Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(20);

/// The `script` Plugin. Each attempt records how many times it ran in the
/// control dir under `<run>-<step>`, then acts on `with: { act: ... }`:
/// `pass` and `fail` report at once, and anything else waits until the
/// test writes `<run>-<step>.<attempt>`, then reports the Verdict it holds,
/// or exits without one for `crash`. The wait gives up once the control
/// dir is gone or after about two minutes, so no Step outlives its test.
const SCRIPT: &str = r#"
dir=$1
at="$dir/$SLOPWATCH_RUN-$SLOPWATCH_STEP"
read -r start
act=$(printf '%s' "$start" | sed -n 's/.*"act":"\([a-z]*\)".*/\1/p')
n=$(( $(cat "$at.attempts" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$at.attempts"
outcome() { printf '{"type":"outcome","verdict":"%s"}\n' "$1"; }
case $act in
  pass|fail) outcome "$act" ;;
  *)
    i=0
    until [ -f "$at.$n" ]; do
      [ -d "$dir" ] && [ "$i" -lt 2400 ] || exit 1
      sleep 0.05; i=$((i + 1))
    done
    verdict=$(cat "$at.$n")
    [ "$verdict" = crash ] && exit 3
    outcome "$verdict" ;;
esac
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

/// `held` waits for the test, and `aside` keeps the Run going meanwhile.
const HELD: &str = "version: 1
steps:
  held: { uses: script }
  aside: { uses: script }
gate: [held, aside]
";

const INVALID: &str = "version: 1
steps:
  review: { uses: nobody }
gate: [review]
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

/// Who the in-process client sends commands as.
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

struct Harness {
    /// First, so the Steps die before their control dir goes.
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    daemon: Arc<Daemon>,
    store: Store,
    control: PathBuf,
    data: tempfile::TempDir,
    _control: tempfile::TempDir,
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
        let control = tempfile::tempdir().unwrap();
        let store = Store::open(&data.path().join("state.db")).unwrap();
        let daemon = daemon(&github, &store, data.path(), control.path());
        Harness {
            _reaper: support::Reaper::new(control.path()),
            github,
            daemon,
            store,
            control: control.path().to_owned(),
            data,
            _control: control,
        }
    }

    /// A new daemon on the same data dir, as after a crash.
    fn restart(&mut self) {
        self.store = Store::open(&self.data.path().join("state.db")).unwrap();
        self.daemon = daemon(&self.github, &self.store, self.data.path(), &self.control);
    }

    /// Lets attempt `attempt` of `step` in `run` report `verdict`.
    fn release(&self, run: RunId, step: &str, attempt: u32, verdict: &str) {
        let file = self.control.join(format!("{run}-{step}.{attempt}"));
        std::fs::write(file, verdict).unwrap();
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

/// A client that keeps its own copy of `watched_prs`, the Inbox, and
/// every Run it subscribed to.
struct Client {
    connection: InProcessClient,
    prs: WatchedPrs,
    inbox: Inbox,
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
            inbox: Inbox::default(),
            runs: HashMap::new(),
        };
        for topic in [Topic::WatchedPrs, Topic::Inbox] {
            client.ok(Command::Subscribe { topic, since: None }).await;
        }
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
                .unwrap_or_else(|_| panic!("timed out waiting for {what}: {:#?}", self.inbox))
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

    /// The one open entry, which the test expects.
    fn only(&self) -> &InboxEntry {
        assert_eq!(self.inbox.count(), 1, "{:#?}", self.inbox);
        &self.inbox.entries[0]
    }

    /// How the entry `id` closed, as Run `run`'s record keeps it.
    fn closed_in(&self, run: RunId, id: EntryId) -> Option<Closing> {
        let view = self.runs.get(&run)?;
        let entry = view.inbox.iter().find(|entry| entry.id == id)?;
        entry.closed.as_ref().map(|closed| closed.how.clone())
    }
}

async fn added(harness: &Harness) -> Client {
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client
}

#[tokio::test]
async fn an_invalid_pipeline_raises_one_shared_entry_and_fixing_it_starts_every_pr() {
    let harness = Harness::new(INVALID, &[1, 2]);
    let mut client = added(&harness).await;

    let entry = client.only().clone();
    assert_eq!(
        entry.scope,
        Scope::Cause {
            cause: Cause::InvalidPipeline {
                repo: repo(),
                base: "main".into(),
            }
        }
    );
    assert_eq!(entry.title, "Invalid Pipeline on main");
    assert_eq!(entry.prs, [pr(1), pr(2)], "one entry holds back both PRs");
    assert!(entry.reasons[0].contains("nobody"), "{:?}", entry.reasons);
    assert!(!entry.dismissable());
    let (code, _) = client
        .refused(Command::DismissEntry { entry: entry.id })
        .await;
    assert_eq!(code, ErrorCode::Invalid, "a cause can't be dismissed");

    harness.github.set_pipeline(&repo(), "main", PASSES);
    client.ok(Command::Refresh).await;

    assert_eq!(client.inbox.count(), 0, "fixing the Pipeline cleared it");
    assert_eq!(client.history(1).len(), 1, "PR 1 started");
    assert_eq!(client.history(2).len(), 1, "PR 2 started");
    let entries = harness.store.run_entries(client.history(1)[0]).unwrap();
    assert!(entries.is_empty(), "the cause came before any Run");
}

#[tokio::test]
async fn a_pipeline_missing_a_library_step_clears_once_the_step_is_saved() {
    let harness = Harness::new(
        "version: 1\nsteps:\n  check: { uses: lib/later }\ngate: [check]\n",
        &[1],
    );
    let mut client = added(&harness).await;
    assert!(client.only().reasons[0].contains("later"));

    client
        .ok(Command::SaveLibraryStep {
            step: "later".into(),
            text: "uses: script\nwith: { act: pass }\n".into(),
        })
        .await;

    assert_eq!(client.inbox.count(), 0);
    assert_eq!(client.history(1).len(), 1);
}

#[tokio::test]
async fn a_not_shippable_run_raises_one_pr_entry_and_the_next_run_closes_it() {
    let harness = Harness::new(FAILS, &[1]);
    let mut client = added(&harness).await;
    let first = client.history(1)[0];
    client.subscribe(first).await;
    client
        .until("the Run to end with a PR entry", |c| {
            c.inbox.count() == 1 && c.end(first).is_some()
        })
        .await;

    let entry = client.only().clone();
    assert_eq!(client.end(first), Some(EndReason::NotShippable));
    assert_eq!(entry.scope, Scope::Pr);
    assert_eq!(entry.title, "Not shippable");
    assert_eq!(entry.reasons, ["`check`: fail"]);
    assert_eq!(entry.prs, [pr(1)]);
    assert_eq!(client.inbox.for_pr(&repo(), 1).count(), 1);

    harness.github.push(&repo(), 1);
    client.ok(Command::Refresh).await;
    let second = client.history(1)[0];
    assert_ne!(second, first);
    client
        .until("the entry to close", |c| {
            c.closed_in(first, entry.id).is_some()
        })
        .await;

    assert_eq!(
        client.closed_in(first, entry.id),
        Some(Closing::NextRunStarted),
        "the first Run's record keeps the entry and how it closed"
    );
    assert!(
        client.inbox.entries.iter().all(|open| open.id != entry.id),
        "{:#?}",
        client.inbox
    );
    let kept = harness.store.run_entries(first).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        kept[0].closed.as_ref().unwrap().how,
        Closing::NextRunStarted
    );
}

#[tokio::test]
async fn dismissing_a_pr_entry_closes_it() {
    let harness = Harness::new(FAILS, &[1]);
    let mut client = added(&harness).await;
    let run = client.history(1)[0];
    client.subscribe(run).await;
    client.until("the PR entry", |c| c.inbox.count() == 1).await;
    let id = client.only().id;

    client.ok(Command::DismissEntry { entry: id }).await;

    assert_eq!(client.inbox.count(), 0);
    client
        .until("the record to show it", |c| c.closed_in(run, id).is_some())
        .await;
    assert_eq!(
        client.closed_in(run, id),
        Some(Closing::Dismissed { actor: developer() })
    );
    assert_eq!(client.end(run), Some(EndReason::NotShippable), "unchanged");
    let (code, _) = client.refused(Command::DismissEntry { entry: id }).await;
    assert_eq!(code, ErrorCode::NotFound, "it's closed already");
}

#[tokio::test]
async fn a_step_error_mid_run_raises_a_run_entry_that_a_retry_answers() {
    let harness = Harness::new(HELD, &[1]);
    let mut client = added(&harness).await;
    let run = client.history(1)[0];
    client.subscribe(run).await;

    harness.release(run, "held", 1, "crash");
    client
        .until("the Run entry", |c| c.inbox.count() == 1)
        .await;
    let entry = client.only().clone();
    assert_eq!(
        entry.scope,
        Scope::Run {
            run,
            step: Some("held".into())
        }
    );
    assert_eq!(entry.title, "`held` errored");
    assert!(entry.reasons[0].starts_with("error(crash)"), "{entry:?}");
    let (code, _) = client
        .refused(Command::DismissEntry { entry: entry.id })
        .await;
    assert_eq!(code, ErrorCode::Invalid, "a Run entry can't be dismissed");

    client
        .ok(Command::RetryStep {
            run,
            step: "held".into(),
        })
        .await;

    assert_eq!(client.inbox.count(), 0);
    client
        .until("the record to show it", |c| {
            c.closed_in(run, entry.id).is_some()
        })
        .await;
    assert_eq!(
        client.closed_in(run, entry.id),
        Some(Closing::Answered {
            action: "retry `held`".into(),
            actor: developer(),
            note: None,
        })
    );

    harness.release(run, "held", 2, "pass");
    harness.release(run, "aside", 1, "pass");
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;
    assert_eq!(client.end(run), Some(EndReason::Shippable));
    assert_eq!(client.inbox.count(), 0, "a shippable Run raises nothing");
}

#[tokio::test]
async fn cancelling_a_run_closes_its_run_entries_and_raises_no_pr_entry() {
    let harness = Harness::new(HELD, &[1]);
    let mut client = added(&harness).await;
    let run = client.history(1)[0];
    client.subscribe(run).await;
    harness.release(run, "held", 1, "crash");
    client
        .until("the Run entry", |c| c.inbox.count() == 1)
        .await;
    let id = client.only().id;

    client.ok(Command::CancelRun { run }).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    assert_eq!(client.end(run), Some(EndReason::Cancelled));
    assert_eq!(client.inbox.count(), 0, "{:#?}", client.inbox);
    client
        .until("the record to show it", |c| c.closed_in(run, id).is_some())
        .await;
    assert_eq!(client.closed_in(run, id), Some(Closing::RunEnded));
    assert_eq!(harness.store.run_entries(run).unwrap().len(), 1);
}

#[tokio::test]
async fn a_run_that_ends_on_a_step_error_lists_it_on_the_pr_entry() {
    let harness = Harness::new(HELD, &[1]);
    let mut client = added(&harness).await;
    let run = client.history(1)[0];
    client.subscribe(run).await;
    harness.release(run, "held", 1, "crash");
    client
        .until("the Run entry", |c| c.inbox.count() == 1)
        .await;

    harness.release(run, "aside", 1, "pass");
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;
    client
        .until("the PR entry", |c| {
            c.inbox.count() == 1 && c.inbox.entries[0].scope == Scope::Pr
        })
        .await;

    let entry = client.only();
    assert_eq!(entry.reasons.len(), 1);
    assert!(
        entry.reasons[0].starts_with("`held`: error(crash)"),
        "{entry:?}"
    );
    let record: Vec<_> = harness
        .store
        .run_entries(run)
        .unwrap()
        .into_iter()
        .map(|entry| (entry.scope, entry.closed.map(|closed| closed.how)))
        .collect();
    assert_eq!(
        record,
        [
            (
                Scope::Run {
                    run,
                    step: Some("held".into())
                },
                Some(Closing::RunEnded)
            ),
            (Scope::Pr, None),
        ]
    );
}

#[tokio::test]
async fn unwatching_closes_the_prs_entries_and_takes_it_off_a_cause() {
    let harness = Harness::new(FAILS, &[1]);
    let mut client = added(&harness).await;
    let run = client.history(1)[0];
    client.subscribe(run).await;
    client.until("the PR entry", |c| c.inbox.count() == 1).await;
    let id = client.only().id;

    client
        .ok(Command::Unwatch {
            repo: repo(),
            number: 1,
        })
        .await;

    assert_eq!(client.inbox.count(), 0);
    client
        .until("the record to show it", |c| c.closed_in(run, id).is_some())
        .await;
    assert_eq!(client.closed_in(run, id), Some(Closing::LeftWatched));
}

#[tokio::test]
async fn a_pr_leaving_a_cause_shrinks_it_and_the_last_one_closes_it() {
    let harness = Harness::new(INVALID, &[1, 2]);
    let mut client = added(&harness).await;
    assert_eq!(client.only().prs, [pr(1), pr(2)]);

    client
        .ok(Command::Unwatch {
            repo: repo(),
            number: 1,
        })
        .await;
    assert_eq!(client.only().prs, [pr(2)]);

    harness.github.close_pr(&repo(), 2);
    client.ok(Command::Refresh).await;
    assert_eq!(client.inbox.count(), 0);
}

#[tokio::test]
async fn open_entries_survive_a_restart_without_doubling() {
    let mut harness = Harness::new(INVALID, &[1]);
    let client = added(&harness).await;
    let id = client.only().id;
    drop(client);

    harness.restart();
    let mut client = Client::connect(&harness.daemon).await;
    assert_eq!(client.only().id, id, "the store kept it");
    client.ok(Command::Refresh).await;

    assert_eq!(client.only().id, id, "the same entry, not a second one");
}

#[tokio::test]
async fn a_pipeline_broken_after_a_run_holds_the_pr_until_it_is_restored() {
    let mut harness = Harness::new(PASSES, &[1]);
    let mut client = added(&harness).await;
    let first = client.history(1)[0];
    client.subscribe(first).await;
    client
        .until("the Run to end", |c| c.end(first).is_some())
        .await;

    harness.github.set_pipeline(&repo(), "main", INVALID);
    client.ok(Command::Refresh).await;
    let id = client.only().id;
    let record = harness.store.run_entries(first).unwrap();
    assert_eq!(record.len(), 1, "the cause touches the PR's latest Run");

    // Restored while the daemon was down, which forgets the block.
    drop(client);
    harness.restart();
    harness.github.set_pipeline(&repo(), "main", PASSES);
    let mut client = Client::connect(&harness.daemon).await;
    assert_eq!(client.only().id, id);
    client.ok(Command::Refresh).await;

    assert_eq!(client.inbox.count(), 0, "{:#?}", client.inbox);
    let record = harness.store.run_entries(first).unwrap();
    assert_eq!(
        record[0].closed.as_ref().map(|closed| &closed.how),
        Some(&Closing::CauseCleared)
    );
}
