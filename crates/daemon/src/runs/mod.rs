//! Runs: one execution of a Pipeline against one head SHA of a Watched PR.
//!
//! [`Runs::sync`] follows what the last poll saw. A Watched PR whose base
//! has a Pipeline gets a Run on its head SHA. A push ends the Run, as
//! pushed if slopwatch made it and superseded otherwise, and starts the next
//! one (ADR 0001). The Run reads the Pipeline from the PR's root base in
//! the daemon's own clone and records that base SHA (ADR 0007).
//!
//! When the Pipeline on the base changes, a PR whose latest Run has ended
//! gets a new Run on the same SHA. A Step about to start first looks for
//! an earlier Outcome on that SHA under the same reuse key and takes it
//! instead of running, unless its Verdict was error, cancelled, missing or
//! skipped. A Run still going keeps its Pipeline; the change reaches the PR
//! once it ends.
//!
//! Within a Run, core's [`Pipeline::plan`] decides what happens next, and
//! the engine carries it out: it queues each Step that may start, marks
//! skips, and ends the Run once every Step has settled. Queued Steps start
//! as their own processes (ADR 0003) in the order they became ready, while
//! fewer than [`STEP_CAP`] run across all Runs and fewer than their
//! Plugin's own cap run. A determined Gate never cancels a running Step
//! (ADR 0006). Every change goes to the store and to the Run's event
//! journal, and the `run/<id>` topic replays from the journal. The store
//! keeps each Step's state, so a restarted daemon picks its Runs back up
//! (ADR 0009).
//!
//! Nothing retries on its own. The developer can retry an errored Step,
//! which reruns it and every Step after it in the same Run, and can cancel
//! any Run that's still going.
//!
//! Each attempt of a Step writes its own Step log under `logs/`, which
//! clients follow live and page through ([`log`]). [`Runs::prune`] drops
//! the detail of old Runs and keeps their record ([`retention`]).
//! The developer can also waive a Step's settled, non-pass Verdict, or
//! override the Gate, which waives every Step that makes a Gate term fail.
//! A Waiver belongs to the head SHA, so every Run on that SHA counts it and
//! a push leaves it behind. In a Run that's still going the Gate moves at
//! once. On an ended Run, a new Run starts on the same SHA with the ended
//! Run's Pipeline, and a waived Step takes its earlier Outcome whatever its
//! Verdict, so nothing runs again just to be waived.
//!
//! One lock serializes the engine: a sync and the reports from Step
//! processes take turns. Reading a Pipeline from git happens outside it.

mod effects;
pub(crate) mod journal;
pub mod log;
pub mod process;
mod retention;
mod source;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use slopwatch_core::{
    Decision, EndReason, GATE, GateState, Pipeline, PrFacts, RunState, Step, StepState, Verdict,
    load, parse_duration,
};
use slopwatch_protocol::step::{
    EffectKind, FromStep, LinkedIssue, MERGE_STATE, Manifest, MergeState, Outcome, Outputs,
    PR_DIFF, PrSnapshot, Start, ToStep,
};
use slopwatch_protocol::{
    Actor, Answer, Cause, Closing, GateTerm, LogFilter, LogKey, LogPage, LogRecord, PrRef,
    RepoName, RunEvent, RunId, SecretInfo, SecretValue, StepInfo, StepLogPage, Waiver,
};
use tokio::sync::mpsc;

use crate::approvals::Approval;
use crate::clones::{Clones, PipelineAt};
use crate::github::{GitHub, GitHubError, OpenPr};
use crate::inbox::Inbox;
use crate::notifications::Notifications;
use crate::plugins::{Plugins, human};
use crate::secrets::{Keychain, Mask, SecretError, Secrets, held_by_cause, missing_secret};
use crate::shell_env;
use crate::store::{ActiveRun, NewRun, NewStep, StepRow, StepRowState, Store, StoreError};
use crate::watching::{RunInfo, Watching};

use journal::Journal;
pub use journal::{Journalled, Live};
pub use log::LiveLog;
use process::{Limits, Report, Spawn, StepHandle, Tripped};
pub use retention::Retention;

/// How many of a PR's newest Runs its row carries, for the history chips.
pub const HISTORY_ON_ROW: usize = 20;

/// The most Step processes that run at once, across every Run.
pub const STEP_CAP: usize = 8;

/// How many of a Step log's latest records a new log subscriber gets.
pub const LOG_TAIL: u32 = 200;

/// How often the daemon looks for detail to prune.
const PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

type PrKey = (RepoName, u64);

/// What GitHub said about merging each PR, and which read it answered.
/// `None` means GitHub has no such PR anymore.
type MergeStates = HashMap<PrKey, (MergeRead, Option<MergeState>)>;

/// A read of a PR's merge state, as the sync asks for it. A Run takes the
/// answer only if it still judges `head_sha`, and if no merge it asked for
/// finished while the read was out, which bumps its `merge_epoch`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeRead {
    head_sha: String,
    epoch: u64,
}

pub struct Runs {
    engine: tokio::sync::Mutex<Engine>,
    journal: Arc<Journal>,
    github: Arc<dyn GitHub>,
    clones: Clones,
    live: AtomicBool,
    store: Store,
    watching: Arc<Watching>,
    logs: log::Hub,
    data_dir: PathBuf,
    retention: Retention,
    inbox: Arc<Inbox>,
    notifications: Arc<Notifications>,
    secrets: Arc<Secrets>,
}

/// Where Runs keep their files, and for how long.
pub struct RunsConfig {
    /// Clones under `repos/`, Step directories under `worktrees/`, Step
    /// logs under `logs/`.
    pub data_dir: PathBuf,
    pub plugins: Plugins,
    /// The `PATH` the developer's login shell reports
    /// ([`shell_env::login_shell_path`]). Steps get the daemon's own `PATH`
    /// with this one merged after it. `None` leaves them the daemon's.
    pub login_path: Option<String>,
    pub retention: Retention,
    /// Where Secret values live: the login Keychain in the daemon, a
    /// [`MemoryKeychain`](crate::secrets::MemoryKeychain) in tests.
    pub keychain: Arc<dyn Keychain>,
}

#[derive(Debug)]
pub enum SubscribeError {
    NotFound(RunId),
    Store(StoreError),
}

/// Why a developer's command on a Run was refused.
#[derive(Debug)]
pub enum RunError {
    /// No such Run, or no such Step in it.
    NotFound(String),
    /// The Run or Step isn't in a state the command applies to.
    Invalid(String),
    Store(StoreError),
}

impl From<StoreError> for RunError {
    fn from(error: StoreError) -> Self {
        RunError::Store(error)
    }
}

/// Why a Step log can't be read.
#[derive(Debug)]
pub enum LogError {
    /// No such Run, or no such Step in it.
    NotFound(String),
    /// The Run's detail was pruned at this time, in seconds.
    Pruned(i64),
    Store(StoreError),
    Io(std::io::Error),
}

impl From<StoreError> for LogError {
    fn from(error: StoreError) -> Self {
        LogError::Store(error)
    }
}

impl From<std::io::Error> for LogError {
    fn from(error: std::io::Error) -> Self {
        LogError::Io(error)
    }
}

impl Runs {
    /// Loads the Runs that hadn't ended and starts taking Step reports.
    /// Their Steps start again on the first sync, once a poll has shown
    /// where each PR stands. Must run inside a tokio runtime.
    pub fn start(
        store: Store,
        github: Arc<dyn GitHub>,
        watching: Arc<Watching>,
        config: RunsConfig,
    ) -> Result<Arc<Self>, StoreError> {
        let journal = Arc::new(Journal::new(store.clone()));
        let (reports, mut received) = mpsc::unbounded_channel();
        let clones = Clones::new(config.data_dir.join("repos"));
        let step_path = shell_env::merge(
            std::env::var("PATH").ok().as_deref(),
            config.login_path.as_deref(),
        );
        let logs = log::Hub::default();
        let notifications = Arc::new(Notifications::load(store.clone())?);
        let inbox = Arc::new(Inbox::load(
            store.clone(),
            Arc::clone(&journal),
            Arc::clone(&notifications),
        )?);
        // Built-in Plugins ship approved for what their manifests ask
        // (ADR 0012), written again on every start so an update's new
        // manifest is the one approved.
        for manifest in config.plugins.builtin_manifests() {
            store.put_approval(&Approval::builtin(&manifest, now()))?;
        }
        let secrets = Arc::new(Secrets::new(config.keychain, store.clone()));
        {
            let secrets = Arc::clone(&secrets);
            tokio::task::spawn_blocking(move || secrets.warm());
        }
        let mut engine = Engine {
            secrets: Arc::clone(&secrets),
            inbox: Arc::clone(&inbox),
            notifications: Arc::clone(&notifications),
            data_dir: config.data_dir.clone(),
            plugins: config.plugins,
            step_path,
            ready: VecDeque::new(),
            processes: HashMap::new(),
            logs: logs.clone(),
            store: store.clone(),
            journal: Arc::clone(&journal),
            watching: Arc::clone(&watching),
            github: Arc::clone(&github),
            reports,
            active: BTreeMap::new(),
            watched: watching
                .prs()
                .into_iter()
                .filter(|(_, pr)| pr.labeled)
                .map(|(repo, pr)| (repo, pr.number))
                .collect(),
            blocked: HashMap::new(),
            checked_base: HashMap::new(),
            reconciled: false,
            stopped: false,
        };
        engine.load()?;
        let runs = Arc::new(Self {
            live: AtomicBool::new(!engine.active.is_empty()),
            engine: tokio::sync::Mutex::new(engine),
            journal,
            github,
            clones,
            store,
            watching,
            logs,
            data_dir: config.data_dir,
            retention: config.retention,
            inbox,
            notifications,
            secrets,
        });
        let driver = Arc::clone(&runs);
        tokio::spawn(async move {
            while let Some(input) = received.recv().await {
                let mut engine = driver.engine.lock().await;
                match input {
                    Input::Step(report) => engine.on_report(report),
                    Input::Effect(finished) => engine.on_effect_finished(finished),
                }
                engine.schedule();
                driver
                    .live
                    .store(!engine.active.is_empty(), Ordering::Relaxed);
            }
        });
        Ok(runs)
    }

    /// Brings Runs in line with the PRs as the last poll saw them. Until
    /// GitHub has answered for every repo since the daemon started, the
    /// PRs are only what the store kept, so nothing moves (ADR 0009).
    pub async fn sync(&self) {
        let wanted = {
            let engine = self.engine.lock().await;
            if !engine.watching.fresh() {
                return;
            }
            // The same PRs go to the sync below, so a poll in between
            // can't show a PR gone that the merge states saw open.
            let prs = engine.watching.prs();
            let wanted = engine.merge_states_wanted(&prs);
            (prs, wanted)
        };
        let (prs, wanted) = wanted;
        let merge_states = self.read_merge_states(wanted).await;
        let starts = {
            let mut engine = self.engine.lock().await;
            let starts = engine.sync(prs, merge_states);
            engine.schedule();
            starts
        };
        for (repo, pr, when) in starts {
            let read = self.read_inputs(&repo, &pr, &when).await;
            let mut engine = self.engine.lock().await;
            engine.start_run(&repo, &pr, when, read);
            engine.schedule();
        }
        let engine = self.engine.lock().await;
        self.live
            .store(!engine.active.is_empty(), Ordering::Relaxed);
    }

    /// Asks GitHub where each PR stands for merging, at the head its Run
    /// judges. A PR GitHub can't find reads as `None`. One that can't be
    /// read for another reason is left out, and the next sync asks again.
    async fn read_merge_states(&self, wanted: Vec<(PrKey, MergeRead)>) -> MergeStates {
        let mut states = HashMap::new();
        for ((repo, number), read) in wanted {
            match self.github.merge_state(&repo, number, &read.head_sha).await {
                Ok(state) => {
                    states.insert((repo, number), (read, Some(state)));
                }
                Err(GitHubError::NotFound(_)) => {
                    states.insert((repo, number), (read, None));
                }
                Err(error) => {
                    eprintln!("slopwatchd: can't read {repo}#{number}'s merge state: {error}");
                }
            }
        }
        states
    }

    /// What a Run on the PR's head starts from: the Pipeline at the tip of
    /// its base, and the files the head changes, both from the repo's
    /// clone, with the issues the PR links. The diff too, when a Step in
    /// the Pipeline asks for it.
    async fn read_inputs(
        &self,
        repo: &RepoName,
        pr: &OpenPr,
        when: &StartWhen,
    ) -> Result<Inputs, String> {
        let remote = self
            .github
            .git_remote(repo)
            .await
            .map_err(|error| error.to_string())?;
        let pipeline = self
            .clones
            .pipeline_at(repo, &remote, &pr.base)
            .await
            .map_err(|error| format!("can't read the Pipeline on {}: {error}", pr.base))?;
        let unchanged = match when {
            StartWhen::PipelineChanged { from } => pipeline.text.as_ref() == Some(from),
            StartWhen::Always => false,
        };
        // No Run starts on a missing or unchanged Pipeline, so its files
        // aren't needed.
        if pipeline.text.is_none() || unchanged {
            return Ok(Inputs {
                pipeline,
                files: Vec::new(),
                linked_issues: Vec::new(),
                diff: None,
            });
        }
        let files = self
            .clones
            .changed_files(repo, &remote, pr.number, &pipeline.sha, &pr.head_sha)
            .await
            .map_err(|error| format!("can't list the files the PR changes: {error}"))?;
        let linked_issues = self
            .github
            .linked_issues(repo, pr.number)
            .await
            .map_err(|error| format!("can't read the issues the PR links: {error}"))?;
        let text = pipeline.text.as_deref().unwrap_or_default();
        let diff = if self.engine.lock().await.wants_diff(text) {
            let path = diff_path(&self.data_dir, repo, pr.number, &pipeline.sha, &pr.head_sha);
            if !path.exists() {
                let diff = self
                    .clones
                    .diff(repo, &remote, pr.number, &pipeline.sha, &pr.head_sha)
                    .await
                    .map_err(|error| format!("can't read the PR's diff: {error}"))?;
                write_diff(&path, diff)
                    .await
                    .map_err(|error| format!("can't keep the PR's diff: {error}"))?;
            }
            Some(path)
        } else {
            None
        };
        Ok(Inputs {
            pipeline,
            files,
            linked_issues,
            diff,
        })
    }

    /// Ends a Run that's still going as cancelled.
    pub async fn cancel(&self, run: RunId) -> Result<(), RunError> {
        self.command(|engine| engine.cancel(run)).await
    }

    /// Runs the errored Step `step` again in its Run, with every Step
    /// after it. That answers the Run entries of those Steps.
    pub async fn retry(&self, run: RunId, step: &str, actor: &Actor) -> Result<(), RunError> {
        self.command(|engine| engine.retry(run, step, actor)).await
    }

    /// Every Secret that's set or that an Approval covers. Names and dates
    /// only.
    pub fn list_secrets(&self) -> Result<Vec<SecretInfo>, SecretError> {
        let approvals = self.store.approvals()?;
        Ok(self.secrets.list(&approvals)?)
    }

    /// Sets or rotates a Secret. The next Step spawned gets the new value.
    /// A missing Secret's Inbox entry clears, and the PRs it held start
    /// again.
    pub async fn set_secret(&self, name: String, value: SecretValue) -> Result<(), SecretError> {
        let set_name = name.clone();
        self.on_secrets(move |secrets| secrets.set(&set_name, value, now()))
            .await?;
        let mut engine = self.engine.lock().await;
        if let Err(error) = engine.secret_set(&name) {
            eprintln!("slopwatchd: can't restart what `{name}` held back: {error}");
        }
        engine.schedule();
        self.live
            .store(!engine.active.is_empty(), Ordering::Relaxed);
        Ok(())
    }

    /// Removes a Secret. Steps that require it error from their next spawn.
    pub async fn delete_secret(&self, name: String) -> Result<(), SecretError> {
        self.on_secrets(move |secrets| secrets.delete(&name)).await
    }

    /// Runs `work` on the blocking pool, since it waits on the Keychain.
    async fn on_secrets(
        &self,
        work: impl FnOnce(&Secrets) -> Result<(), SecretError> + Send + 'static,
    ) -> Result<(), SecretError> {
        let secrets = Arc::clone(&self.secrets);
        tokio::task::spawn_blocking(move || work(&secrets))
            .await
            .unwrap_or_else(|error| {
                Err(SecretError::Internal(format!(
                    "Secret work failed: {error}"
                )))
            })
    }

    /// Answers the Human Step `step`, which waits in Run `run`. The note,
    /// if not blank, goes into the Step's Outcome for later Steps to read.
    pub async fn answer(
        &self,
        run: RunId,
        step: &str,
        answer: Answer,
        note: Option<String>,
        actor: &Actor,
    ) -> Result<(), RunError> {
        self.command(|engine| engine.answer_step(run, step, answer, note, actor))
            .await
    }

    /// The Inbox, which these Runs raise and close entries in.
    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    /// The notifications these Runs and their Inbox record for the GUI.
    pub fn notifications(&self) -> &Arc<Notifications> {
        &self.notifications
    }

    /// Waives Step `step`'s settled, non-pass Verdict for the Run's head
    /// SHA.
    pub async fn waive(&self, run: RunId, step: &str, waiver: Waiver) -> Result<(), RunError> {
        check_reason(&waiver)?;
        self.command(|engine| engine.waive(run, Waive::Step(step), &waiver))
            .await
    }

    /// Waives every Step that makes one of the Gate's terms fail.
    pub async fn override_gate(&self, run: RunId, waiver: Waiver) -> Result<(), RunError> {
        check_reason(&waiver)?;
        self.command(|engine| engine.waive(run, Waive::Gate, &waiver))
            .await
    }

    /// Carries out a developer's command on a Run, then starts whatever it
    /// freed up or queued.
    async fn command(
        &self,
        act: impl FnOnce(&mut Engine) -> Result<(), RunError>,
    ) -> Result<(), RunError> {
        let mut engine = self.engine.lock().await;
        let result = act(&mut engine);
        engine.schedule();
        self.live
            .store(!engine.active.is_empty(), Ordering::Relaxed);
        result
    }

    /// Kills every running Step's process group at once, for a daemon
    /// about to exit. The Runs stay as they are in the store, and the next
    /// start picks them back up.
    pub async fn kill_steps(&self) {
        let mut engine = self.engine.lock().await;
        // What the killed Steps report now is the kill, not their work, so
        // none of it goes on record.
        engine.stopped = true;
        for run in engine.active.values() {
            for running in run.running.values() {
                running.handle.kill();
            }
        }
    }

    /// Whether any Run is going, which makes polling faster.
    pub fn live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    /// The Run's events after `after`, then every event appended from now
    /// on, across all Runs.
    pub fn subscribe(
        &self,
        run: RunId,
        after: u64,
    ) -> Result<(Vec<Journalled>, Live), SubscribeError> {
        if !self.journal.has_run(run).map_err(SubscribeError::Store)? {
            return Err(SubscribeError::NotFound(run));
        }
        self.journal
            .subscribe(run, after)
            .map_err(SubscribeError::Store)
    }

    /// The Run's stored events after `after`, for a subscriber that fell
    /// behind the live ones.
    pub fn replay(&self, run: RunId, after: u64) -> Result<Vec<Journalled>, StoreError> {
        self.journal.replay(run, after)
    }

    /// The Step log's records after `after`, at most the latest
    /// [`LOG_TAIL`], then every record written to any log from now on.
    pub async fn subscribe_log(
        &self,
        key: &LogKey,
        after: u64,
    ) -> Result<(Vec<LogRecord>, LiveLog), LogError> {
        self.check_log(key)?;
        // Subscribing first means a record written while the file is read
        // arrives twice at worst, and the connection drops the repeat.
        let live = self.logs.subscribe();
        let dir = log_dir(&self.data_dir, key);
        let records = blocking(move || log::read_after(&dir, after, LOG_TAIL)).await?;
        Ok((records, live))
    }

    /// One page of a Step log, searched and filtered.
    pub async fn read_log(
        &self,
        key: LogKey,
        page: LogPage,
        filter: LogFilter,
    ) -> Result<StepLogPage, LogError> {
        let attempt = self.check_log(&key)?;
        let dir = log_dir(
            &self.data_dir,
            &LogKey {
                attempt,
                ..key.clone()
            },
        );
        let found = {
            let filter = filter.clone();
            blocking(move || log::read_page(&dir, page, &filter)).await?
        };
        Ok(StepLogPage {
            key,
            page,
            filter,
            records: found.records,
            more_before: found.more_before,
            more_after: found.more_after,
            truncated: found.truncated,
        })
    }

    /// Checks the log can be read, and returns its attempt: the key's, or
    /// for attempt 0, the Step's latest.
    fn check_log(&self, key: &LogKey) -> Result<u32, LogError> {
        let Some(latest) = self.store.step_attempt(key.run, &key.step)? else {
            return Err(LogError::NotFound(format!(
                "Step `{}` in Run {}",
                key.step, key.run
            )));
        };
        if let Some(at) = self.store.pruned_at(key.run)? {
            return Err(LogError::Pruned(at));
        }
        Ok(if key.attempt == 0 {
            latest
        } else {
            key.attempt
        })
    }

    /// Prunes the detail retention says should go at `now`, in seconds
    /// since the epoch, and shows or clears the storage warning.
    pub async fn prune(&self, now: i64) -> Result<(), StoreError> {
        let unpruned = self.store.unpruned_runs()?;
        let data_dir = self.data_dir.clone();
        let details = blocking(move || {
            Ok(unpruned
                .into_iter()
                .map(|run| retention::Detail {
                    run: run.id,
                    ended_at: run.ended_at,
                    latest_of_watched: run.latest && run.pr_watched,
                    reused: run.reused,
                    pr_closed: !run.pr_open,
                    bytes: run.journal_bytes + log::disk_usage(&run_logs(&data_dir, run.id)),
                })
                .collect::<Vec<_>>())
        })
        .await
        .unwrap_or_else(|error| {
            eprintln!("slopwatchd: can't size Run detail: {error}");
            Vec::new()
        });
        let plan = retention::plan(&details, self.retention, now);
        for run in plan.prune {
            let logs = run_logs(&self.data_dir, run);
            if let Err(error) = tokio::fs::remove_dir_all(&logs).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!("slopwatchd: can't prune {}: {error}", logs.display());
                continue;
            }
            let _ = tokio::fs::remove_dir_all(run_dir(&self.data_dir, run)).await;
            if let Err(error) = self.journal.prune(run, now) {
                eprintln!("slopwatchd: can't prune Run {run}'s journal: {error}");
            }
        }
        self.watching.set_storage_warning(plan.warning);
        Ok(())
    }

    /// Prunes now and then every hour, for good.
    pub async fn prune_forever(self: Arc<Self>) {
        loop {
            if let Err(error) = self.prune(now()).await {
                eprintln!("slopwatchd: can't prune Run detail: {error}");
            }
            tokio::time::sleep(PRUNE_EVERY).await;
        }
    }
}

struct Engine {
    store: Store,
    secrets: Arc<Secrets>,
    inbox: Arc<Inbox>,
    notifications: Arc<Notifications>,
    plugins: Plugins,
    logs: log::Hub,
    journal: Arc<Journal>,
    watching: Arc<Watching>,
    data_dir: PathBuf,
    /// The `PATH` every Step gets.
    step_path: String,
    github: Arc<dyn GitHub>,
    reports: mpsc::UnboundedSender<Input>,
    /// The Run going on each PR. A PR has at most one.
    active: BTreeMap<PrKey, Active>,
    /// Steps the plan starts that wait for a free slot, oldest first.
    ready: VecDeque<(RunId, String)>,
    /// Every Step process that hasn't exited, by Run, Step and attempt,
    /// with its Plugin. A cancelled Run's processes count until they're
    /// gone.
    processes: HashMap<(RunId, String, u32), String>,
    /// PRs that were watched at the last sync.
    watched: HashSet<PrKey>,
    /// Why a PR's next Run can't start.
    blocked: HashMap<PrKey, Blocked>,
    /// The base SHA whose Pipeline each PR's latest Run was last compared
    /// with, so a base commit that leaves the Pipeline alone is read once.
    checked_base: HashMap<PrKey, String>,
    /// The Effect intents a crash left open have been settled, which waits
    /// for the first sync (ADR 0009).
    reconciled: bool,
    /// The daemon is about to exit: nothing more changes.
    stopped: bool,
}

/// When a PR the sync picked gets its new Run.
#[derive(Debug, Clone)]
enum StartWhen {
    /// Always: the PR has a new head or was just watched.
    Always,
    /// Only if the Pipeline on its base differs from `from`, the text its
    /// latest Run, now ended, read. The new Run is on the same SHA (ADR
    /// 0007).
    PipelineChanged { from: String },
}

/// What a new Run reads from git before it starts.
struct Inputs {
    pipeline: PipelineAt,
    files: Vec<String>,
    linked_issues: Vec<LinkedIssue>,
    diff: Option<PathBuf>,
}

/// What a Run's snapshot carries besides what the poll reads, fixed when
/// the Run starts.
#[derive(Debug, Clone, Default)]
struct Evidence {
    linked_issues: Vec<LinkedIssue>,
    /// The diff file, when a Step in the Pipeline asks for it.
    diff: Option<PathBuf>,
}

impl Evidence {
    fn fill(&self, snapshot: &mut PrSnapshot) {
        snapshot.linked_issues.clone_from(&self.linked_issues);
        snapshot.diff.clone_from(&self.diff);
    }
}

struct Blocked {
    /// The Pipeline that doesn't load, and the base commit it's on. The
    /// next sync loads the same text again, which comes out different
    /// only after a Library edit, and reads the base again once it
    /// moves. `None` when the read itself failed, and the next sync
    /// tries again.
    invalid: Option<Invalid>,
    message: String,
}

struct Invalid {
    base: String,
    base_sha: String,
    text: String,
}

/// A Run that hasn't ended.
struct Active {
    id: RunId,
    repo: RepoName,
    number: u64,
    head_sha: String,
    pipeline: Pipeline,
    state: RunState,
    gate: GateState,
    /// The PR as the last poll saw it. `None` after a restart until the
    /// first sync, and nothing starts before then.
    snapshot: Option<PrSnapshot>,
    /// The paths the head changes, for `files:` Conditions.
    files: Vec<String>,
    evidence: Evidence,
    /// Steps the developer's retry reset. They run again rather than take
    /// an earlier Run's Outcome.
    rerun: HashSet<String>,
    outcomes: HashMap<String, Outcome>,
    /// Why the daemon gave a settled Step its Verdict, such as an error's
    /// cause, for the Inbox.
    reasons: HashMap<String, String>,
    attempts: HashMap<String, u32>,
    running: HashMap<String, Running>,
    /// Steps a restart killed before they reported, with how many restarts
    /// in a row did. Each starts again or settles once the first poll has
    /// run (ADR 0009).
    interrupted: BTreeMap<String, u32>,
    /// GitHub merged the PR after the Run asked it to, and the PR has left
    /// the poll. The Run ends merged once its Steps settle.
    merged: bool,
    /// How many merges the Run asked for have finished. A merge state
    /// read before the latest one doesn't count (see [`MergeRead`]).
    merge_epoch: u64,
}

impl Active {
    /// The merge state read this Run would take now.
    fn merge_read(&self) -> MergeRead {
        MergeRead {
            head_sha: self.head_sha.clone(),
            epoch: self.merge_epoch,
        }
    }
}

/// What a developer's Waiver covers.
#[derive(Debug, Clone, Copy)]
enum Waive<'a> {
    Step(&'a str),
    /// Every Step behind a failing Gate term.
    Gate,
}

fn check_reason(waiver: &Waiver) -> Result<(), RunError> {
    if waiver.reason.trim().is_empty() {
        return Err(RunError::Invalid("A Waiver needs a reason".to_owned()));
    }
    Ok(())
}

/// The Steps `what` waives in a Run in `state`, or why it can't.
fn waivable(
    pipeline: &Pipeline,
    state: &RunState,
    what: Waive<'_>,
) -> Result<Vec<String>, RunError> {
    match what {
        Waive::Step(step) => {
            if pipeline.step(step).is_none() {
                return Err(RunError::NotFound(format!("The Run has no Step `{step}`")));
            }
            let verdict = match state.steps.get(step) {
                Some(StepState::Settled(verdict)) => *verdict,
                Some(StepState::Running) => {
                    return Err(RunError::Invalid(format!(
                        "Step `{step}` is still running, and only a settled Verdict can be \
                         waived. Cancel the Run first to waive it as cancelled."
                    )));
                }
                _ => {
                    return Err(RunError::Invalid(format!(
                        "Step `{step}` hasn't settled, and only a settled Verdict can be waived"
                    )));
                }
            };
            if verdict == Verdict::Pass {
                return Err(RunError::Invalid(format!(
                    "Step `{step}` passed, and only a non-pass Verdict can be waived"
                )));
            }
            if state.waived.contains(step) {
                return Err(RunError::Invalid(format!(
                    "Step `{step}` is already waived on this head SHA"
                )));
            }
            Ok(vec![step.to_owned()])
        }
        Waive::Gate => {
            if pipeline.gate(state) != GateState::Fail {
                return Err(RunError::Invalid(
                    "The Gate isn't failing, so there's nothing to override".to_owned(),
                ));
            }
            let steps = pipeline.failing_waivable_steps(state);
            if steps.is_empty() {
                return Err(RunError::Invalid(
                    "No Waiver can pass the Gate: its failing terms read no settled, non-pass \
                     Verdict"
                        .to_owned(),
                ));
            }
            Ok(steps)
        }
    }
}

/// How many restarts in a row may interrupt a Step before it ends
/// `error(daemon_restart)` instead of starting again, so a Step that
/// crashes the daemon can't loop it (ADR 0009).
const RESTARTS_BEFORE_ERROR: u32 = 2;

struct Running {
    attempt: u32,
    handle: StepHandle,
    /// The Step reported its Outcome, and only its exit is still to come.
    reported: bool,
    /// The request ids of Effects it asked for that haven't finished.
    awaiting: HashSet<String>,
    /// Where it stands with the developer, for a Step that asks.
    question: Question,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Question {
    NotAsked,
    /// In the Inbox, waiting for the developer.
    Asked,
    /// The developer answered, and the Step's Outcome is on its way. Its
    /// Inbox entry closes this way once the Outcome is in, so an answer a
    /// restart lost leaves the entry open to answer again.
    Answered(Closing),
}

/// What the engine hears from the tasks around it.
enum Input {
    Step(StepReport),
    Effect(effects::Finished),
}

struct StepReport {
    run: RunId,
    step: String,
    attempt: u32,
    report: Report,
}

impl Engine {
    /// Picks up the Runs that hadn't ended when the daemon last stopped,
    /// keeping their settled Outcomes. A Step that was running is
    /// interrupted: its leftover process group is killed and its directory
    /// deleted, and after the first poll it starts again from scratch,
    /// which isn't a retry because it never reported.
    fn load(&mut self) -> Result<(), StoreError> {
        for mut stored in self.store.active_runs()? {
            for row in &mut stored.steps {
                if let StepRowState::Running {
                    pgid,
                    started_us,
                    sid,
                } = row.state
                {
                    process::kill_leftover(&process::Leftover {
                        pgid,
                        started_us,
                        sid,
                    });
                    let _ = std::fs::remove_dir_all(step_dir(
                        &self.data_dir,
                        stored.id,
                        &row.step,
                        row.attempt,
                    ));
                    let restarts = self.store.interrupt_step(stored.id, &row.step)?;
                    row.state = StepRowState::Interrupted { restarts };
                }
            }
            let pipeline = match load(&stored.pipeline, &self.plugins) {
                Ok(pipeline) => pipeline,
                Err(errors) => {
                    let reason = format!("the Pipeline no longer loads: {}", join(&errors));
                    eprintln!("slopwatchd: Run {}: {reason}, ending it", stored.id);
                    self.end_unloadable(&stored, &reason)?;
                    continue;
                }
            };
            let mut active = Active {
                id: stored.id,
                repo: stored.repo.clone(),
                number: stored.number,
                head_sha: stored.head_sha.clone(),
                pipeline,
                state: RunState::default(),
                gate: stored.gate,
                snapshot: None,
                files: stored.files,
                evidence: Evidence {
                    diff: kept_diff(
                        &self.data_dir,
                        &stored.repo,
                        stored.number,
                        &stored.base_sha,
                        &stored.head_sha,
                    ),
                    linked_issues: stored.linked_issues,
                },
                outcomes: HashMap::new(),
                reasons: HashMap::new(),
                attempts: HashMap::new(),
                running: HashMap::new(),
                interrupted: BTreeMap::new(),
                rerun: HashSet::new(),
                merged: false,
                merge_epoch: 0,
            };
            active.state.waived =
                self.store
                    .waived_steps(&active.repo, active.number, &active.head_sha)?;
            for row in stored.steps {
                active.attempts.insert(row.step.clone(), row.attempt);
                match row.state {
                    StepRowState::Settled {
                        verdict,
                        outputs,
                        reason,
                    } => {
                        if let Some(reason) = reason {
                            active.reasons.insert(row.step.clone(), reason);
                        }
                        active
                            .state
                            .steps
                            .insert(row.step.clone(), StepState::Settled(verdict));
                        active
                            .outcomes
                            .insert(row.step, Outcome { verdict, outputs });
                    }
                    StepRowState::Interrupted { restarts } => {
                        active.interrupted.insert(row.step, restarts);
                    }
                    StepRowState::Running { .. } | StepRowState::Pending => {}
                }
            }
            let key = (stored.repo, stored.number);
            if let Some(older) = self.active.insert(key.clone(), active) {
                self.end_run(older, EndReason::Superseded)?;
            }
        }
        let going = self.active.values().map(|run| run.id).collect();
        self.inbox.keep_runs(&going)?;
        for (repo, number) in self.store.prs_with_runs()? {
            self.publish(&repo, number)?;
        }
        Ok(())
    }

    /// Ends a Run whose Pipeline stopped loading while the daemon was
    /// down, as after an update dropped a Plugin it uses. Its unsettled
    /// Steps get `error` with the reason, so the Run ends not shippable
    /// rather than cancelled, which only the developer does.
    fn end_unloadable(&mut self, run: &ActiveRun, reason: &str) -> Result<(), StoreError> {
        for row in &run.steps {
            if matches!(row.state, StepRowState::Settled { .. }) {
                continue;
            }
            let state = StepRowState::Settled {
                verdict: Verdict::Error,
                reason: Some(reason.to_owned()),
                outputs: Outputs::default(),
            };
            self.store.put_step(
                run.id,
                &StepRow {
                    step: row.step.clone(),
                    state,
                    attempt: row.attempt,
                },
            )?;
            self.journal.append(
                run.id,
                RunEvent::StepSettled {
                    step: row.step.clone(),
                    verdict: Verdict::Error,
                    reason: Some(reason.to_owned()),
                    outputs: Outputs::default(),
                    reused_from: None,
                },
            )?;
        }
        let ended = EndReason::NotShippable;
        self.store.end_run(run.id, ended, now())?;
        self.journal.append(
            run.id,
            RunEvent::Ended {
                reason: ended,
                waived: false,
            },
        )?;
        let _ = std::fs::remove_dir_all(run_dir(&self.data_dir, run.id));
        self.inbox.run_ended(run.id)?;
        self.inbox.raise_pr(
            pr_ref(&run.repo, run.number),
            run.id,
            NOT_SHIPPABLE,
            vec![reason.to_owned()],
        )
    }

    /// Ends the Runs the poll says are over, passes PR updates to running
    /// Steps, and returns the PRs that may need a new Run.
    fn sync(
        &mut self,
        prs: Vec<(RepoName, OpenPr)>,
        merge_states: MergeStates,
    ) -> Vec<(RepoName, OpenPr, StartWhen)> {
        if self.stopped {
            return Vec::new();
        }
        self.try_sync(prs, merge_states).unwrap_or_else(|error| {
            eprintln!("slopwatchd: can't update Runs: {error}");
            Vec::new()
        })
    }

    /// The PRs whose merge state the next sync needs, with the head SHA
    /// their Run judges: those with a running Step that reads it, and those
    /// that left the poll after their Run asked to merge them, which may
    /// mean GitHub merged them.
    /// Whether a Step in the Pipeline `text` asks for the PR's diff
    /// ([`PR_DIFF`]). A Pipeline that doesn't load asks for nothing.
    fn wants_diff(&self, text: &str) -> bool {
        load(text, &self.plugins).is_ok_and(|pipeline| {
            pipeline.steps().any(|step| {
                self.plugins
                    .manifest(&step.plugin)
                    .is_some_and(|manifest| manifest.features.iter().any(|f| f == PR_DIFF))
            })
        })
    }

    fn merge_states_wanted(&self, prs: &[(RepoName, OpenPr)]) -> Vec<(PrKey, MergeRead)> {
        let open: HashSet<PrKey> = prs
            .iter()
            .map(|(repo, pr)| (repo.clone(), pr.number))
            .collect();
        self.active
            .iter()
            .filter(|(key, run)| {
                if run.merged {
                    return false;
                }
                if open.contains(*key) {
                    return run.running.iter().any(|(step, running)| {
                        !running.reported && self.reads_merge_state(run, step)
                    });
                }
                self.store
                    .asked_for(run.id, EffectKind::Merge)
                    .unwrap_or_else(|error| {
                        eprintln!("slopwatchd: can't read Run {}'s Effects: {error}", run.id);
                        false
                    })
            })
            .map(|(key, run)| (key.clone(), run.merge_read()))
            .collect()
    }

    fn reads_merge_state(&self, run: &Active, step: &str) -> bool {
        run.pipeline
            .step(step)
            .and_then(|step| self.plugins.manifest(&step.plugin))
            .is_some_and(|manifest| manifest.features.iter().any(|f| f == MERGE_STATE))
    }

    fn try_sync(
        &mut self,
        prs: Vec<(RepoName, OpenPr)>,
        merge_states: MergeStates,
    ) -> Result<Vec<(RepoName, OpenPr, StartWhen)>, StoreError> {
        let open: BTreeMap<PrKey, (RepoName, OpenPr)> = prs
            .into_iter()
            .map(|(repo, pr)| ((repo.clone(), pr.number), (repo, pr)))
            .collect();

        let keys: Vec<PrKey> = self.active.keys().cloned().collect();
        for key in keys {
            let run = &self.active[&key];
            let ending = match open.get(&key) {
                // GitHub merged it, and the Run ends once its Steps settle.
                None if run.merged => continue,
                None => match merge_states.get(&key) {
                    Some((_, Some(state))) if state.merged => {
                        self.merged_on_github(&key, state)?;
                        continue;
                    }
                    // A merge finished while the read was out, so its
                    // "not merged" may predate the merge.
                    Some((read, _)) if read.epoch != run.merge_epoch => continue,
                    Some(_) => Some(EndReason::Closed),
                    // The Run asked to merge it, but GitHub couldn't say
                    // whether it did. The next sync asks again.
                    None if self.store.asked_for(run.id, EffectKind::Merge)? => continue,
                    None => Some(EndReason::Closed),
                },
                Some((_, pr)) if !pr.labeled => Some(EndReason::Cancelled),
                Some((repo, pr)) if pr.head_sha != run.head_sha => {
                    // A rebase doesn't say what head it made, so whatever
                    // head follows one is its push (ADR 0004).
                    let ours = self.store.pushed_by_slopwatch(repo, &pr.head_sha)?
                        || self.store.asked_for(run.id, EffectKind::Rebase)?;
                    Some(if ours {
                        EndReason::Pushed
                    } else {
                        EndReason::Superseded
                    })
                }
                Some(_) => None,
            };
            if let Some(reason) = ending {
                let run = self.active.remove(&key).expect("listed above");
                self.end_run(run, reason)?;
                continue;
            }
            let (repo, pr) = &open[&key];
            let mut snapshot = snapshot(repo, pr);
            let run = self.active.get_mut(&key).expect("listed above");
            run.evidence.fill(&mut snapshot);
            // A merge state read this time replaces the last one, which
            // stays until then.
            snapshot.merge = match merge_states.get(&key) {
                Some((read, Some(state))) if *read == run.merge_read() => Some(state.clone()),
                _ => run.snapshot.as_ref().and_then(|last| last.merge.clone()),
            };
            if run.snapshot.as_ref() == Some(&snapshot) {
                continue;
            }
            let first = run.snapshot.is_none();
            run.state.pr = facts(&snapshot, &run.files);
            run.snapshot = Some(snapshot.clone());
            if first {
                self.resume(&key)?;
            } else {
                for running in run.running.values() {
                    running.handle.send(ToStep::PrUpdated {
                        snapshot: snapshot.clone(),
                    });
                }
            }
            // New PR facts, such as a label, can turn a Condition.
            self.advance(&key)?;
        }
        if !self.reconciled {
            self.reconcile_effects()?;
            self.reconciled = true;
        }
        self.clear_set_secrets()?;

        let gone: Vec<PrKey> = self
            .blocked
            .keys()
            .filter(|key| !open.get(key).is_some_and(|(_, pr)| pr.labeled))
            .cloned()
            .collect();
        for key in gone {
            self.unblock(&key, Closing::LeftWatched)?;
        }
        let mut starts = Vec::new();
        for (key, (repo, pr)) in &open {
            if !pr.labeled || self.active.contains_key(key) {
                continue;
            }
            if !pr.base_has_pipeline {
                // A Pipeline that's gone holds nothing back: the PR waits.
                self.unblock(key, Closing::NothingHeld)?;
                continue;
            }
            // An invalid Pipeline on a base that hasn't moved comes out
            // different only once the Library changed under it.
            let mut cleared = false;
            if let Some(invalid) = self.blocked.get(key).and_then(|b| b.invalid.as_ref())
                && invalid.base_sha == pr.detail.base_sha
            {
                if load(&invalid.text, &self.plugins).is_err() {
                    continue;
                }
                cleared = true;
            }
            let newly_watched = !self.watched.contains(key);
            let latest = self.store.latest_run(repo, pr.number)?;
            let when = match latest {
                Some(run) if !cleared && !newly_watched && run.head_sha == pr.head_sha => {
                    // The base moved since the latest Run read it, so its
                    // Pipeline may have changed. An empty SHA is a PR no
                    // poll has seen yet.
                    let base_sha = &pr.detail.base_sha;
                    let base_moved = !base_sha.is_empty()
                        && run.base_sha != *base_sha
                        && self.checked_base.get(key) != Some(base_sha);
                    if !base_moved {
                        continue;
                    }
                    StartWhen::PipelineChanged { from: run.pipeline }
                }
                _ => StartWhen::Always,
            };
            starts.push((repo.clone(), pr.clone(), when));
        }
        self.checked_base
            .retain(|key, _| open.get(key).is_some_and(|(_, pr)| pr.labeled));
        self.watched = open
            .into_iter()
            .filter(|(_, (_, pr))| pr.labeled)
            .map(|(key, _)| key)
            .collect();
        let watched = self
            .watched
            .iter()
            .map(|(repo, number)| pr_ref(repo, *number))
            .collect();
        self.inbox.keep_watched(&watched)?;
        Ok(starts)
    }

    /// GitHub merged a PR whose Run asked it to, as a merge queue does
    /// some time after the merge Effect. The PR has left the poll, so its
    /// Steps hear it through the merge state, and the Run ends merged once
    /// they settle.
    fn merged_on_github(&mut self, key: &PrKey, state: &MergeState) -> Result<(), StoreError> {
        let run = self.active.get_mut(key).expect("only active Runs merge");
        run.merged = true;
        let Some(snapshot) = run.snapshot.as_mut() else {
            // The daemon restarted, and with the PR gone no poll will
            // bring the snapshot its Steps need to go on.
            let run = self.active.remove(key).expect("found above");
            return self.end_run(run, EndReason::Merged);
        };
        snapshot.merge = Some(state.clone());
        for running in run.running.values() {
            running.handle.send(ToStep::PrUpdated {
                snapshot: snapshot.clone(),
            });
        }
        self.advance(key)
    }

    /// Starts a Run on the PR's head, with the Pipeline from its root
    /// base. A Stack's root base comes with Stacks; until then it's the
    /// PR's own base.
    fn start_run(
        &mut self,
        repo: &RepoName,
        pr: &OpenPr,
        when: StartWhen,
        read: Result<Inputs, String>,
    ) {
        if self.stopped {
            return;
        }
        if let Err(error) = self.try_start_run(repo, pr, when, read) {
            eprintln!(
                "slopwatchd: can't start a Run on {repo}#{}: {error}",
                pr.number
            );
        }
    }

    fn try_start_run(
        &mut self,
        repo: &RepoName,
        pr: &OpenPr,
        when: StartWhen,
        read: Result<Inputs, String>,
    ) -> Result<(), StoreError> {
        let key = (repo.clone(), pr.number);
        // Another sync started one while this one read the Pipeline.
        if self.active.contains_key(&key) {
            return Ok(());
        }
        let base = &pr.base;
        let Inputs {
            pipeline: read,
            files,
            linked_issues,
            diff,
        } = match (read, &when) {
            (Ok(inputs), _) => inputs,
            // The PR's latest Run stands, so nothing blocks it. The next
            // sync tries again.
            (Err(error), StartWhen::PipelineChanged { .. }) => {
                eprintln!(
                    "slopwatchd: can't start a Run on {repo}#{}: {error}",
                    pr.number
                );
                return Ok(());
            }
            (Err(error), StartWhen::Always) => {
                let message = format!("Can't start a Run: {error}");
                return self.block(&key, None, message);
            }
        };
        let Some(text) = read.text else {
            // The poll saw a Pipeline the fetch didn't. The next poll
            // catches up.
            return Ok(());
        };
        if let StartWhen::PipelineChanged { from } = &when {
            self.checked_base
                .insert(key.clone(), pr.detail.base_sha.clone());
            if *from == text {
                // A Pipeline that was invalid and is now back as it was,
                // perhaps while the daemon was down.
                self.inbox.clear(invalid_pipeline(repo, base))?;
                return self.unblock(&key, Closing::NothingHeld);
            }
        }
        let pipeline = match load(&text, &self.plugins) {
            Ok(pipeline) => pipeline,
            Err(errors) => {
                let message = format!("The Pipeline on {base} is invalid: {}", join(&errors));
                let invalid = Invalid {
                    base: base.clone(),
                    base_sha: pr.detail.base_sha.clone(),
                    text,
                };
                let reasons = errors.iter().map(ToString::to_string).collect();
                return self.block_invalid(&key, invalid, message, reasons);
            }
        };
        // The Pipeline on this base loads, so every PR it held back may
        // start.
        self.inbox.clear(invalid_pipeline(repo, base))?;
        self.unblock(&key, Closing::NothingHeld)?;

        let steps: Vec<_> = pipeline.ordered_steps().collect();
        let id = self.store.insert_run(
            &NewRun {
                repo,
                number: pr.number,
                head_sha: &pr.head_sha,
                base,
                base_sha: &read.sha,
                pipeline: &text,
                files: &files,
                linked_issues: &linked_issues,
                steps: steps
                    .iter()
                    .map(|step| NewStep {
                        id: step.id.clone(),
                        plugin: step.plugin.clone(),
                        config_hash: step.config_hash(),
                    })
                    .collect(),
            },
            now(),
        )?;
        let started = RunEvent::Started {
            repo: repo.clone(),
            number: pr.number,
            head_sha: pr.head_sha.clone(),
            base: base.clone(),
            base_sha: read.sha,
            steps: steps
                .iter()
                .map(|step| step_info(&pipeline, step))
                .collect(),
            gate: gate_text(&pipeline),
            gate_terms: pipeline.gate_terms().iter().map(GateTerm::from).collect(),
        };
        self.journal.append(id, started)?;
        self.inbox
            .close_pr(&pr_ref(repo, pr.number), Closing::NextRunStarted)?;

        let evidence = Evidence {
            linked_issues,
            diff,
        };
        let mut snapshot = snapshot(repo, pr);
        evidence.fill(&mut snapshot);
        self.active.insert(
            key.clone(),
            Active {
                id,
                repo: repo.clone(),
                number: pr.number,
                head_sha: pr.head_sha.clone(),
                state: RunState {
                    pr: facts(&snapshot, &files),
                    ..RunState::default()
                },
                pipeline,
                gate: GateState::Pending,
                snapshot: Some(snapshot),
                files,
                evidence,
                outcomes: HashMap::new(),
                reasons: HashMap::new(),
                attempts: HashMap::new(),
                running: HashMap::new(),
                interrupted: BTreeMap::new(),
                rerun: HashSet::new(),
                merged: false,
                merge_epoch: 0,
            },
        );
        for (step, waiver) in self.store.waivers(repo, pr.number, &pr.head_sha)? {
            if self.active[&key].pipeline.step(&step).is_some() {
                self.record_waiver(&key, &step, waiver)?;
            }
        }
        self.publish(repo, pr.number)?;
        self.advance(&key)
    }

    /// Counts `step`'s Verdict as pass in the Run from now on.
    fn record_waiver(&mut self, key: &PrKey, step: &str, waiver: Waiver) -> Result<(), StoreError> {
        let run = self
            .active
            .get_mut(key)
            .expect("only active Runs take Waivers");
        run.state.waived.insert(step.to_owned());
        self.journal.append(
            run.id,
            RunEvent::StepWaived {
                step: step.to_owned(),
                waiver,
            },
        )?;
        Ok(())
    }

    /// Settles the Steps that too many restarts in a row interrupted, once
    /// the first poll has shown the Run goes on. The rest start again as
    /// the plan allows.
    fn resume(&mut self, key: &PrKey) -> Result<(), StoreError> {
        let run = &self.active[key];
        let looping: Vec<String> = run
            .interrupted
            .iter()
            .filter(|&(_, &restarts)| restarts >= RESTARTS_BEFORE_ERROR)
            .map(|(step, _)| step.clone())
            .collect();
        for step in looping {
            let reason = format!(
                "error(daemon_restart): {RESTARTS_BEFORE_ERROR} daemon restarts in a row \
                 interrupted it"
            );
            self.settle(key, &step, Verdict::Error, Some(reason), Outputs::default())?;
        }
        Ok(())
    }

    fn block(
        &mut self,
        key: &PrKey,
        invalid: Option<Invalid>,
        message: String,
    ) -> Result<(), StoreError> {
        let changed = self.blocked.get(key).map(|old| &old.message) != Some(&message);
        self.blocked
            .insert(key.clone(), Blocked { invalid, message });
        if changed {
            self.publish(&key.0, key.1)?;
        }
        Ok(())
    }

    /// Blocks the PR on a Pipeline that doesn't load, which holds it back
    /// through the base's shared Inbox entry.
    fn block_invalid(
        &mut self,
        key: &PrKey,
        invalid: Invalid,
        message: String,
        reasons: Vec<String>,
    ) -> Result<(), StoreError> {
        let (repo, number) = key;
        let pr = pr_ref(repo, *number);
        let cause = invalid_pipeline(repo, &invalid.base);
        // A PR that moved to another base no longer hits the old one's.
        self.inbox.release(&pr, Closing::NothingHeld, |held| {
            is_invalid_pipeline(held) && *held != cause
        })?;
        let title = format!("Invalid Pipeline on {}", invalid.base);
        let latest = self.store.latest_run(repo, *number)?.map(|run| run.id);
        self.inbox.hold(cause, pr, latest, &title, reasons)?;
        self.block(key, Some(invalid), message)
    }

    /// Lets the PR's next Run start. A PR an invalid Pipeline held back
    /// leaves that cause's entry, which closes as `how` if it was the last.
    /// The entry may come from before a restart, which `blocked` doesn't
    /// remember, so the Inbox is asked either way.
    fn unblock(&mut self, key: &PrKey, how: Closing) -> Result<(), StoreError> {
        let (repo, number) = key;
        self.inbox
            .release(&pr_ref(repo, *number), how, is_invalid_pipeline)?;
        if self.blocked.remove(key).is_some() {
            self.publish(repo, *number)?;
        }
        Ok(())
    }

    /// Does what the plan says: queues the Steps that may start, skips the
    /// ones whose Condition is false, records the Gate, and ends the Run
    /// once every Step has settled. [`Engine::schedule`] starts what's
    /// queued.
    fn advance(&mut self, key: &PrKey) -> Result<(), StoreError> {
        let Some(run) = self.active.get(key) else {
            return Ok(());
        };
        if run.snapshot.is_none() {
            return Ok(());
        }
        let id = run.id;
        // A Step that settles as it starts, reused or unable to spawn, needs
        // another plan for the Steps after it. The rest wait in the queue.
        let mut starts = HashSet::new();
        let mut replan = true;
        while replan {
            replan = false;
            let run = &self.active[key];
            let plan = run.pipeline.plan(&run.state);
            for (step, decision) in plan.decisions {
                match decision {
                    Decision::Start => {
                        if self.reuse(key, &step)? {
                            replan = true;
                            continue;
                        }
                        let entry = (id, step.clone());
                        if !self.ready.contains(&entry) {
                            self.ready.push_back(entry);
                        }
                        starts.insert(step);
                    }
                    Decision::Skip(reason) => self.settle(
                        key,
                        &step,
                        Verdict::Skipped,
                        Some(reason.to_string()),
                        Outputs::default(),
                    )?,
                    Decision::Wait => {}
                }
            }
        }
        // A queued Step the plan no longer starts, say because a label
        // change turned its Condition false, leaves the queue.
        self.ready
            .retain(|(run, step)| *run != id || starts.contains(step));

        let run = self.active.get_mut(key).expect("checked above");
        let gate = run.pipeline.gate(&run.state);
        if gate != run.gate {
            run.gate = gate;
            let id = run.id;
            self.store.set_gate(id, gate)?;
            self.journal.append(id, RunEvent::Gate { state: gate })?;
            self.publish(&key.0, key.1)?;
        }

        let run = &self.active[key];
        let settled = run
            .pipeline
            .steps()
            .all(|step| matches!(run.state.steps.get(&step.id), Some(StepState::Settled(_))));
        if settled {
            let reason = end_reason(run);
            let run = self.active.remove(key).expect("checked above");
            return self.end_run(run, reason);
        }
        // An errored Step holds up a Run that goes on, until the developer
        // retries it or the Run ends.
        let pr = pr_ref(&key.0, key.1);
        for step in run.pipeline.steps() {
            if run.state.steps.get(&step.id) == Some(&StepState::Settled(Verdict::Error)) {
                let reason = run.reasons.get(&step.id).map_or("error", String::as_str);
                // A missing Secret's shared entry already asks for it.
                if held_by_cause(reason) {
                    continue;
                }
                self.inbox
                    .raise_step_error(run.id, pr.clone(), &step.id, reason)?;
            }
        }
        Ok(())
    }

    /// Starts queued Steps, oldest first, while a slot is free. A Step whose
    /// Plugin is at its own cap waits without holding up the Steps behind
    /// it.
    fn schedule(&mut self) {
        if self.stopped {
            return;
        }
        if let Err(error) = self.try_schedule() {
            eprintln!("slopwatchd: can't start Steps: {error}");
        }
    }

    fn try_schedule(&mut self) -> Result<(), StoreError> {
        while self.working().count() < STEP_CAP {
            let Some(index) = self.ready.iter().position(|(run, step)| {
                let Some(step) = self.run(*run).and_then(|run| run.pipeline.step(step)) else {
                    return true;
                };
                let cap = self
                    .plugins
                    .manifest(&step.plugin)
                    .and_then(|manifest| manifest.concurrency);
                // A cap of 0 would hold the Plugin's Steps forever.
                cap.is_none_or(|cap| {
                    let running = self.working().filter(|plugin| **plugin == step.plugin);
                    running.count() < cap.max(1) as usize
                })
            }) else {
                return Ok(());
            };
            let (run, step) = self.ready.remove(index).expect("found above");
            let Some(key) = self.key_of(run) else {
                continue;
            };
            self.start_step(&key, &step)?;
            if !self.active[&key].running.contains_key(&step) {
                // It settled without a process, so its Run moves on.
                self.advance(&key)?;
            }
        }
        Ok(())
    }

    /// The Plugins of the Step processes that count toward the caps: all
    /// but the ones waiting for the developer, which may wait for days.
    fn working(&self) -> impl Iterator<Item = &String> {
        self.processes
            .iter()
            .filter(|((run, step, attempt), _)| {
                let waiting = self
                    .run(*run)
                    .and_then(|run| run.running.get(step))
                    .is_some_and(|running| {
                        running.attempt == *attempt && running.question == Question::Asked
                    });
                !waiting
            })
            .map(|(_, plugin)| plugin)
    }

    fn key_of(&self, run: RunId) -> Option<PrKey> {
        self.active
            .iter()
            .find(|(_, active)| active.id == run)
            .map(|(key, _)| key.clone())
    }

    fn run(&self, run: RunId) -> Option<&Active> {
        self.active.values().find(|active| active.id == run)
    }

    fn start_step(&mut self, key: &PrKey, step_id: &str) -> Result<(), StoreError> {
        let run = self.active.get_mut(key).expect("only active Runs advance");
        let step = run
            .pipeline
            .step(step_id)
            .expect("the plan names Pipeline Steps");
        let attempt = run.attempts.get(step_id).copied().unwrap_or(0) + 1;
        run.attempts.insert(step_id.to_owned(), attempt);
        let Some((program, args)) = self.plugins.command(&step.plugin) else {
            let reason = format!("Plugin `{}` isn't installed", step.plugin);
            return self.settle(
                key,
                step_id,
                Verdict::Error,
                Some(reason),
                Outputs::default(),
            );
        };

        let manifest = self.plugins.manifest(&step.plugin);
        let handed = match self.secrets.for_step(&step.plugin, manifest.as_ref()) {
            Ok(handed) => handed,
            Err(denied) => {
                let pr = pr_ref(&run.repo, run.number);
                let id = run.id;
                for name in denied.unset {
                    self.inbox.hold(
                        Cause::MissingSecret { name: name.clone() },
                        pr.clone(),
                        Some(id),
                        &format!("Secret `{name}` isn't set"),
                        vec![
                            "Steps whose Plugin requires it error until it's set. Setting it \
                             starts the PRs it holds back again."
                                .to_owned(),
                        ],
                    )?;
                }
                return self.settle(
                    key,
                    step_id,
                    Verdict::Error,
                    Some(denied.reason),
                    Outputs::default(),
                );
            }
        };
        let mask = Mask::new(handed.iter().map(|(_, value)| value.expose()));

        // Taken before the spawn, so a rebuild in between makes the
        // Outcome harder to reuse, never easier.
        let version = self.plugins.version(&step.plugin);
        let dir = step_dir(&self.data_dir, run.id, step_id, attempt);
        let log_key = LogKey {
            run: run.id,
            step: step_id.to_owned(),
            attempt,
        };
        let log_dir = log_dir(&self.data_dir, &log_key);
        let logs = self.logs.clone();
        let start = ToStep::Start(Start {
            run: run.id,
            step: step_id.to_owned(),
            config: step.config.clone(),
            snapshot: run
                .snapshot
                .clone()
                .expect("Runs advance only with a snapshot"),
            upstream: upstream(&run.pipeline, step_id)
                .into_iter()
                .filter_map(|id| Some((id.clone(), run.outcomes.get(&id)?.clone())))
                .collect(),
        });
        let mut env = step_env(run.id, step_id, &self.step_path);
        env.extend(
            handed
                .iter()
                .map(|(name, value)| (name.clone(), value.expose().to_owned())),
        );
        let limits = limits(step, manifest.as_ref());
        let plugin = step.plugin.clone();
        let reports = self.reports.clone();
        let (id, name) = (run.id, step_id.to_owned());
        let spawned = std::fs::create_dir_all(&dir).and_then(|()| {
            let log = log::Writer::create(log_dir, log_key, log::Limits::default(), logs)?
                .with_mask(mask.clone());
            process::spawn(
                Spawn {
                    program,
                    args,
                    dir,
                    env,
                    log,
                    start,
                    limits,
                    mask,
                },
                move |report| {
                    let _ = reports.send(Input::Step(StepReport {
                        run: id,
                        step: name.clone(),
                        attempt,
                        report,
                    }));
                },
            )
        });
        let handle = match spawned {
            Ok(handle) => handle,
            Err(error) => {
                let reason = format!("can't start the Step: {error}");
                return self.settle(
                    key,
                    step_id,
                    Verdict::Error,
                    Some(reason),
                    Outputs::default(),
                );
            }
        };

        self.processes
            .insert((run.id, step_id.to_owned(), attempt), plugin);
        self.store.put_step(
            run.id,
            &StepRow {
                step: step_id.to_owned(),
                state: StepRowState::Running {
                    pgid: handle.pgid,
                    started_us: handle.started_us,
                    sid: handle.sid,
                },
                attempt,
            },
        )?;
        run.interrupted.remove(step_id);
        if let Some(version) = &version {
            self.store.set_plugin_version(run.id, step_id, version)?;
        }
        run.state
            .steps
            .insert(step_id.to_owned(), StepState::Running);
        run.running.insert(
            step_id.to_owned(),
            Running {
                attempt,
                handle,
                reported: false,
                awaiting: HashSet::new(),
                question: Question::NotAsked,
            },
        );
        self.journal.append(
            run.id,
            RunEvent::StepStarted {
                step: step_id.to_owned(),
                attempt,
            },
        )?;
        Ok(())
    }

    /// Settles the Step with an earlier same-SHA Run's Outcome under the
    /// same reuse key, if there is one it may take (ADR 0007). Returns
    /// whether it did.
    fn reuse(&mut self, key: &PrKey, step_id: &str) -> Result<bool, StoreError> {
        let run = &self.active[key];
        if run.rerun.contains(step_id) {
            return Ok(false);
        }
        let step = run
            .pipeline
            .step(step_id)
            .expect("the plan names Pipeline Steps");
        let Some(version) = self.plugins.version(&step.plugin) else {
            return Ok(false);
        };
        let reuse_key = step.reuse_key(&run.head_sha, &version);
        // A waived Step keeps the Outcome it was waived on, even an error,
        // so the Waiver doesn't run it again.
        let waived = run.state.waived.contains(step_id);
        let Some(reused) = self
            .store
            .reusable_outcome(&run.repo, run.number, &reuse_key, run.id, waived)?
        else {
            return Ok(false);
        };
        let id = run.id;
        self.store.put_reused_step(id, &reuse_key, &reused)?;
        self.record_settled(
            key,
            step_id,
            reused.verdict,
            reused.reason,
            reused.outputs,
            Some(reused.run),
        )?;
        Ok(true)
    }

    /// Records a Step's Verdict. Its process, if it still runs, is
    /// cancelled when its handle drops.
    fn settle(
        &mut self,
        key: &PrKey,
        step: &str,
        verdict: Verdict,
        reason: Option<String>,
        outputs: Outputs,
    ) -> Result<(), StoreError> {
        let run = &self.active[key];
        self.store.put_step(
            run.id,
            &StepRow {
                step: step.to_owned(),
                state: StepRowState::Settled {
                    verdict,
                    reason: reason.clone(),
                    outputs: outputs.clone(),
                },
                attempt: run.attempts.get(step).copied().unwrap_or(0),
            },
        )?;
        self.record_settled(key, step, verdict, reason, outputs, None)
    }

    /// Puts a Verdict the store already has into the Run and its journal.
    fn record_settled(
        &mut self,
        key: &PrKey,
        step: &str,
        verdict: Verdict,
        reason: Option<String>,
        outputs: Outputs,
        reused_from: Option<RunId>,
    ) -> Result<(), StoreError> {
        let run = self
            .active
            .get_mut(key)
            .expect("only active Runs settle Steps");
        run.state
            .steps
            .insert(step.to_owned(), StepState::Settled(verdict));
        run.outcomes.insert(
            step.to_owned(),
            Outcome {
                verdict,
                outputs: outputs.clone(),
            },
        );
        match &reason {
            Some(reason) => run.reasons.insert(step.to_owned(), reason.clone()),
            None => run.reasons.remove(step),
        };
        let mut closing = Closing::StepSettled;
        if let Some(running) = run.running.get_mut(step) {
            running.reported = true;
            if let Question::Answered(how) = &running.question {
                closing = how.clone();
            }
        }
        run.interrupted.remove(step);
        self.inbox.close_human(run.id, step, closing)?;
        self.journal.append(
            run.id,
            RunEvent::StepSettled {
                step: step.to_owned(),
                verdict,
                reason,
                outputs,
                reused_from,
            },
        )?;
        Ok(())
    }

    fn on_report(&mut self, report: StepReport) {
        if self.stopped {
            return;
        }
        if let Err(error) = self.try_on_report(report) {
            eprintln!("slopwatchd: can't record a Step report: {error}");
        }
    }

    fn try_on_report(&mut self, report: StepReport) -> Result<(), StoreError> {
        if let Report::Message(FromStep::Effect { id, effect }) = report.report {
            return self.on_effect(report.run, &report.step, report.attempt, id, effect);
        }
        if matches!(report.report, Report::Exited(_)) {
            self.processes
                .remove(&(report.run, report.step.clone(), report.attempt));
            let dir = step_dir(&self.data_dir, report.run, &report.step, report.attempt);
            let _ = std::fs::remove_dir_all(dir);
        }
        let Some(key) = self.key_of(report.run) else {
            // The Run already ended.
            return Ok(());
        };
        let run = &self.active[&key];
        let Some(running) = run.running.get(&report.step) else {
            return Ok(());
        };
        if running.attempt != report.attempt {
            return Ok(());
        }
        let reported = running.reported;
        let step = report.step;
        match report.report {
            Report::Message(FromStep::Outcome(outcome)) if !reported => {
                if matches!(
                    outcome.verdict,
                    Verdict::Pass | Verdict::Fail | Verdict::Inconclusive
                ) {
                    self.settle(&key, &step, outcome.verdict, None, outcome.outputs)?;
                } else {
                    let reason = format!(
                        "error(protocol): reported `{}`, a Verdict only the daemon assigns",
                        outcome.verdict
                    );
                    self.settle(
                        &key,
                        &step,
                        Verdict::Error,
                        Some(reason),
                        Outputs::default(),
                    )?;
                }
            }
            Report::ProtocolError(message) if !reported => {
                self.protocol_error(&key, &step, &message)?;
            }
            Report::Tripped(tripped) if !reported => {
                let reason = match tripped {
                    Tripped::Timeout(after) => {
                        format!("error(timeout): ran past its {} timeout", duration(after))
                    }
                    Tripped::Stall(after) => {
                        format!("error(stall): wrote nothing for {}", duration(after))
                    }
                };
                self.settle(
                    &key,
                    &step,
                    Verdict::Error,
                    Some(reason),
                    Outputs::default(),
                )?;
            }
            Report::Exited(code) => {
                let run = self.active.get_mut(&key).expect("found above");
                run.running.remove(&step);
                if !reported {
                    let reason = match code {
                        Some(code) => {
                            format!("error(crash): exited with status {code} before reporting")
                        }
                        None => "error(crash): killed by a signal before reporting".to_owned(),
                    };
                    self.settle(
                        &key,
                        &step,
                        Verdict::Error,
                        Some(reason),
                        Outputs::default(),
                    )?;
                }
            }
            Report::Message(FromStep::Ask { prompt }) if !reported => {
                self.on_ask(&key, &step, &prompt)?;
            }
            // Usage counts even after a cancel: the call was made.
            Report::Message(FromStep::Usage(usage)) => {
                self.journal
                    .append(report.run, RunEvent::StepUsage { step, usage })?;
                return Ok(());
            }
            Report::Message(FromStep::Progress {
                message: Some(message),
            }) if !reported => {
                self.journal
                    .append(report.run, RunEvent::StepProgress { step, message })?;
                return Ok(());
            }
            // The process task keeps the stall watchdog on heartbeats, and
            // anything after the Outcome is ignored.
            Report::Message(_) | Report::ProtocolError(_) | Report::Tripped(_) => return Ok(()),
        }
        self.advance(&key)
    }

    /// Settles a Step that broke the Step protocol as `error(protocol)` and
    /// cancels its process.
    fn protocol_error(&mut self, key: &PrKey, step: &str, message: &str) -> Result<(), StoreError> {
        let reason = format!("error(protocol): {message}");
        self.settle(key, step, Verdict::Error, Some(reason), Outputs::default())?;
        if let Some(running) = self.active[key].running.get(step) {
            running.handle.cancel();
        }
        Ok(())
    }

    /// A Human Step asked the developer, so it goes in the Inbox and stops
    /// counting toward the Step caps. A Step that got this far didn't bring
    /// the daemon down, so the restarts that interrupted it stop counting.
    fn on_ask(&mut self, key: &PrKey, step: &str, prompt: &str) -> Result<(), StoreError> {
        let run = self
            .active
            .get_mut(key)
            .expect("only active Runs get reports");
        let plugin = &run
            .pipeline
            .step(step)
            .expect("running Steps are Pipeline Steps")
            .plugin;
        if plugin != human::ID {
            let message = format!("asked the developer, which only `{}` may", human::ID);
            return self.protocol_error(key, step, &message);
        }
        let running = run.running.get_mut(step).expect("the report matched it");
        if running.question != Question::NotAsked {
            return Ok(());
        }
        running.question = Question::Asked;
        let id = run.id;
        self.store.clear_restarts(id, step)?;
        self.inbox.ask(id, pr_ref(&key.0, key.1), step, prompt)
    }

    /// The developer answers the Human Step `step` in Run `id`. The Step
    /// hears the answer and reports its Outcome, which closes the entry.
    fn answer_step(
        &mut self,
        id: RunId,
        step: &str,
        answer: Answer,
        note: Option<String>,
        actor: &Actor,
    ) -> Result<(), RunError> {
        let key = self.going(id)?;
        let run = self.active.get_mut(&key).expect("found above");
        if run.pipeline.step(step).is_none() {
            return Err(RunError::NotFound(format!("Run {id} has no Step `{step}`")));
        }
        if run.interrupted.contains_key(step) {
            return Err(RunError::Invalid(format!(
                "Step `{step}` is starting again after a daemon restart. Answer once it asks \
                 again."
            )));
        }
        let Some(running) = run
            .running
            .get_mut(step)
            .filter(|running| running.question == Question::Asked && !running.reported)
        else {
            return Err(RunError::Invalid(format!(
                "Step `{step}` isn't waiting for an answer"
            )));
        };
        let note = note
            .map(|note| note.trim().to_owned())
            .filter(|note| !note.is_empty());
        running.handle.send(ToStep::Answer {
            answer,
            note: note.clone(),
            actor: actor.clone(),
        });
        running.question = Question::Answered(Closing::Answered {
            action: answer.to_string(),
            actor: actor.clone(),
            note,
        });
        Ok(())
    }

    /// Ends `run`. Its running Steps, and the ones a restart interrupted,
    /// are cancelled: their Verdict is cancelled whatever they report
    /// afterwards.
    fn end_run(&mut self, mut run: Active, reason: EndReason) -> Result<(), StoreError> {
        self.ready.retain(|(id, _)| *id != run.id);
        let still_running: Vec<String> = run
            .running
            .iter()
            .filter(|(_, running)| !running.reported)
            .map(|(step, _)| step.clone())
            .chain(run.interrupted.keys().cloned())
            .collect();
        for running in run.running.values() {
            running.handle.cancel();
        }
        for step in still_running {
            let attempt = run.attempts.get(&step).copied().unwrap_or(0);
            run.state
                .steps
                .insert(step.clone(), StepState::Settled(Verdict::Cancelled));
            self.store.put_step(
                run.id,
                &StepRow {
                    step: step.clone(),
                    state: StepRowState::Settled {
                        verdict: Verdict::Cancelled,
                        reason: Some(format!("the Run ended {reason}")),
                        outputs: Outputs::default(),
                    },
                    attempt,
                },
            )?;
            self.journal.append(
                run.id,
                RunEvent::StepSettled {
                    step,
                    verdict: Verdict::Cancelled,
                    reason: Some(format!("the Run ended {reason}")),
                    outputs: Outputs::default(),
                    reused_from: None,
                },
            )?;
        }
        self.store.end_run(run.id, reason, now())?;
        let waived = reason == EndReason::Shippable && run.pipeline.passes_by_waiver(&run.state);
        if waived {
            self.store.mark_waived(run.id)?;
        }
        self.journal
            .append(run.id, RunEvent::Ended { reason, waived })?;
        // A cancelled Step's process may take a moment to go, and its
        // directory with it; the Run's own directory goes now.
        let _ = std::fs::remove_dir_all(run_dir(&self.data_dir, run.id));
        // No Run judges a closed PR again, so its diff goes too.
        if matches!(reason, EndReason::Closed | EndReason::Merged) {
            let _ = std::fs::remove_dir_all(pr_diffs(&self.data_dir, &run.repo, run.number));
        }
        self.inbox.run_ended(run.id)?;
        // A Run the developer cancelled, or one a push or close ended,
        // needs nothing from them.
        // Nor does one whose every failure a shared cause's entry explains,
        // or one their rejection ended.
        if reason == EndReason::NotShippable {
            let raised = match couldnt_merge(&run) {
                Some(reasons) => Some((COULDNT_MERGE, reasons)),
                None => not_shippable(&run).map(|reasons| (NOT_SHIPPABLE, reasons)),
            };
            if let Some((title, reasons)) = raised {
                self.inbox
                    .raise_pr(pr_ref(&run.repo, run.number), run.id, title, reasons)?;
            }
        }
        if reason == EndReason::Shippable {
            let title = self.watching.title(&run.repo, run.number);
            self.notifications.shippable(
                run.id,
                pr_ref(&run.repo, run.number),
                title.as_deref(),
            )?;
        }
        self.publish(&run.repo, run.number)
    }

    /// The developer cancels a Run that's still going.
    fn cancel(&mut self, id: RunId) -> Result<(), RunError> {
        let key = self.going(id)?;
        let run = self.active.remove(&key).expect("found above");
        Ok(self.end_run(run, EndReason::Cancelled)?)
    }

    /// The developer retries an errored Step: it and every Step after it
    /// go back to pending, and the plan starts them again in this Run.
    /// The Steps after it that still run are cancelled first, since what
    /// they read is about to change.
    fn retry(&mut self, id: RunId, step: &str, actor: &Actor) -> Result<(), RunError> {
        let key = self.going(id)?;
        let run = &self.active[&key];
        if run.pipeline.step(step).is_none() {
            return Err(RunError::NotFound(format!("Run {id} has no Step `{step}`")));
        }
        if run.state.steps.get(step) != Some(&StepState::Settled(Verdict::Error)) {
            return Err(RunError::Invalid(format!(
                "Step `{step}` didn't error, and only an errored Step can be retried"
            )));
        }
        self.rerun_step(&key, step, Some((&format!("retry `{step}`"), actor)))?;
        Ok(self.advance(&key)?)
    }

    /// Puts `step` and every Step after it back to pending in the Run going
    /// on `key`, so the plan starts them again. `answer` closes their Run
    /// entries as answered by that action and actor.
    fn rerun_step(
        &mut self,
        key: &PrKey,
        step: &str,
        answer: Option<(&str, &Actor)>,
    ) -> Result<(), StoreError> {
        let run = &self.active[key];
        let id = run.id;
        let dependents = dependents(&run.pipeline, step);
        for reset in std::iter::once(step).chain(dependents.iter().map(String::as_str)) {
            self.reset(key, reset)?;
        }
        if let Some((action, actor)) = answer {
            let answered: Vec<String> = std::iter::once(step.to_owned())
                .chain(dependents.iter().cloned())
                .collect();
            self.inbox.answer(id, &answered, action, actor)?;
        }
        self.journal.append(
            id,
            RunEvent::StepRetried {
                step: step.to_owned(),
                dependents,
            },
        )?;
        Ok(())
    }

    /// The developer set the Secret `name`. Its Inbox entry clears, and
    /// every PR it held starts again: a Run still going reruns the Steps
    /// that missed it, and an ended latest Run gets a same-SHA Run.
    fn secret_set(&mut self, name: &str) -> Result<(), StoreError> {
        // Until GitHub has answered since the start, no held PR can start
        // again, so the entry stays and the first sync clears it.
        if !self.watching.fresh() {
            return Ok(());
        }
        let cause = Cause::MissingSecret {
            name: name.to_owned(),
        };
        let held = self.inbox.held_by(&cause);
        self.inbox.clear(cause)?;
        for pr in held {
            let key = (pr.repo.clone(), pr.number);
            match self.restart_held(&key, name) {
                Ok(()) => {}
                Err(RunError::Store(error)) => return Err(error),
                Err(RunError::NotFound(why) | RunError::Invalid(why)) => {
                    eprintln!("slopwatchd: {pr} stays as it is after `{name}` was set: {why}");
                }
            }
        }
        Ok(())
    }

    fn restart_held(&mut self, key: &PrKey, name: &str) -> Result<(), RunError> {
        if let Some(run) = self.active.get(key) {
            let missed: Vec<String> = run
                .pipeline
                .ordered_steps()
                .filter(|step| {
                    run.state.steps.get(&step.id) == Some(&StepState::Settled(Verdict::Error))
                        && run
                            .reasons
                            .get(&step.id)
                            .is_some_and(|reason| missing_secret(reason, name))
                })
                .map(|step| step.id.clone())
                .collect();
            for step in missed {
                // An earlier rerun may have reset it as a dependent.
                if self.active[key].state.steps.get(&step)
                    == Some(&StepState::Settled(Verdict::Error))
                {
                    self.rerun_step(key, &step, None)?;
                }
            }
            return Ok(self.advance(key)?);
        }
        let (repo, number) = key;
        let Some(latest) = self.store.latest_run_id(repo, *number)? else {
            return Ok(());
        };
        let stored = self
            .store
            .run(latest)?
            .ok_or_else(|| RunError::NotFound(format!("No Run {latest}")))?;
        let missed = stored.steps.iter().any(|row| {
            matches!(
                &row.state,
                StepRowState::Settled { verdict: Verdict::Error, reason: Some(reason), .. }
                    if missing_secret(reason, name)
            )
        });
        if !missed {
            return Ok(());
        }
        let pr = self.same_sha_pr(&stored)?;
        Ok(self.start_same_sha(stored, &pr)?)
    }

    /// Clears the entries of missing Secrets that are set by now, as when
    /// the daemon stopped between setting one and restarting what it held.
    fn clear_set_secrets(&mut self) -> Result<(), StoreError> {
        for cause in self.inbox.open_causes() {
            if let Cause::MissingSecret { name } = cause
                && self.secrets.is_set(&name)?
            {
                self.secret_set(&name)?;
            }
        }
        Ok(())
    }

    /// The developer waives `what` in Run `id`. A Run that's still going
    /// counts the Waivers at once. The PR's latest Run, once ended, gets a
    /// new Run on the same SHA that counts them.
    fn waive(&mut self, id: RunId, what: Waive<'_>, waiver: &Waiver) -> Result<(), RunError> {
        if self.stopped {
            return Err(RunError::Invalid("The daemon is restarting".to_owned()));
        }
        if let Some(key) = self.key_of(id) {
            let run = &self.active[&key];
            for step in waivable(&run.pipeline, &run.state, what)? {
                self.store.add_waiver(id, &step, waiver, now())?;
                self.record_waiver(&key, &step, waiver.clone())?;
            }
            return Ok(self.advance(&key)?);
        }

        let stored = self
            .store
            .run(id)?
            .ok_or_else(|| RunError::NotFound(format!("No Run {id}")))?;
        let latest = self.store.latest_run_id(&stored.repo, stored.number)?;
        if latest != Some(id) {
            return Err(RunError::Invalid(format!(
                "Run {id} has ended and a later Run judges the PR. Waive from the latest Run."
            )));
        }
        let pr = self.same_sha_pr(&stored).map_err(|error| match error {
            RunError::Invalid(why) if why.starts_with(HEAD_MOVED) => RunError::Invalid(format!(
                "{why}, and a Waiver covers only the SHA it's made on"
            )),
            error => error,
        })?;
        let pipeline = load(&stored.pipeline, &self.plugins).map_err(|errors| {
            RunError::Invalid(format!(
                "Run {id}'s Pipeline no longer loads: {}",
                join(&errors)
            ))
        })?;
        let state = RunState {
            steps: stored
                .steps
                .iter()
                .filter_map(|row| match row.state {
                    StepRowState::Settled { verdict, .. } => {
                        Some((row.step.clone(), StepState::Settled(verdict)))
                    }
                    _ => None,
                })
                .collect(),
            waived: self
                .store
                .waived_steps(&stored.repo, stored.number, &stored.head_sha)?,
            ..RunState::default()
        };
        for step in waivable(&pipeline, &state, what)? {
            self.store.add_waiver(id, &step, waiver, now())?;
        }
        Ok(self.start_same_sha(stored, &pr)?)
    }

    /// The PR of `stored`, an ended Run, if a new Run on the same SHA may
    /// start: the PR is still watched on that head, and nothing blocks it.
    fn same_sha_pr(&self, stored: &ActiveRun) -> Result<OpenPr, RunError> {
        if !self.watching.fresh() {
            return Err(RunError::Invalid(
                "The daemon hasn't heard from GitHub since it started. Try again after the \
                 next poll."
                    .to_owned(),
            ));
        }
        let pr = self
            .watching
            .prs()
            .into_iter()
            .find(|(repo, pr)| (repo, pr.number) == (&stored.repo, stored.number))
            .map(|(_, pr)| pr)
            .filter(|pr| pr.labeled)
            .ok_or_else(|| {
                RunError::Invalid(format!(
                    "{}#{} isn't a Watched PR any more",
                    stored.repo, stored.number
                ))
            })?;
        if pr.head_sha != stored.head_sha {
            return Err(RunError::Invalid(format!(
                "{HEAD_MOVED} {}",
                stored.head_sha
            )));
        }
        if let Some(blocked) = self.blocked.get(&(stored.repo.clone(), stored.number)) {
            return Err(RunError::Invalid(blocked.message.clone()));
        }
        Ok(pr)
    }

    /// Starts a Run on the same SHA as `stored`, with its Pipeline, so the
    /// same Steps settle the same way. If the base has a newer one, the
    /// next sync compares it as usual.
    fn start_same_sha(&mut self, stored: ActiveRun, pr: &OpenPr) -> Result<(), StoreError> {
        let diff = kept_diff(
            &self.data_dir,
            &stored.repo,
            stored.number,
            &stored.base_sha,
            &stored.head_sha,
        );
        let inputs = Inputs {
            pipeline: PipelineAt {
                sha: stored.base_sha,
                text: Some(stored.pipeline),
            },
            files: stored.files,
            linked_issues: stored.linked_issues,
            diff,
        };
        self.try_start_run(&stored.repo, pr, StartWhen::Always, Ok(inputs))
    }

    /// The Run's key, if the Run is still going.
    fn going(&self, id: RunId) -> Result<PrKey, RunError> {
        if self.stopped {
            return Err(RunError::Invalid("The daemon is restarting".to_owned()));
        }
        match self.key_of(id) {
            Some(key) => Ok(key),
            None if self.store.run_exists(id)? => {
                Err(RunError::Invalid(format!("Run {id} has already ended")))
            }
            None => Err(RunError::NotFound(format!("No Run {id}"))),
        }
    }

    /// Puts a Step back to pending, cancelling its process if it runs.
    fn reset(&mut self, key: &PrKey, step: &str) -> Result<(), StoreError> {
        let run = self.active.get_mut(key).expect("only active Runs reset");
        run.state.steps.remove(step);
        run.outcomes.remove(step);
        run.interrupted.remove(step);
        run.rerun.insert(step.to_owned());
        if let Some(running) = run.running.remove(step) {
            // Its exit, when it comes, matches no running attempt and only
            // frees its slot and directory.
            running.handle.cancel();
        }
        self.store.put_step(
            run.id,
            &StepRow {
                step: step.to_owned(),
                state: StepRowState::Pending,
                attempt: run.attempts.get(step).copied().unwrap_or(0),
            },
        )
    }

    /// Shows the PR's latest Runs and what blocks it on its row.
    fn publish(&self, repo: &RepoName, number: u64) -> Result<(), StoreError> {
        let info = RunInfo {
            runs: self.store.run_summaries(repo, number, HISTORY_ON_ROW)?,
            blocked: self
                .blocked
                .get(&(repo.clone(), number))
                .map(|blocked| blocked.message.clone()),
        };
        self.watching.set_run_info(repo, number, info);
        Ok(())
    }
}

/// Why a Run whose Steps have all settled ends. It's merged once a Merge
/// Step passes, which it does only once GitHub merged the PR. A Merge Step
/// that ran and couldn't land the PR leaves it not shippable, so the
/// developer hears about it. Shippable needs the Gate to pass with no
/// Merge Step, or with one that skipped or ended inconclusive, as on a
/// draft.
fn end_reason(run: &Active) -> EndReason {
    let merge_verdicts = || {
        run.pipeline
            .steps()
            .filter(|step| step.is_merge())
            .filter_map(|step| match run.state.steps.get(&step.id) {
                Some(StepState::Settled(verdict)) => Some(*verdict),
                _ => None,
            })
    };
    if run.merged || merge_verdicts().any(|verdict| verdict == Verdict::Pass) {
        return EndReason::Merged;
    }
    match run.gate {
        GateState::Pass if !merge_verdicts().any(failed_to_land) => EndReason::Shippable,
        _ => EndReason::NotShippable,
    }
}

/// Whether a Merge Step that settled `verdict` tried to land the PR and
/// couldn't. A skip, or an inconclusive draft, didn't try.
fn failed_to_land(verdict: Verdict) -> bool {
    matches!(
        verdict,
        Verdict::Fail | Verdict::Error | Verdict::Cancelled | Verdict::Missing
    )
}

/// Where a Run's Step logs live.
fn run_logs(data_dir: &std::path::Path, run: RunId) -> PathBuf {
    data_dir.join("logs").join(run.to_string())
}

/// One attempt's Step log.
fn log_dir(data_dir: &std::path::Path, key: &LogKey) -> PathBuf {
    run_logs(data_dir, key.run)
        .join(path_segment(&key.step))
        .join(key.attempt.to_string())
}

/// A Step id as one directory name. A Pipeline may name a Step anything,
/// so an id that isn't plain letters, digits, `-` and `_` goes in as hex
/// behind a `~`, which no plain id has. `../x` can't climb out, and `a/1`
/// can't land inside `a`'s directory.
fn path_segment(step: &str) -> String {
    let plain = !step.is_empty()
        && step
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if plain {
        return step.to_owned();
    }
    let mut segment = String::from("~");
    for byte in step.bytes() {
        segment.push_str(&format!("{byte:02x}"));
    }
    segment
}

/// Runs file work off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| Err(std::io::Error::other(error)))
}

/// How a refusal to start a same-SHA Run on a PR whose head moved begins.
const HEAD_MOVED: &str = "The PR's head moved on from";

/// The title of the PR entry a not-shippable Run raises.
const NOT_SHIPPABLE: &str = "Not shippable";

/// Why a Run ended not shippable, one line per Step: every errored Step,
/// and every Step the Gate reads that settled other than pass or skipped.
/// A Human Step the developer rejected isn't news to them, so it isn't
/// listed. `None` when a rejection or a shared cause, such as a missing
/// Secret, explains every failure, since the developer already knows or
/// the cause's own entry asks.
fn not_shippable(run: &Active) -> Option<Vec<String>> {
    let mut reasons = Vec::new();
    let mut all_held = true;
    let mut rejected = false;
    for step in run.pipeline.ordered_steps() {
        let Some(StepState::Settled(verdict)) = run.state.steps.get(&step.id) else {
            continue;
        };
        // A Human Step fails only when the developer rejects it.
        if *verdict == Verdict::Fail && step.plugin == human::ID {
            rejected = true;
            continue;
        }
        let counts = match verdict {
            Verdict::Error => true,
            Verdict::Pass | Verdict::Skipped => false,
            _ => run.pipeline.gate_reads(&step.id),
        };
        if !counts {
            continue;
        }
        let id = &step.id;
        all_held &= run
            .reasons
            .get(id)
            .is_some_and(|reason| held_by_cause(reason));
        reasons.push(match (verdict, run.reasons.get(id)) {
            (Verdict::Error, Some(reason)) => format!("`{id}`: {reason}"),
            (_, Some(reason)) => format!("`{id}`: {verdict} ({reason})"),
            (_, None) => format!("`{id}`: {verdict}"),
        });
    }
    if reasons.is_empty() {
        if rejected {
            return None;
        }
        reasons.push("The Gate didn't pass".to_owned());
        all_held = false;
    }
    (!all_held).then_some(reasons)
}

const COULDNT_MERGE: &str = "Couldn't merge";

/// Why the Merge Steps of a Run whose Gate passed didn't land the PR, such
/// as a conflict with the base, a merge queue that dropped it, or GitHub
/// blocking it past Merge's timeout. `None` when the Gate didn't pass or
/// no Merge Step failed, which [`not_shippable`] explains instead.
fn couldnt_merge(run: &Active) -> Option<Vec<String>> {
    if run.gate != GateState::Pass {
        return None;
    }
    let reasons: Vec<String> = run
        .pipeline
        .ordered_steps()
        .filter(|step| step.is_merge())
        .filter_map(|step| {
            let id = &step.id;
            let Some(StepState::Settled(verdict)) = run.state.steps.get(id) else {
                return None;
            };
            if !failed_to_land(*verdict) {
                return None;
            }
            let finding = run
                .outcomes
                .get(id)
                .and_then(|outcome| outcome.outputs.findings.first())
                .map(|finding| finding.message.clone());
            Some(match finding.or_else(|| run.reasons.get(id).cloned()) {
                Some(why) => format!("`{id}`: {why}"),
                None => format!("`{id}`: {verdict}"),
            })
        })
        .collect();
    (!reasons.is_empty()).then_some(reasons)
}

fn pr_ref(repo: &RepoName, number: u64) -> PrRef {
    PrRef {
        repo: repo.clone(),
        number,
    }
}

fn is_invalid_pipeline(cause: &Cause) -> bool {
    matches!(cause, Cause::InvalidPipeline { .. })
}

fn invalid_pipeline(repo: &RepoName, base: &str) -> Cause {
    Cause::InvalidPipeline {
        repo: repo.clone(),
        base: base.to_owned(),
    }
}

/// Where a Run's Steps get their working directories.
fn run_dir(data_dir: &std::path::Path, run: RunId) -> PathBuf {
    data_dir.join("worktrees").join(run.to_string())
}

/// A Step attempt's working directory, deleted once its process exits.
/// Each attempt gets its own, so a retry never shares one with the
/// cancelled attempt still on its way out.
fn step_dir(data_dir: &std::path::Path, run: RunId, step: &str, attempt: u32) -> PathBuf {
    run_dir(data_dir, run).join(format!("{}.{attempt}", path_segment(step)))
}

/// Every Step after `step`, in Pipeline order: the Steps that need it, or
/// need the Gate while the Gate reads it, and so on down.
fn dependents(pipeline: &Pipeline, step: &str) -> Vec<String> {
    let mut after: HashSet<&str> = HashSet::from([step]);
    let mut gate_after = pipeline.gate_reads(step);
    let mut found = Vec::new();
    // Pipeline order puts the Gate after the Steps it reads, so by the time
    // a Step that needs the Gate comes up, `gate_after` is settled.
    for next in pipeline.ordered_steps() {
        let reads_one = next.needs.iter().any(|need| {
            if need == GATE {
                gate_after
            } else {
                after.contains(need.as_str())
            }
        });
        if reads_one && after.insert(&next.id) {
            gate_after |= pipeline.gate_reads(&next.id);
            found.push(next.id.clone());
        }
    }
    found
}

/// A Step's limits: what the Pipeline sets, or else its Plugin's manifest.
fn limits(step: &Step, manifest: Option<&Manifest>) -> Limits {
    let parsed = |value: Option<&String>| value.and_then(|value| parse_duration(value));
    Limits {
        timeout: step
            .timeout
            .or_else(|| parsed(manifest.and_then(|m| m.timeout.as_ref()))),
        stall_after: step
            .stall_after
            .or_else(|| parsed(manifest.and_then(|m| m.stall_after.as_ref()))),
    }
}

/// A duration the way a Pipeline writes one, such as `90m`.
fn duration(span: Duration) -> String {
    let secs = span.as_secs();
    match secs {
        0 => format!("{}ms", span.as_millis()),
        _ if secs.is_multiple_of(3600) => format!("{}h", secs / 3600),
        _ if secs.is_multiple_of(60) => format!("{}m", secs / 60),
        _ => format!("{secs}s"),
    }
}

/// Every Step upstream of `step`, nearest first.
fn upstream(pipeline: &Pipeline, step: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut next = vec![step.to_owned()];
    while let Some(id) = next.pop() {
        let Some(step) = pipeline.step(&id) else {
            continue;
        };
        for need in &step.needs {
            if pipeline.step(need).is_some() && !found.contains(need) {
                found.push(need.clone());
                next.push(need.clone());
            }
        }
    }
    found
}

fn snapshot(repo: &RepoName, pr: &OpenPr) -> PrSnapshot {
    PrSnapshot {
        repo: repo.clone(),
        number: pr.number,
        title: pr.title.clone(),
        body: pr.detail.body.clone(),
        url: pr.url.clone(),
        author: pr.detail.author.clone(),
        head_sha: pr.head_sha.clone(),
        base: pr.base.clone(),
        draft: pr.draft,
        labels: pr.detail.labels.clone(),
        checks: pr.detail.checks.clone(),
        merge: None,
        diff: None,
        linked_issues: Vec::new(),
    }
}

/// Where the diff of PR `number` at `head_sha` against `base_sha` is kept.
/// It belongs to the PR rather than one Run, so a same-SHA Run finds the
/// diff its predecessor read, and the next new head replaces it.
fn diff_path(
    data_dir: &std::path::Path,
    repo: &RepoName,
    number: u64,
    base_sha: &str,
    head_sha: &str,
) -> PathBuf {
    pr_diffs(data_dir, repo, number).join(format!("{base_sha}-{head_sha}.diff"))
}

fn pr_diffs(data_dir: &std::path::Path, repo: &RepoName, number: u64) -> PathBuf {
    data_dir
        .join("diffs")
        .join(&repo.owner)
        .join(&repo.name)
        .join(number.to_string())
}

/// The kept diff, if there is one.
fn kept_diff(
    data_dir: &std::path::Path,
    repo: &RepoName,
    number: u64,
    base_sha: &str,
    head_sha: &str,
) -> Option<PathBuf> {
    Some(diff_path(data_dir, repo, number, base_sha, head_sha)).filter(|path| path.exists())
}

/// Keeps `diff` at `path`, written whole before it shows up there, and
/// drops the PR's older diffs.
async fn write_diff(path: &std::path::Path, diff: String) -> std::io::Result<()> {
    let dir = path.parent().expect("diff paths have a parent");
    tokio::fs::create_dir_all(dir).await?;
    let partial = path.with_extension("partial");
    tokio::fs::write(&partial, diff).await?;
    tokio::fs::rename(&partial, path).await?;
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.path() != path {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}

/// What Conditions can read about the PR: the snapshot, plus the files
/// the head changes.
fn facts(snapshot: &PrSnapshot, files: &[String]) -> PrFacts {
    PrFacts {
        files: files.to_vec(),
        labels: snapshot.labels.clone(),
        base: snapshot.base.clone(),
        draft: snapshot.draft,
        author: snapshot.author.clone(),
        linked_issue: !snapshot.linked_issues.is_empty(),
    }
}

/// A Step's whole environment: `PATH` with the login shell's merged in,
/// the daemon's `HOME`, and which Run and Step it is. Secrets come later.
fn step_env(run: RunId, step: &str, path: &str) -> Vec<(String, String)> {
    let mut env = vec![("PATH".to_owned(), path.to_owned())];
    env.extend(
        std::env::var("HOME")
            .ok()
            .map(|home| ("HOME".to_owned(), home)),
    );
    env.push(("SLOPWATCH_RUN".into(), run.to_string()));
    env.push(("SLOPWATCH_STEP".into(), step.to_owned()));
    env
}

fn step_info(pipeline: &Pipeline, step: &Step) -> StepInfo {
    StepInfo {
        id: step.id.clone(),
        plugin: step.plugin.clone(),
        needs: step.needs.clone(),
        gated: pipeline.gate_reads(&step.id),
        write: step.is_write(),
        condition: step.when.as_ref().map(ToString::to_string),
    }
}

fn gate_text(pipeline: &Pipeline) -> String {
    let terms: Vec<String> = pipeline
        .gate_terms()
        .iter()
        .map(ToString::to_string)
        .collect();
    format!("[{}]", terms.join(", "))
}

fn join(errors: &[slopwatch_core::LoadError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

/// Now in milliseconds since the epoch, the unit of journal and log
/// timestamps.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_core::{PluginInfo, Resolver, Workspace};

    struct Builtins;

    impl Resolver for Builtins {
        fn library_step(&self, _: &str) -> Option<String> {
            None
        }

        fn plugin(&self, name: &str) -> Option<PluginInfo> {
            Some(PluginInfo {
                workspace: if name == "fix" {
                    Workspace::Write
                } else {
                    Workspace::None
                },
                builtin: true,
            })
        }
    }

    fn pipeline(text: &str) -> Pipeline {
        load(text, &Builtins).unwrap_or_else(|errors| panic!("{}", join(&errors)))
    }

    #[test]
    fn a_steps_dependents_follow_needs_and_the_gate_when_it_reads_the_step() {
        let pipeline = pipeline(
            "version: 1
steps:
  ci: { uses: ci }
  review: { uses: claude, needs: [ci] }
  notes: { uses: jev, needs: [review] }
  lint: { uses: ci }
  fix: { uses: fix, needs: [gate] }
gate: [review, lint]
",
        );

        assert_eq!(dependents(&pipeline, "ci"), ["review", "fix", "notes"]);
        assert_eq!(dependents(&pipeline, "lint"), ["fix"]);
        assert!(
            dependents(&pipeline, "notes").is_empty(),
            "the Gate doesn't read notes"
        );
    }

    #[test]
    fn a_runs_steps_carry_what_the_graph_draws() {
        let pipeline = pipeline(
            "version: 1
steps:
  ci: { uses: ci }
  docs: { uses: jev, when: { files: [\"docs/**\"] } }
  fix: { uses: fix, needs: [gate] }
gate: [ci, { or: [docs, ci] }]
",
        );
        let info = |id| step_info(&pipeline, pipeline.step(id).unwrap());

        assert!(info("ci").gated && !info("ci").write);
        assert_eq!(info("ci").condition, None);
        assert_eq!(
            info("docs").condition.as_deref(),
            Some("{files: [docs/**]}")
        );
        assert!(info("fix").write && !info("fix").gated);
        assert_eq!(
            pipeline
                .gate_terms()
                .iter()
                .map(GateTerm::from)
                .collect::<Vec<_>>(),
            [
                GateTerm::Step {
                    id: "ci".into(),
                    accepts_skipped: false
                },
                GateTerm::AnyOf {
                    terms: vec![
                        GateTerm::Step {
                            id: "docs".into(),
                            accepts_skipped: false
                        },
                        GateTerm::Step {
                            id: "ci".into(),
                            accepts_skipped: false
                        },
                    ]
                },
            ]
        );
    }

    #[test]
    fn limits_come_from_the_pipeline_then_the_manifest() {
        let pipeline = pipeline(
            "version: 1
steps:
  ci: { uses: ci }
  quick: { uses: ci, timeout: 5m, stall_after: 30s }
gate: [ci]
",
        );
        let manifest = crate::plugins::ci::manifest();

        assert_eq!(
            limits(pipeline.step("ci").unwrap(), Some(&manifest)),
            Limits {
                timeout: Some(Duration::from_secs(90 * 60)),
                stall_after: None,
            },
            "CI gets 90 minutes and no stall watchdog"
        );
        assert_eq!(
            limits(pipeline.step("quick").unwrap(), Some(&manifest)),
            Limits {
                timeout: Some(Duration::from_secs(300)),
                stall_after: Some(Duration::from_secs(30)),
            }
        );
    }

    #[test]
    fn durations_read_the_way_a_pipeline_writes_them() {
        assert_eq!(duration(Duration::from_secs(90 * 60)), "90m");
        assert_eq!(duration(Duration::from_secs(7200)), "2h");
        assert_eq!(duration(Duration::from_secs(45)), "45s");
        assert_eq!(duration(Duration::from_millis(300)), "300ms");
    }

    #[test]
    fn a_step_id_is_one_directory_name_that_cant_collide_or_climb() {
        assert_eq!(path_segment("ci"), "ci");
        assert_eq!(path_segment("claude-review_2"), "claude-review_2");
        assert_eq!(path_segment("../x"), "~2e2e2f78");
        assert_eq!(path_segment("a/1"), "~612f31");
        assert_eq!(path_segment(""), "~");
    }
}
