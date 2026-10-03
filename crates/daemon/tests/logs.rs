//! Step logs and retention end to end: a fake GitHub, the daemon over the
//! in-process transport, and the real `ci` Step, which writes a stderr line
//! and a progress line every time the checks change.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use slopwatch_core::{EndReason, Verdict};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::transport::in_process::InProcessClient;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Check, CheckState, Checks, ChecksState};
use slopwatch_protocol::{
    ClientFrame, ClientHello, Command, ErrorCode, LogFilter, LogKey, LogPage, LogRecord, LogSource,
    PullRequest, Reply, RepoName, ResponseBody, RunEvent, RunId, RunView, ServerFrame, StepLogPage,
    StepStatus, StorageWarning, Topic, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

const WAIT: Duration = Duration::from_secs(20);
const DAY: i64 = 24 * 60 * 60;

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

struct Harness {
    github: Arc<FakeGitHub>,
    runs: Arc<Runs>,
    daemon: Arc<Daemon>,
    data: tempfile::TempDir,
}

/// My PR 1 in `jnsdls/app`, labelled, with the CI-only Pipeline on main.
fn harness(retention: Retention) -> Harness {
    let github = Arc::new(FakeGitHub::new("me"));
    github.add_repo(&repo());
    github.open_pr(&repo(), 1, "me", "Add the thing");
    github.label_on_github(&repo(), 1, true);
    github.add_pipeline(&repo(), "main");
    let store = Store::in_memory();
    let data = tempfile::tempdir().unwrap();
    let dyn_github: Arc<dyn GitHub> = Arc::clone(&github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&dyn_github)).unwrap());
    let library = Arc::new(Library::open(data.path().join("steps")).unwrap());
    let runs = Runs::start(
        store,
        dyn_github,
        Arc::clone(&watching),
        RunsConfig {
            data_dir: data.path().to_owned(),
            plugins: Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library)),
            login_path: None,
            retention,
        },
    )
    .unwrap();
    let daemon =
        Arc::new(Daemon::with_build_id("test", watching, library).with_runs(Arc::clone(&runs)));
    Harness {
        github,
        runs,
        daemon,
        data,
    }
}

/// `n` pending checks, so each `n` is a new snapshot for `ci`.
fn pending(n: usize) -> Checks {
    Checks {
        state: ChecksState::Pending,
        runs: (0..n)
            .map(|i| Check {
                name: format!("check {i}"),
                state: CheckState::Pending,
                url: None,
            })
            .collect(),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn ci_log(run: RunId, attempt: u32) -> LogKey {
    LogKey {
        run,
        step: "ci".into(),
        attempt,
    }
}

/// A client that keeps what it hears: `watched_prs`, the Runs it
/// subscribed to with each event's timestamp, and Step log records.
struct Client {
    connection: InProcessClient,
    prs: WatchedPrs,
    runs: HashMap<RunId, RunView>,
    events: Vec<(RunId, i64, RunEvent)>,
    logs: HashMap<LogKey, Vec<LogRecord>>,
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
            logs: HashMap::new(),
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
            TopicUpdate::Run { id, seq, ts, event } => {
                self.events.push((id, ts, event.clone()));
                self.runs.entry(id).or_default().apply(seq, event);
            }
            TopicUpdate::StepLog { key, records } => {
                let log = self.logs.entry(key).or_default();
                for record in records {
                    assert!(
                        log.last().is_none_or(|last| last.seq < record.seq),
                        "records arrive in order, once each"
                    );
                    log.push(record);
                }
            }
        }
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

    fn log_texts(&self, key: &LogKey) -> Vec<String> {
        self.logs
            .get(key)
            .map(|records| records.iter().map(|record| record.text.clone()).collect())
            .unwrap_or_default()
    }

    async fn subscribe(&mut self, topic: Topic) {
        self.ok(Command::Subscribe { topic, since: None }).await;
    }

    async fn set_checks(&mut self, github: &FakeGitHub, checks: Checks) {
        github.set_checks(&repo(), 1, checks);
        self.ok(Command::Refresh).await;
    }

    async fn read(&mut self, key: LogKey, page: LogPage, filter: LogFilter) -> StepLogPage {
        match self.ok(Command::ReadStepLog { key, page, filter }).await {
            Reply::StepLog(page) => page,
            other => panic!("expected a log page, got {other:?}"),
        }
    }
}

async fn first_run(harness: &Harness) -> (Client, RunId) {
    let mut client = Client::connect(&harness.daemon).await;
    client.ok(Command::AddRepo { repo: repo() }).await;
    let run = client.history()[0];
    client.subscribe(Topic::Run(run)).await;
    (client, run)
}

fn ended(client: &Client, run: RunId) -> bool {
    client.runs.get(&run).is_some_and(|view| view.end.is_some())
}

fn texts(page: &StepLogPage) -> Vec<&str> {
    page.records
        .iter()
        .map(|record| record.text.as_str())
        .collect()
}

#[tokio::test]
async fn a_running_steps_log_streams_to_the_pr_pane_as_its_written() {
    let harness = harness(Retention::default());
    let (mut client, run) = first_run(&harness).await;
    let key = ci_log(run, 1);
    client
        .until("ci to start", |c| {
            c.runs[&run]
                .step("ci")
                .is_some_and(|step| step.attempt == 1)
        })
        .await;

    client.subscribe(Topic::StepLog(key.clone())).await;
    client
        .until("the first line", |c| !c.log_texts(&key).is_empty())
        .await;
    assert_eq!(client.log_texts(&key), ["ci: Waiting for 0 of 0 checks"]);
    assert_eq!(client.logs[&key][0].source, LogSource::Stderr);

    client.set_checks(&harness.github, pending(2)).await;
    client
        .until("the next line, live", |c| c.log_texts(&key).len() == 2)
        .await;
    assert_eq!(client.log_texts(&key)[1], "ci: Waiting for 2 of 2 checks");

    // Progress goes to the Run's journal, timestamped, to interleave.
    client
        .until("the progress event", |c| {
            c.runs[&run].step("ci").unwrap().progress.as_deref()
                == Some("Waiting for 2 of 2 checks")
        })
        .await;
    let line_at = client.logs[&key][1].ts;
    let progress_at = client
        .events
        .iter()
        .rev()
        .find_map(|(_, ts, event)| matches!(event, RunEvent::StepProgress { .. }).then_some(*ts))
        .unwrap();
    assert!(
        (line_at - progress_at).abs() < 5_000,
        "{line_at} vs {progress_at}"
    );

    // A client that comes back asks from the last record it has.
    let mut again = Client::connect(&harness.daemon).await;
    again
        .ok(Command::Subscribe {
            topic: Topic::StepLog(key.clone()),
            since: Some(1),
        })
        .await;
    assert_eq!(again.log_texts(&key), ["ci: Waiting for 2 of 2 checks"]);
}

#[tokio::test]
async fn search_filters_and_paging_work_on_a_log_larger_than_one_page() {
    let harness = harness(Retention::default());
    let (mut client, run) = first_run(&harness).await;
    let key = ci_log(run, 1);
    for n in 1..=12 {
        client.set_checks(&harness.github, pending(n)).await;
    }
    client
        .until("ci to have heard every update", |c| {
            c.runs[&run].step("ci").unwrap().progress.as_deref()
                == Some("Waiting for 12 of 12 checks")
        })
        .await;
    let page = |limit| LogPage {
        limit: Some(limit),
        ..LogPage::default()
    };

    let last = client
        .read(key.clone(), page(5), LogFilter::default())
        .await;
    assert_eq!(last.records.len(), 5);
    assert_eq!(texts(&last)[4], "ci: Waiting for 12 of 12 checks");
    assert!(last.more_before && !last.more_after);

    let older = client
        .read(
            key.clone(),
            LogPage {
                before: Some(last.records[0].seq),
                limit: Some(5),
                after: None,
            },
            LogFilter::default(),
        )
        .await;
    assert_eq!(texts(&older)[4], "ci: Waiting for 7 of 7 checks");

    let search = LogFilter {
        search: Some("FOR 1".into()),
        ..LogFilter::default()
    };
    let found = client.read(key.clone(), page(2), search.clone()).await;
    assert_eq!(
        texts(&found),
        [
            "ci: Waiting for 11 of 11 checks",
            "ci: Waiting for 12 of 12 checks"
        ]
    );
    assert!(found.more_before, "`for 1` and `for 10` match too");
    let earlier = client
        .read(
            key.clone(),
            LogPage {
                before: Some(found.records[0].seq),
                limit: Some(2),
                after: None,
            },
            search,
        )
        .await;
    assert_eq!(
        texts(&earlier),
        [
            "ci: Waiting for 1 of 1 checks",
            "ci: Waiting for 10 of 10 checks"
        ]
    );
    assert!(!earlier.more_before && earlier.more_after);

    let log_only = LogFilter {
        sources: vec![LogSource::Log],
        ..LogFilter::default()
    };
    let none = client.read(key.clone(), page(5), log_only).await;
    assert!(none.records.is_empty(), "ci writes only to stderr");
}

#[tokio::test]
async fn a_log_of_a_step_the_run_doesnt_have_is_not_found() {
    let harness = harness(Retention::default());
    let (mut client, run) = first_run(&harness).await;

    for step in ["nope", "../../etc"] {
        let key = LogKey {
            run,
            step: step.into(),
            attempt: 1,
        };
        let ResponseBody::Error(error) = client
            .send(Command::ReadStepLog {
                key,
                page: LogPage::default(),
                filter: LogFilter::default(),
            })
            .await
        else {
            panic!("expected an error");
        };
        assert_eq!(error.code, ErrorCode::NotFound);
    }
}

#[tokio::test]
async fn detail_older_than_14_days_is_pruned_and_the_record_stays() {
    let harness = harness(Retention::default());
    let (mut client, first) = first_run(&harness).await;
    client.set_checks(&harness.github, pending(1)).await;
    harness.github.push(&repo(), 1);
    client.ok(Command::Refresh).await;
    client
        .until("the first Run to end", |c| ended(c, first))
        .await;
    let second = client.history()[0];
    assert_ne!(second, first);
    let first_logs = harness.data.path().join("logs").join(first.to_string());
    assert!(first_logs.exists());

    // 13 days on, nothing goes.
    harness.runs.prune(now() + 13 * DAY).await.unwrap();
    assert!(first_logs.exists());

    harness.runs.prune(now() + 15 * DAY).await.unwrap();

    assert!(!first_logs.exists(), "the Step logs are gone");
    let ResponseBody::Error(error) = client
        .send(Command::ReadStepLog {
            key: ci_log(first, 1),
            page: LogPage::default(),
            filter: LogFilter::default(),
        })
        .await
    else {
        panic!("a pruned log can't be read");
    };
    assert!(error.message.contains("pruned"), "{error:?}");
    client
        .until("the pane to hear about it", |c| {
            c.runs[&first].pruned_at.is_some()
        })
        .await;

    // A client starting from scratch gets the record.
    let mut fresh = Client::connect(&harness.daemon).await;
    fresh.subscribe(Topic::Run(first)).await;
    let view = &fresh.runs[&first];
    assert_eq!(view.end, Some(EndReason::Superseded));
    assert!(view.pruned_at.is_some());
    assert!(matches!(
        view.step("ci").unwrap().status,
        StepStatus::Settled {
            verdict: Verdict::Cancelled,
            ..
        }
    ));
    let progress = fresh
        .events
        .iter()
        .filter(|(_, _, event)| matches!(event, RunEvent::StepProgress { .. }))
        .count();
    assert_eq!(progress, 0, "the rest of the journal is detail");
    assert_eq!(fresh.history(), [second, first], "Run history stays");

    // The latest Run of the open Watched PR keeps its detail.
    let deadline = tokio::time::Instant::now() + WAIT;
    while client
        .read(ci_log(second, 1), LogPage::default(), LogFilter::default())
        .await
        .records
        .is_empty()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the latest log stays"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn detail_past_the_cap_is_pruned_and_protected_detail_raises_the_warning() {
    let harness = harness(Retention {
        cap_bytes: 1,
        ..Retention::default()
    });
    let (mut client, first) = first_run(&harness).await;
    harness.github.push(&repo(), 1);
    client.ok(Command::Refresh).await;
    client
        .until("the first Run to end", |c| ended(c, first))
        .await;

    harness.runs.prune(now()).await.unwrap();

    client
        .until("the first Run to be pruned", |c| {
            c.runs[&first].pruned_at.is_some()
        })
        .await;
    client
        .until("the storage warning", |c| c.prs.storage.is_some())
        .await;
    let StorageWarning {
        used_bytes,
        cap_bytes,
    } = client.prs.storage.unwrap();
    assert_eq!(cap_bytes, 1);
    assert!(used_bytes > 1, "the running Run's detail stays");
}

#[tokio::test]
async fn unwatching_and_rewatching_a_pr_keeps_its_run_history() {
    let harness = harness(Retention::default());
    let (mut client, first) = first_run(&harness).await;

    client
        .ok(Command::Unwatch {
            repo: repo(),
            number: 1,
        })
        .await;
    client.until("the Run to end", |c| ended(c, first)).await;
    // Unwatched, its latest Run is no longer protected.
    harness.runs.prune(now() + 15 * DAY).await.unwrap();
    client
        .ok(Command::Watch {
            repo: repo(),
            number: 1,
        })
        .await;

    let history = client.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1], first);
    assert_eq!(client.pr().runs[1].end, Some(EndReason::Cancelled));
    let mut fresh = Client::connect(&harness.daemon).await;
    fresh.subscribe(Topic::Run(first)).await;
    assert_eq!(fresh.runs[&first].end, Some(EndReason::Cancelled));
    assert!(fresh.runs[&first].pruned_at.is_some());
}

#[tokio::test]
async fn a_run_whose_outcome_is_reused_keeps_its_log() {
    let harness = harness(Retention::default());
    let (mut client, first) = first_run(&harness).await;
    client
        .set_checks(
            &harness.github,
            Checks {
                state: ChecksState::Success,
                runs: vec![Check {
                    name: "test".into(),
                    state: CheckState::Success,
                    url: None,
                }],
            },
        )
        .await;
    client
        .until("the first Run to end", |c| ended(c, first))
        .await;

    // A Pipeline change starts a same-SHA Run that reuses ci's Outcome.
    harness.github.set_pipeline(
        &repo(),
        "main",
        "version: 1\nsteps:\n  ci: { uses: ci }\n  more: { uses: ci }\ngate: [ci, more]\n",
    );
    client.ok(Command::Refresh).await;
    let second = client.history()[0];
    assert_ne!(second, first);
    client.subscribe(Topic::Run(second)).await;
    let reused_from = match &client.runs[&second].step("ci").unwrap().status {
        StepStatus::Settled { reused_from, .. } => *reused_from,
        other => panic!("ci is reused, not {other:?}"),
    };
    assert_eq!(reused_from, Some(first));

    harness.runs.prune(now() + 15 * DAY).await.unwrap();

    // Attempt 0 reads the Step's latest attempt in that Run.
    let page = client
        .read(ci_log(first, 0), LogPage::default(), LogFilter::default())
        .await;
    assert_eq!(page.key, ci_log(first, 0));
    assert!(
        !page.records.is_empty(),
        "the reused Outcome's log stays while the reusing Run keeps its detail"
    );
}
