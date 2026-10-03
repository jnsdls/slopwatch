//! Runs: one execution of a Pipeline against one head SHA of a Watched PR.
//!
//! [`Runs::sync`] follows what the last poll saw. A Watched PR whose base
//! has a Pipeline gets a Run on its head SHA. A push ends the Run, as
//! pushed if slopwatch made it and superseded otherwise, and starts the next
//! one (ADR 0001). The Run reads the Pipeline from the PR's root base in
//! the daemon's own clone and records that base SHA (ADR 0007).
//!
//! Within a Run, core's [`Pipeline::plan`] decides what happens next, and
//! the engine carries it out: it spawns each Step that may start as its own
//! process (ADR 0003), marks skips, and ends the Run once every Step has
//! settled. Every change goes to the Run's event journal first, and the
//! `run/<id>` topic replays from it. The store keeps each Step's state, so
//! a restarted daemon picks its Runs back up (ADR 0009).
//!
//! One lock serializes the engine: a sync and the reports from Step
//! processes take turns.

mod journal;
pub mod process;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use slopwatch_core::{EndReason, GateState, Pipeline, PrFacts, RunState, StepState, Verdict, load};
use slopwatch_protocol::step::{FromStep, Outcome, Outputs, PrSnapshot, Start, ToStep};
use slopwatch_protocol::{RepoName, RunEvent, RunId, StepInfo};
use tokio::sync::mpsc;

use crate::clones::Clones;
use crate::github::{GitHub, OpenPr};
use crate::plugins::Plugins;
use crate::store::{NewRun, StepRow, StepRowState, Store, StoreError};
use crate::watching::{RunInfo, Watching};

use journal::Journal;
pub use journal::Live;
use process::{Report, Spawn, StepHandle};

/// How many of a PR's newest Runs its row carries, for the history chips.
pub const HISTORY_ON_ROW: usize = 20;

type PrKey = (RepoName, u64);

pub struct Runs {
    engine: tokio::sync::Mutex<Engine>,
    journal: Arc<Journal>,
    live: AtomicBool,
}

/// Where Runs keep their files.
pub struct RunsConfig {
    /// Clones under `repos/`, Step directories under `worktrees/`, Step
    /// logs under `logs/`.
    pub data_dir: PathBuf,
    pub plugins: Plugins,
}

#[derive(Debug)]
pub enum SubscribeError {
    NotFound(RunId),
    Store(StoreError),
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
        let mut engine = Engine {
            clones: Clones::new(config.data_dir.join("repos")),
            data_dir: config.data_dir,
            plugins: config.plugins,
            store,
            github,
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
            stopped: false,
        };
        engine.load()?;
        let runs = Arc::new(Self {
            live: AtomicBool::new(!engine.active.is_empty()),
            engine: tokio::sync::Mutex::new(engine),
            journal,
        });
        let driver = Arc::clone(&runs);
        tokio::spawn(async move {
            while let Some(report) = received.recv().await {
                let mut engine = driver.engine.lock().await;
                engine.on_report(report);
                driver
                    .live
                    .store(!engine.active.is_empty(), Ordering::Relaxed);
            }
        });
        Ok(runs)
    }

    /// Brings Runs in line with the PRs as the last poll saw them.
    pub async fn sync(&self) {
        let mut engine = self.engine.lock().await;
        let prs = engine.watching.prs();
        engine.sync(prs).await;
        self.live
            .store(!engine.active.is_empty(), Ordering::Relaxed);
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
    github: Arc<dyn GitHub>,
    clones: Clones,
    plugins: Plugins,
    journal: Arc<Journal>,
    watching: Arc<Watching>,
    data_dir: PathBuf,
    reports: mpsc::UnboundedSender<StepReport>,
    /// The Run going on each PR. A PR has at most one.
    active: BTreeMap<PrKey, Active>,
    /// PRs that were watched at the last sync.
    watched: HashSet<PrKey>,
    /// Why a PR's next Run can't start. With a base SHA, the Pipeline at
    /// that commit is invalid, so nothing is tried again until the base
    /// moves. Without one, the next sync tries again.
    blocked: HashMap<PrKey, (Option<String>, String)>,
    /// The daemon is about to exit: nothing more changes.
    stopped: bool,
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
    outcomes: HashMap<String, Outcome>,
    attempts: HashMap<String, u32>,
    running: HashMap<String, Running>,
}

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
    /// Picks up the Runs that hadn't ended when the daemon last stopped. A
    /// Step that was running starts again from scratch, which isn't a
    /// retry: it never reported.
    fn load(&mut self) -> Result<(), StoreError> {
        for stored in self.store.active_runs()? {
            let pipeline = match load(&stored.pipeline, &self.plugins) {
                Ok(pipeline) => pipeline,
                Err(errors) => {
                    eprintln!(
                        "slopwatchd: Run {} no longer loads its Pipeline ({}), ending it",
                        stored.id,
                        join(&errors)
                    );
                    self.journal.append(
                        stored.id,
                        RunEvent::Ended {
                            reason: EndReason::Cancelled,
                        },
                    )?;
                    self.store.end_run(stored.id, EndReason::Cancelled, now())?;
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
                outcomes: HashMap::new(),
                attempts: HashMap::new(),
                running: HashMap::new(),
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
                    StepRowState::Running { .. } => {
                        self.store.put_step(
                            active.id,
                            &StepRow {
                                state: StepRowState::Pending,
                                ..row
                            },
                        )?;
                    }
                    StepRowState::Pending => {}
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

    async fn sync(&mut self, prs: Vec<(RepoName, OpenPr)>) {
        if self.stopped {
            return;
        }
        if let Err(error) = self.try_sync(prs).await {
            eprintln!("slopwatchd: can't update Runs: {error}");
        }
    }

    async fn try_sync(&mut self, prs: Vec<(RepoName, OpenPr)>) -> Result<(), StoreError> {
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
            run.state.pr = facts(&snapshot);
            run.snapshot = Some(snapshot.clone());
            if first {
                self.advance(&key)?;
            } else {
                for running in run.running.values() {
                    running.handle.send(ToStep::PrUpdated {
                        snapshot: snapshot.clone(),
                    });
                }
            }
        }

        for (key, (repo, pr)) in &open {
            if !pr.labeled {
                self.blocked.remove(key);
                continue;
            }
            if !pr.base_has_pipeline || self.active.contains_key(key) {
                continue;
            }
            let newly_watched = !self.watched.contains(key);
            let latest = self.store.run_summaries(repo, pr.number, 1)?;
            let new_head = latest.first().is_none_or(|run| run.head_sha != pr.head_sha);
            let still_invalid = matches!(
                self.blocked.get(key),
                Some((Some(sha), _)) if *sha == pr.detail.base_sha
            );
            if (newly_watched || new_head) && !still_invalid {
                self.start_run(repo, pr).await?;
            }
        }
        self.watched = open
            .into_iter()
            .filter(|(_, (_, pr))| pr.labeled)
            .map(|(key, _)| key)
            .collect();
        Ok(())
    }

    /// Starts a Run on the PR's head, with the Pipeline from its root
    /// base. A Stack's root base comes with Stacks; until then it's the
    /// PR's own base.
    async fn start_run(&mut self, repo: &RepoName, pr: &OpenPr) -> Result<(), StoreError> {
        let key = (repo.clone(), pr.number);
        let base = &pr.base;
        let read = match self.github.git_remote(repo).await {
            Ok(remote) => self
                .clones
                .pipeline_at(repo, &remote, base)
                .await
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        let read = match read {
            Ok(read) => read,
            Err(error) => {
                let message = format!("Can't read the Pipeline on {base}: {error}");
                return self.block(&key, None, message);
            }
        };
        let Some(text) = read.text else {
            // The poll saw a Pipeline the fetch didn't. The next poll
            // catches up.
            return Ok(());
        };
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
                steps: steps
                    .iter()
                    .map(|step| (step.id.clone(), step.plugin.clone(), step.config_hash()))
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
                    pr: facts(&snapshot),
                    ..RunState::default()
                },
                pipeline,
                gate: GateState::Pending,
                snapshot: Some(snapshot),
                outcomes: HashMap::new(),
                attempts: HashMap::new(),
                running: HashMap::new(),
            },
        );
        self.publish(repo, pr.number)?;
        self.advance(&key)
    }

    fn block(
        &mut self,
        key: &PrKey,
        base_sha: Option<String>,
        message: String,
    ) -> Result<(), StoreError> {
        let changed = self.blocked.get(key).map(|(_, old)| old) != Some(&message);
        self.blocked.insert(key.clone(), (base_sha, message));
        if changed {
            self.publish(&key.0, key.1)?;
        }
        Ok(())
    }

    /// Does what the plan says: starts the Steps that may start, skips the
    /// ones whose Condition is false, records the Gate, and ends the Run
    /// once every Step has settled.
    fn advance(&mut self, key: &PrKey) -> Result<(), StoreError> {
        let Some(run) = self.active.get(key) else {
            return Ok(());
        };
        if run.snapshot.is_none() {
            return Ok(());
        }
        let plan = run.pipeline.plan(&run.state);
        for (step, decision) in plan.decisions {
            match decision {
                slopwatch_core::Decision::Start => self.start_step(key, &step)?,
                slopwatch_core::Decision::Skip(reason) => self.settle(
                    key,
                    &step,
                    Verdict::Skipped,
                    Some(reason.to_string()),
                    Outputs::default(),
                )?,
                slopwatch_core::Decision::Wait => {}
            }
        }

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

        let dir = self
            .data_dir
            .join("worktrees")
            .join(run.id.to_string())
            .join(step_id);
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
        let env = step_env(run.id, step_id);
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

        self.store.put_step(
            run.id,
            &StepRow {
                step: step_id.to_owned(),
                state: StepRowState::Running {
                    pgid: handle.pgid,
                    started_us: handle.started_us,
                },
                attempt,
            },
        )?;
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
        let run = self
            .active
            .get_mut(key)
            .expect("only active Runs settle Steps");
        let attempt = run.attempts.get(step).copied().unwrap_or(0);
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
        self.store.put_step(
            run.id,
            &StepRow {
                step: step.to_owned(),
                state: StepRowState::Settled {
                    verdict,
                    reason: reason.clone(),
                    outputs: outputs.clone(),
                },
                attempt,
            },
        )?;
        self.journal.append(
            run.id,
            RunEvent::StepSettled {
                step: step.to_owned(),
                verdict,
                reason,
                outputs,
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
        let Some(key) = self
            .active
            .iter()
            .find(|(_, run)| run.id == report.run)
            .map(|(key, _)| key.clone())
        else {
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
                // The Step exits after its Outcome. One that lingers gets
                // the cancel path's signals.
                running.handle.cancel();
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
            Report::Exited(code) => {
                let run = self.active.get_mut(&key).expect("found above");
                run.running.remove(&step);
                let _ = std::fs::remove_dir_all(
                    self.data_dir
                        .join("worktrees")
                        .join(run.id.to_string())
                        .join(&step),
                );
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
            // Heartbeats matter once the stall watchdog exists, and
            // anything after the Outcome is ignored.
            Report::Message(_) | Report::ProtocolError(_) => return Ok(()),
        }
        self.advance(&key)
    }

    /// Ends `run`. Its running Steps are cancelled: their Verdict is
    /// cancelled whatever they report afterwards.
    fn end_run(&mut self, mut run: Active, reason: EndReason) -> Result<(), StoreError> {
        let still_running: Vec<String> = run
            .running
            .iter()
            .filter(|(_, running)| !running.reported)
            .map(|(step, _)| step.clone())
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
                },
            )?;
        }
        self.store.end_run(run.id, reason, now())?;
        self.journal.append(run.id, RunEvent::Ended { reason })?;
        // A cancelled Step's process may take a moment to go, and its
        // directory with it; the Run's own directory goes now.
        let _ = std::fs::remove_dir_all(self.data_dir.join("worktrees").join(run.id.to_string()));
        self.publish(&run.repo, run.number)
    }

    /// Shows the PR's latest Runs and what blocks it on its row.
    fn publish(&self, repo: &RepoName, number: u64) -> Result<(), StoreError> {
        let info = RunInfo {
            runs: self.store.run_summaries(repo, number, HISTORY_ON_ROW)?,
            blocked: self
                .blocked
                .get(&(repo.clone(), number))
                .map(|(_, message)| message.clone()),
        };
        self.watching.set_run_info(repo, number, info);
        Ok(())
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

/// What Conditions can read about the PR. The changed files aren't in the
/// snapshot yet, so `files:` Conditions see none.
fn facts(snapshot: &PrSnapshot) -> PrFacts {
    PrFacts {
        files: Vec::new(),
        labels: snapshot.labels.clone(),
        base: snapshot.base.clone(),
        draft: snapshot.draft,
        author: snapshot.author.clone(),
    }
}

/// A Step's whole environment: the daemon's `PATH` and `HOME`, plus which
/// Run and Step it is. Secrets and the login-shell `PATH` come later.
fn step_env(run: RunId, step: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ["PATH", "HOME"]
        .into_iter()
        .filter_map(|name| Some((name.to_owned(), std::env::var(name).ok()?)))
        .collect();
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
