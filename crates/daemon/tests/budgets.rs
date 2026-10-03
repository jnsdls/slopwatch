//! Budgets: a Step past its own Budget stops, a PR past its Budget and a
//! spent day end Runs over budget and hold PRs on their entries, and
//! raising a Budget or running anyway once starts them again. The daemon
//! runs over the in-process transport against a fake GitHub, with shell
//! script Plugins that report usage.

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
    BudgetHit, BudgetKind, Cause, Cents, ClientFrame, ClientHello, Closing, Command,
    DaemonSettings, ErrorCode, Inbox, InboxEntry, InboxUpdate, PrRef, Reply, RepoName,
    ResponseBody, RunId, RunView, Scope, ServerFrame, StepStatus, Topic, TopicUpdate, WatchedPrs,
    WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(20);

/// The script behind every Plugin here. Each attempt keeps its `start`
/// line under `<run>-<step>.start.<attempt>` in the control dir, reports
/// a call that cost `with: { usd: ... }` or used `with: { tokens: ... }`
/// input tokens of `gpt-5.4`, then acts on `with: { act: ... }`: `pass`
/// reports at once, and `wait` waits until the test writes
/// `<run>-<step>.<attempt>` and reports the Verdict it holds. The wait
/// gives up once the control dir is gone or after about two minutes.
const SCRIPT: &str = r#"
dir=$1
at="$dir/$SLOPWATCH_RUN-$SLOPWATCH_STEP"
read -r start
field() { printf '%s' "$start" | sed -n "s/.*\"$1\":\"*\([a-z0-9.]*\)\"*.*/\1/p"; }
act=$(field act)
usd=$(field usd)
tokens=$(field tokens)
n=$(( $(cat "$at.attempts" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$at.attempts"
printf '%s' "$start" > "$at.start.$n"
[ -n "$usd" ] && printf '{"type":"usage","model":"m","input_tokens":1,"usd":%s}\n' "$usd"
[ -n "$tokens" ] && printf '{"type":"usage","model":"gpt-5.4","input_tokens":%s}\n' "$tokens"
outcome() { printf '{"type":"outcome","verdict":"%s"}\n' "$1"; }
case $act in
  pass) outcome pass ;;
  *)
    i=0
    until [ -f "$at.$n" ]; do
      [ -d "$dir" ] && [ "$i" -lt 2400 ] || exit 1
      sleep 0.05; i=$((i + 1))
    done
    outcome "$(cat "$at.$n")" ;;
esac
"#;

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

fn pr(number: u64) -> PrRef {
    PrRef {
        repo: repo(),
        number,
    }
}

/// `script` has no Budget of its own, like CI. `agent` may spend $0.50 a
/// Step, like an agent CLI.
fn manifest(id: &str, budget_usd: Option<f64>) -> Manifest {
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
        budget_usd,
        concurrency: None,
    }
}

struct Harness {
    /// First, so the Steps die before their control dir goes.
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    daemon: Arc<Daemon>,
    control: PathBuf,
    _data: tempfile::TempDir,
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
            control: control.path().to_owned(),
            _data: data,
            _control: control,
        }
    }

    /// Lets attempt `attempt` of `step` in `run` report `verdict`.
    fn release(&self, run: RunId, step: &str, attempt: u32, verdict: &str) {
        let file = self.control.join(format!("{run}-{step}.{attempt}"));
        std::fs::write(file, verdict).unwrap();
    }

    /// The `start` line attempt `attempt` of `step` in `run` read, once
    /// the Step has written it down.
    fn start(&self, run: RunId, step: &str, attempt: u32) -> serde_json::Value {
        let file = self.control.join(format!("{run}-{step}.start.{attempt}"));
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let read = std::fs::read_to_string(&file).ok();
            if let Some(start) = read.and_then(|text| serde_json::from_str(&text).ok()) {
                return start;
            }
            assert!(std::time::Instant::now() < deadline, "{step} never started");
            std::thread::sleep(Duration::from_millis(20));
        }
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
    let mut plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library));
    for manifest in [manifest("script", None), manifest("agent", Some(0.5))] {
        store
            .put_approval(&slopwatch_daemon::approvals::Approval::granting(
                &manifest,
                slopwatch_protocol::Actor::Developer { via: "test".into() },
                0,
            ))
            .unwrap();
        plugins = plugins.with_plugin(manifest, PathBuf::from("/bin/sh"), args.clone());
    }
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

    /// Step `step` of `run`, once settled, with its reason.
    fn settled(&self, run: RunId, step: &str) -> Option<(Verdict, Option<String>)> {
        match &self.runs.get(&run)?.step(step)?.status {
            StepStatus::Settled {
                verdict, reason, ..
            } => Some((*verdict, reason.clone())),
            _ => None,
        }
    }

    /// The one open entry, which the test expects.
    fn only(&self) -> &InboxEntry {
        assert_eq!(self.inbox.count(), 1, "{:#?}", self.inbox);
        &self.inbox.entries[0]
    }

    /// The newest Run of PR `number`, subscribed to.
    async fn latest(&mut self, number: u64) -> RunId {
        let run = self.history(number)[0];
        if !self.runs.contains_key(&run) {
            self.subscribe(run).await;
        }
        run
    }
}

async fn added(harness: &Harness) -> Client {
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    client
}

#[tokio::test]
async fn a_step_past_its_own_budget_is_stopped_and_the_run_goes_on() {
    let harness = Harness::new(
        "version: 1
steps:
  review: { uses: agent, with: { act: wait, usd: 0.6 } }
  tight: { uses: agent, budget_usd: 0.25, with: { act: wait } }
  ci: { uses: script, with: { act: wait } }
gate: [review, tight, ci]
",
        &[1],
    );
    let mut client = added(&harness).await;
    let run = client.latest(1).await;
    client
        .until("review to stop", |c| c.settled(run, "review").is_some())
        .await;

    assert_eq!(
        client.settled(run, "review"),
        Some((
            Verdict::Error,
            Some("error(budget): spent $0.60 of its $0.50 Budget".into())
        ))
    );
    assert_eq!(client.end(run), None, "the rest of the Run carries on");
    client
        .until("a Run entry for the stopped Step", |c| c.inbox.count() == 1)
        .await;
    assert_eq!(
        client.only().scope,
        Scope::Run {
            run,
            step: Some("review".into())
        }
    );
    assert_eq!(
        harness.start(run, "review", 1)["budget_usd"],
        json!(0.5),
        "the tightest Budget: its own, under the PR's $10 and the day's $25"
    );
    assert_eq!(
        harness.start(run, "tight", 1)["budget_usd"],
        json!(0.25),
        "the Pipeline's Budget for the Step wins over the manifest's"
    );

    harness.release(run, "tight", 1, "pass");
    harness.release(run, "ci", 1, "pass");
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;
    assert_eq!(client.end(run), Some(EndReason::NotShippable));
}

#[tokio::test]
async fn codex_tokens_are_priced_from_the_table_and_count_against_the_budget() {
    let harness = Harness::new(
        "version: 1
steps:
  review: { uses: agent, with: { act: wait, tokens: 400000 } }
gate: [review]
",
        &[1],
    );
    let mut client = added(&harness).await;
    let run = client.latest(1).await;
    client
        .until("the Run to end", |c| c.end(run).is_some())
        .await;

    // gpt-5.4 lists input at $2.50 a million tokens.
    assert_eq!(
        client.settled(run, "review"),
        Some((
            Verdict::Error,
            Some("error(budget): spent $1 of its $0.50 Budget".into())
        ))
    );
    assert_eq!(
        client.runs[&run].cost().unwrap().to_string(),
        "$1.00",
        "the Run's total counts the priced call"
    );
}

#[tokio::test]
async fn a_pr_past_its_budget_ends_the_run_over_budget_with_one_pr_entry() {
    const PIPELINE: &str = "version: 1
budget_usd: 1
steps:
  review: { uses: agent, budget_usd: 5, with: { act: wait, usd: 1.2 } }
  ci: { uses: script, with: { act: wait } }
  later: { uses: agent, needs: [ci], with: { act: pass } }
gate: [review, ci, later]
";
    let harness = Harness::new(PIPELINE, &[1]);
    let mut client = added(&harness).await;
    let first = client.latest(1).await;
    client
        .until("review to stop", |c| c.settled(first, "review").is_some())
        .await;

    assert_eq!(
        client.settled(first, "review"),
        Some((
            Verdict::Error,
            Some("error(budget): the PR's Budget is spent".into())
        ))
    );
    assert_eq!(client.end(first), None, "ci costs nothing and runs on");
    assert_eq!(client.inbox.count(), 0, "no entry until the Run ends");

    harness.release(first, "ci", 1, "pass");
    client
        .until("the Run to end with its entry", |c| {
            c.end(first).is_some() && c.inbox.count() == 1
        })
        .await;
    assert_eq!(client.end(first), Some(EndReason::OverBudget));
    assert_eq!(
        client.runs[&first].step("later").unwrap().status,
        StepStatus::Pending,
        "nothing new starts once the Budget is spent"
    );
    let entry = client.only().clone();
    assert_eq!(entry.scope, Scope::Pr);
    assert_eq!(entry.title, "Over budget");
    assert_eq!(entry.prs, [pr(1)]);
    assert_eq!(
        entry.budget,
        Some(BudgetHit {
            kind: BudgetKind::Pr,
            spent: Cents(120),
            budget: Cents(100),
        })
    );
    assert_eq!(
        entry.reasons[0],
        "Spent $1.20 of $1, the PR's Budget since its last outside push"
    );

    // A Pipeline change would start a same-SHA Run, but the Budget holds
    // the PR.
    harness
        .github
        .set_pipeline(&repo(), "main", &format!("{PIPELINE}# changed\n"));
    client.ok(Command::Refresh).await;
    assert_eq!(client.history(1), [first]);
    assert!(
        client
            .prs
            .pr(&repo(), 1)
            .unwrap()
            .blocked
            .as_deref()
            .unwrap()
            .starts_with("Over budget"),
        "{:?}",
        client.prs.pr(&repo(), 1)
    );
    assert_eq!(client.only().id, entry.id, "still the one entry");

    client.ok(Command::RunAnywayOnce { entry: entry.id }).await;
    assert_eq!(client.history(1).len(), 2, "run anyway started a Run");
    let second = client.latest(1).await;
    assert!(
        client.runs[&first]
            .inbox
            .iter()
            .any(|kept| kept.id == entry.id),
        "the first Run's record keeps the entry"
    );
    harness.release(second, "review", 1, "pass");
    client
        .until("the second Run to end", |c| c.end(second).is_some())
        .await;
    assert_eq!(
        client.end(second),
        Some(EndReason::Shippable),
        "the lifted Run spent past the PR's Budget and still finished"
    );
    assert_eq!(client.inbox.count(), 0);

    // Exactly one Run got through: the next one is held again.
    harness
        .github
        .set_pipeline(&repo(), "main", &format!("{PIPELINE}# changed again\n"));
    client.ok(Command::Refresh).await;
    assert_eq!(client.history(1).len(), 2);
    let held = client.only().clone();
    assert_eq!(held.title, "Over budget");
    assert_eq!(held.budget.unwrap().spent, Cents(240));

    let (code, _) = client
        .refused(Command::RaiseBudget {
            entry: held.id,
            to: Cents(200),
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid, "a raise has to clear the spend");
    client
        .ok(Command::RaiseBudget {
            entry: held.id,
            to: Cents(1000),
        })
        .await;
    assert_eq!(client.history(1).len(), 3, "raising it started a Run");
    let third = client.latest(1).await;
    client
        .until("the third Run to end", |c| c.end(third).is_some())
        .await;
    assert_eq!(
        client.end(third),
        Some(EndReason::Shippable),
        "it took the second Run's Outcomes on the same SHA"
    );
    assert_eq!(client.inbox.count(), 0);

    // An outside push starts a new window under the Pipeline's $1.
    harness.github.push(&repo(), 1);
    client.ok(Command::Refresh).await;
    let fourth = client.latest(1).await;
    assert_ne!(fourth, third, "the push started a Run");
    assert_eq!(harness.start(fourth, "review", 1)["budget_usd"], json!(1.0));
}

#[tokio::test]
async fn hitting_the_daily_budget_holds_every_pr_behind_one_entry_and_raising_it_restarts_them() {
    let harness = Harness::new(
        "version: 1
steps:
  review: { uses: agent, with: { act: wait, usd: 0.4 } }
gate: [review]
",
        &[1, 2],
    );
    let mut client = Client::connect(&harness.daemon).await;
    client
        .ok(Command::SetSettings {
            settings: DaemonSettings {
                daily_budget: Some(Cents(60)),
            },
        })
        .await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let one = client.latest(1).await;
    let two = client.latest(2).await;
    client
        .until("both Runs to end on the entry", |c| {
            c.end(one).is_some()
                && c.end(two).is_some()
                && c.inbox
                    .entries
                    .first()
                    .is_some_and(|entry| entry.prs.len() == 2)
        })
        .await;

    assert_eq!(client.end(one), Some(EndReason::OverBudget));
    assert_eq!(client.end(two), Some(EndReason::OverBudget));
    let entry = client.only().clone();
    assert_eq!(
        entry.scope,
        Scope::Cause {
            cause: Cause::DailyBudget
        }
    );
    assert_eq!(entry.title, "Daily Budget spent");
    assert_eq!(entry.prs, [pr(1), pr(2)], "one entry holds every PR");
    assert_eq!(entry.budget.unwrap().kind, BudgetKind::Daily);
    assert!(!entry.dismissable());

    // A PR watched now waits behind the same entry.
    harness.github.open_pr(&repo(), 3, "me", "PR 3");
    harness.github.label_on_github(&repo(), 3, true);
    client.ok(Command::Refresh).await;
    assert!(client.history(3).is_empty());
    assert_eq!(client.only().prs, [pr(1), pr(2), pr(3)]);

    let Reply::Settings {
        settings,
        spent_today,
    } = client.ok(Command::GetSettings).await
    else {
        panic!("expected settings");
    };
    assert_eq!(settings.daily_budget, Some(Cents(60)));
    assert_eq!(spent_today, Cents(80));

    client
        .ok(Command::RaiseBudget {
            entry: entry.id,
            to: Cents(500),
        })
        .await;

    assert_eq!(client.inbox.count(), 0);
    for number in [1, 2] {
        assert_eq!(client.history(number).len(), 2, "PR {number} started again");
    }
    assert_eq!(client.history(3).len(), 1, "PR 3 got its first Run");
    let Reply::Settings { settings, .. } = client.ok(Command::GetSettings).await else {
        panic!("expected settings");
    };
    assert_eq!(
        settings.daily_budget,
        Some(Cents(500)),
        "the raise is the new setting"
    );
    client.subscribe(one).await;
    let closed = client.runs[&one]
        .inbox
        .iter()
        .find(|kept| kept.id == entry.id)
        .and_then(|kept| kept.closed.clone())
        .map(|closed| closed.how);
    assert!(
        matches!(closed, Some(Closing::Answered { ref action, .. }) if action == "raise the Budget to $5"),
        "{closed:?}"
    );
}

#[tokio::test]
async fn turning_the_daily_budget_off_clears_its_entry() {
    let harness = Harness::new(
        "version: 1
steps:
  review: { uses: agent, with: { act: wait, usd: 0.4 } }
gate: [review]
",
        &[1],
    );
    let mut client = Client::connect(&harness.daemon).await;
    client
        .ok(Command::SetSettings {
            settings: DaemonSettings {
                daily_budget: Some(Cents(30)),
            },
        })
        .await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.latest(1).await;
    client
        .until("the daily entry", |c| {
            c.end(run).is_some() && c.inbox.count() == 1
        })
        .await;
    assert_eq!(client.end(run), Some(EndReason::OverBudget));
    assert_eq!(client.only().title, "Daily Budget spent");

    let (code, _) = client
        .refused(Command::SetSettings {
            settings: DaemonSettings {
                daily_budget: Some(Cents(0)),
            },
        })
        .await;
    assert_eq!(code, ErrorCode::Invalid);
    client
        .ok(Command::SetSettings {
            settings: DaemonSettings { daily_budget: None },
        })
        .await;

    assert_eq!(client.inbox.count(), 0);
    assert_eq!(client.history(1).len(), 2, "the PR started again");
}
