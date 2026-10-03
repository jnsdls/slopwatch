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
//! One lock serializes the engine: a sync and the reports from Step
//! processes take turns. Reading a Pipeline from git happens outside it.

mod journal;
pub mod process;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use slopwatch_core::{
    Decision, EndReason, GATE, GateState, Pipeline, PrFacts, RunState, Step, StepState, Verdict,
    load, parse_duration,
};
use slopwatch_protocol::step::{FromStep, Manifest, Outcome, Outputs, PrSnapshot, Start, ToStep};
use slopwatch_protocol::{RepoName, RunEvent, RunId, StepInfo};
use tokio::sync::mpsc;

use crate::clones::{Clones, PipelineAt};
use crate::github::{GitHub, OpenPr};
use crate::plugins::Plugins;
use crate::shell_env;
use crate::store::{ActiveRun, NewRun, NewStep, StepRow, StepRowState, Store, StoreError};
use crate::watching::{RunInfo, Watching};

use journal::Journal;
pub use journal::Live;
use process::{Limits, Report, Spawn, StepHandle, Tripped};

/// How many of a PR's newest Runs its row carries, for the history chips.
pub const HISTORY_ON_ROW: usize = 20;

/// The most Step processes that run at once, across every Run.
pub const STEP_CAP: usize = 8;

type PrKey = (RepoName, u64);

pub struct Runs {
    engine: tokio::sync::Mutex<Engine>,
    journal: Arc<Journal>,
    github: Arc<dyn GitHub>,
    clones: Clones,
    live: AtomicBool,
}

/// Where Runs keep their files.
pub struct RunsConfig {
    /// Clones under `repos/`, Step directories under `worktrees/`, Step
    /// logs under `logs/`.
    pub data_dir: PathBuf,
    pub plugins: Plugins,
    /// The `PATH` the developer's login shell reports
    /// ([`shell_env::login_shell_path`]). Steps get the daemon's own `PATH`
    /// with this one merged after it. `None` leaves them the daemon's.
    pub login_path: Option<String>,
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
        let mut engine = Engine {
            data_dir: config.data_dir,
            plugins: config.plugins,
            step_path,
            ready: VecDeque::new(),
            processes: HashMap::new(),
            store,
            journal: Arc::clone(&journal),
            watching: Arc::clone(&watching),
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
            stopped: false,
        };
        engine.load()?;
        let runs = Arc::new(Self {
            live: AtomicBool::new(!engine.active.is_empty()),
            engine: tokio::sync::Mutex::new(engine),
            journal,
            github,
            clones,
        });
        let driver = Arc::clone(&runs);
        tokio::spawn(async move {
            while let Some(report) = received.recv().await {
                let mut engine = driver.engine.lock().await;
                engine.on_report(report);
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
        let starts = {
            let mut engine = self.engine.lock().await;
            if !engine.watching.fresh() {
                return;
            }
            let prs = engine.watching.prs();
            let starts = engine.sync(prs);
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

    /// What a Run on the PR's head starts from: the Pipeline at the tip of
    /// its base, and the files the head changes, both from the repo's
    /// clone.
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
            });
        }
        let files = self
            .clones
            .changed_files(repo, &remote, pr.number, &pipeline.sha, &pr.head_sha)
            .await
            .map_err(|error| format!("can't list the files the PR changes: {error}"))?;
        Ok(Inputs { pipeline, files })
    }

    /// Ends a Run that's still going as cancelled.
    pub async fn cancel(&self, run: RunId) -> Result<(), RunError> {
        self.command(|engine| engine.cancel(run)).await
    }

    /// Runs the errored Step `step` again in its Run, with every Step
    /// after it.
    pub async fn retry(&self, run: RunId, step: &str) -> Result<(), RunError> {
        self.command(|engine| engine.retry(run, step)).await
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
    ) -> Result<(Vec<(u64, RunEvent)>, Live), SubscribeError> {
        if !self.journal.has_run(run).map_err(SubscribeError::Store)? {
            return Err(SubscribeError::NotFound(run));
        }
        self.journal
            .subscribe(run, after)
            .map_err(SubscribeError::Store)
    }

    /// The Run's stored events after `after`, for a subscriber that fell
    /// behind the live ones.
    pub fn replay(&self, run: RunId, after: u64) -> Result<Vec<(u64, RunEvent)>, StoreError> {
        self.journal.replay(run, after)
    }
}

struct Engine {
    store: Store,
    plugins: Plugins,
    journal: Arc<Journal>,
    watching: Arc<Watching>,
    data_dir: PathBuf,
    /// The `PATH` every Step gets.
    step_path: String,
    reports: mpsc::UnboundedSender<StepReport>,
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
}

struct Blocked {
    /// The base commit whose Pipeline is invalid, so nothing is tried
    /// again until the base moves. `None` when the read itself failed,
    /// and the next sync tries again.
    invalid_at: Option<String>,
    message: String,
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
    /// Steps the developer's retry reset. They run again rather than take
    /// an earlier Run's Outcome.
    rerun: HashSet<String>,
    outcomes: HashMap<String, Outcome>,
    attempts: HashMap<String, u32>,
    running: HashMap<String, Running>,
    /// Steps a restart killed before they reported, with how many restarts
    /// in a row did. Each starts again or settles once the first poll has
    /// run (ADR 0009).
    interrupted: BTreeMap<String, u32>,
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
                head_sha: stored.head_sha,
                pipeline,
                state: RunState::default(),
                gate: stored.gate,
                snapshot: None,
                files: stored.files,
                outcomes: HashMap::new(),
                attempts: HashMap::new(),
                running: HashMap::new(),
                interrupted: BTreeMap::new(),
                rerun: HashSet::new(),
            };
            for row in stored.steps {
                active.attempts.insert(row.step.clone(), row.attempt);
                match row.state {
                    StepRowState::Settled {
                        verdict, outputs, ..
                    } => {
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
        self.journal
            .append(run.id, RunEvent::Ended { reason: ended })?;
        let _ = std::fs::remove_dir_all(run_dir(&self.data_dir, run.id));
        Ok(())
    }

    /// Ends the Runs the poll says are over, passes PR updates to running
    /// Steps, and returns the PRs that may need a new Run.
    fn sync(&mut self, prs: Vec<(RepoName, OpenPr)>) -> Vec<(RepoName, OpenPr, StartWhen)> {
        if self.stopped {
            return Vec::new();
        }
        self.try_sync(prs).unwrap_or_else(|error| {
            eprintln!("slopwatchd: can't update Runs: {error}");
            Vec::new()
        })
    }

    fn try_sync(
        &mut self,
        prs: Vec<(RepoName, OpenPr)>,
    ) -> Result<Vec<(RepoName, OpenPr, StartWhen)>, StoreError> {
        let open: BTreeMap<PrKey, (RepoName, OpenPr)> = prs
            .into_iter()
            .map(|(repo, pr)| ((repo.clone(), pr.number), (repo, pr)))
            .collect();

        let keys: Vec<PrKey> = self.active.keys().cloned().collect();
        for key in keys {
            let ending = match open.get(&key) {
                None => Some(EndReason::Closed),
                Some((_, pr)) if !pr.labeled => Some(EndReason::Cancelled),
                Some((repo, pr)) if pr.head_sha != self.active[&key].head_sha => {
                    Some(if self.store.pushed_by_slopwatch(repo, &pr.head_sha)? {
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
            let snapshot = snapshot(repo, pr);
            let run = self.active.get_mut(&key).expect("listed above");
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

        let mut starts = Vec::new();
        for (key, (repo, pr)) in &open {
            if !pr.labeled {
                self.blocked.remove(key);
                continue;
            }
            if !pr.base_has_pipeline || self.active.contains_key(key) {
                continue;
            }
            let still_invalid = self
                .blocked
                .get(key)
                .and_then(|blocked| blocked.invalid_at.as_ref())
                .is_some_and(|sha| *sha == pr.detail.base_sha);
            if still_invalid {
                continue;
            }
            let newly_watched = !self.watched.contains(key);
            let latest = self.store.latest_run(repo, pr.number)?;
            let when = match latest {
                Some(run) if !newly_watched && run.head_sha == pr.head_sha => {
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
        Ok(starts)
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
                // A Pipeline that was invalid and is now back as it was.
                if self.blocked.remove(&key).is_some() {
                    self.publish(repo, pr.number)?;
                }
                return Ok(());
            }
        }
        let pipeline = match load(&text, &self.plugins) {
            Ok(pipeline) => pipeline,
            Err(errors) => {
                let message = format!("The Pipeline on {base} is invalid: {}", join(&errors));
                return self.block(&key, Some(pr.detail.base_sha.clone()), message);
            }
        };
        self.blocked.remove(&key);

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
                .map(|step| StepInfo {
                    id: step.id.clone(),
                    plugin: step.plugin.clone(),
                    needs: step.needs.clone(),
                    gated: pipeline.gate_reads(&step.id),
                })
                .collect(),
            gate: gate_text(&pipeline),
        };
        self.journal.append(id, started)?;

        let snapshot = snapshot(repo, pr);
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
                outcomes: HashMap::new(),
                attempts: HashMap::new(),
                running: HashMap::new(),
                interrupted: BTreeMap::new(),
                rerun: HashSet::new(),
            },
        );
        self.publish(repo, pr.number)?;
        self.advance(&key)
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
        base_sha: Option<String>,
        message: String,
    ) -> Result<(), StoreError> {
        let changed = self.blocked.get(key).map(|old| &old.message) != Some(&message);
        self.blocked.insert(
            key.clone(),
            Blocked {
                invalid_at: base_sha,
                message,
            },
        );
        if changed {
            self.publish(&key.0, key.1)?;
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
            let reason = match run.gate {
                GateState::Pass => EndReason::Shippable,
                GateState::Fail | GateState::Pending => EndReason::NotShippable,
            };
            let run = self.active.remove(key).expect("checked above");
            self.end_run(run, reason)?;
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
        while self.processes.len() < STEP_CAP {
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
                    let running = self.processes.values().filter(|p| **p == step.plugin);
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

        // Taken before the spawn, so a rebuild in between makes the
        // Outcome harder to reuse, never easier.
        let version = self.plugins.version(&step.plugin);
        let dir = step_dir(&self.data_dir, run.id, step_id, attempt);
        let log = self
            .data_dir
            .join("logs")
            .join(run.id.to_string())
            .join(format!("{step_id}.{attempt}.log"));
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
        let env = step_env(run.id, step_id, &self.step_path);
        let limits = limits(step, self.plugins.manifest(&step.plugin).as_ref());
        let plugin = step.plugin.clone();
        let reports = self.reports.clone();
        let (id, name) = (run.id, step_id.to_owned());
        let spawned = std::fs::create_dir_all(&dir).and_then(|()| {
            process::spawn(
                Spawn {
                    program,
                    args,
                    dir,
                    env,
                    log,
                    start,
                    limits,
                },
                move |report| {
                    let _ = reports.send(StepReport {
                        run: id,
                        step: name.clone(),
                        attempt,
                        report,
                    });
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
            },
        );
        self.journal.append(
            run.id,
            RunEvent::StepStarted {
                step: step_id.to_owned(),
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
        let Some(reused) = self
            .store
            .reusable_outcome(&run.repo, run.number, &reuse_key, run.id)?
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
        if let Some(running) = run.running.get_mut(step) {
            running.reported = true;
        }
        run.interrupted.remove(step);
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
                let reason = format!("error(protocol): {message}");
                self.settle(
                    &key,
                    &step,
                    Verdict::Error,
                    Some(reason),
                    Outputs::default(),
                )?;
                if let Some(running) = self.active[&key].running.get(&step) {
                    running.handle.cancel();
                }
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
            // The process task keeps the stall watchdog on heartbeats, and
            // anything after the Outcome is ignored.
            Report::Message(_) | Report::ProtocolError(_) | Report::Tripped(_) => return Ok(()),
        }
        self.advance(&key)
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
        self.journal.append(run.id, RunEvent::Ended { reason })?;
        // A cancelled Step's process may take a moment to go, and its
        // directory with it; the Run's own directory goes now.
        let _ = std::fs::remove_dir_all(run_dir(&self.data_dir, run.id));
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
    fn retry(&mut self, id: RunId, step: &str) -> Result<(), RunError> {
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
        let dependents = dependents(&run.pipeline, step);
        for reset in std::iter::once(step).chain(dependents.iter().map(String::as_str)) {
            self.reset(&key, reset)?;
        }
        self.journal.append(
            id,
            RunEvent::StepRetried {
                step: step.to_owned(),
                dependents,
            },
        )?;
        Ok(self.advance(&key)?)
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

/// Where a Run's Steps get their working directories.
fn run_dir(data_dir: &std::path::Path, run: RunId) -> PathBuf {
    data_dir.join("worktrees").join(run.to_string())
}

/// A Step attempt's working directory, deleted once its process exits.
/// Each attempt gets its own, so a retry never shares one with the
/// cancelled attempt still on its way out.
fn step_dir(data_dir: &std::path::Path, run: RunId, step: &str, attempt: u32) -> PathBuf {
    run_dir(data_dir, run).join(format!("{step}.{attempt}"))
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
    }
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

fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
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
}
