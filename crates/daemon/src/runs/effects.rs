//! Effects: the changes on GitHub that Steps ask for (ADR 0003), and the
//! intent journal that carries them across a crash (ADR 0009).
//!
//! A request is taken only from the running attempt of a Step that hasn't
//! reported, in a Run that's still going. Anything else is dropped, and the
//! drop goes in the Run's events. A Step asking for an Effect its manifest
//! doesn't declare is a protocol error.
//!
//! A taken request gets an intent row before the daemon calls GitHub, and
//! the row gets its result after. The call runs on a task of its own, so a
//! slow GitHub never holds the engine. Requests are idempotent per Run,
//! Step and the Step's own request id: asking again gets the first answer,
//! so a Step respawned after a restart doesn't repeat what it asked for.
//!
//! On the first sync after a start, every intent still open is reconciled:
//! a comment is looked up by the hidden marker it carries, and whatever
//! isn't on GitHub yet is done again if its Run is still going, or dropped
//! if not. Labels, reruns, rebases and merges are redone. A rebase or a
//! merge carries the head SHA its Run judged, so GitHub refuses it once
//! the PR has moved on, and a merge GitHub already made answers merged.

use std::sync::Arc;

use slopwatch_core::GateState;
use slopwatch_protocol::step::{CheckState, Effect, EffectResult, ToStep};
use slopwatch_protocol::{RunEvent, RunId};

use super::{Engine, Input, PrKey};
use crate::github::{GitHub, Merged, WATCH_LABEL};
use crate::store::{EffectRow, NewEffect, StoreError};

/// How many rebase-started Runs in a row a PR gets before the daemon
/// stops rebasing it and asks the developer instead (ADR 0004).
pub(super) const REBASE_STARTED_RUNS: u32 = 3;

/// The hidden line a comment Effect carries, so the daemon can find the
/// comment after a crash before posting it again.
pub(crate) fn marker(intent: i64) -> String {
    format!("<!-- slopwatch:effect={intent} -->")
}

/// An Effect the daemon finished carrying out, on its way back to the
/// Step that asked.
pub(super) struct Finished {
    run: RunId,
    step: String,
    request: String,
    result: EffectResult,
}

/// Which call on GitHub an open intent gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    /// Just recorded: nothing has reached GitHub yet.
    First,
    /// Left open by a crash. `current` says whether its Run is still going
    /// on the head it expected.
    Reconcile { current: bool },
}

impl Engine {
    /// A Step asked for an Effect.
    pub(super) fn on_effect(
        &mut self,
        run: RunId,
        step: &str,
        attempt: u32,
        request: String,
        effect: Effect,
    ) -> Result<(), StoreError> {
        let Some(key) = self.key_of(run) else {
            let reason = "the Run had ended".to_owned();
            return self.record(run, step, effect, EffectResult::Dropped { reason });
        };
        let run = &self.active[&key];
        let taking = run
            .running
            .get(step)
            .is_some_and(|running| running.attempt == attempt && !running.reported);
        if !taking {
            let reason = "the Step had already settled".to_owned();
            return self.answer(
                &key,
                step,
                &request,
                effect,
                EffectResult::Dropped { reason },
            );
        }
        let plugin = &run
            .pipeline
            .step(step)
            .expect("running Steps are Pipeline Steps")
            .plugin;
        let declared = self
            .plugins
            .manifest(plugin)
            .is_some_and(|manifest| manifest.effects.contains(&effect.kind()));
        if !declared {
            let message = format!(
                "requested a `{}` Effect its manifest doesn't declare",
                effect.kind()
            );
            self.protocol_error(&key, step, &message)?;
            return self.advance(&key);
        }

        if let Some(earlier) = self.store.effect_by_request(run.id, step, &request)? {
            if earlier.effect != effect {
                let reason = format!("`{request}` already named a different Effect");
                return self.answer(
                    &key,
                    step,
                    &request,
                    effect,
                    EffectResult::Refused { reason },
                );
            }
            match earlier.result {
                // The Run's events have it already.
                Some(result) => self.tell(&key, step, &request, result),
                None => self.await_result(&key, step, request),
            }
            return Ok(());
        }
        if let Some(reason) = self.refusal(&key, &effect)? {
            return self.answer(
                &key,
                step,
                &request,
                effect,
                EffectResult::Refused { reason },
            );
        }

        let row = self.store.insert_effect(&NewEffect {
            run: run.id,
            step,
            request: &request,
            repo: &run.repo,
            number: run.number,
            head_sha: &run.head_sha,
            effect: &effect,
        })?;
        self.await_result(&key, step, request);
        self.carry_out(row, Call::First);
        Ok(())
    }

    /// Why the daemon won't carry out `effect` for the Run, if it won't.
    fn refusal(&self, key: &PrKey, effect: &Effect) -> Result<Option<String>, StoreError> {
        let run = &self.active[key];
        Ok(match effect {
            Effect::Label { name, .. } if name.eq_ignore_ascii_case(WATCH_LABEL) => Some(format!(
                "the `{WATCH_LABEL}` label is the developer's to change"
            )),
            Effect::Rerun { check, job } => {
                let failed = run.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.checks.runs.iter().any(|c| {
                        c.name == *check
                            && c.actions_job == Some(*job)
                            && c.state == CheckState::Failure
                    })
                });
                if !failed {
                    Some(format!(
                        "`{check}` isn't a failed GitHub Actions job on the head commit"
                    ))
                } else if self.store.reran(&run.repo, &run.head_sha, check)? {
                    Some(format!("`{check}` was already rerun once on this SHA"))
                } else {
                    None
                }
            }
            Effect::Rebase { .. } | Effect::Merge { .. } if run.gate != GateState::Pass => {
                Some("the Gate hasn't passed".to_owned())
            }
            Effect::Merge { .. }
                if run.snapshot.as_ref().is_some_and(|snapshot| snapshot.draft) =>
            {
                Some("the PR is a draft".to_owned())
            }
            Effect::Rebase { .. }
                if self.store.rebase_streak(&run.repo, run.number, run.id)?
                    >= REBASE_STARTED_RUNS =>
            {
                Some(format!(
                    "the base moved under {REBASE_STARTED_RUNS} rebase-started Runs in a row; \
                     a merge queue would land the PR without rebasing it each time"
                ))
            }
            Effect::Comment { .. }
            | Effect::Label { .. }
            | Effect::Rebase { .. }
            | Effect::Merge { .. } => None,
        })
    }

    fn await_result(&mut self, key: &PrKey, step: &str, request: String) {
        if let Some(running) = self
            .active
            .get_mut(key)
            .and_then(|run| run.running.get_mut(step))
        {
            running.awaiting.insert(request);
        }
    }

    fn tell(&self, key: &PrKey, step: &str, request: &str, result: EffectResult) {
        if let Some(running) = self.active[key].running.get(step) {
            running.handle.send(ToStep::EffectResult {
                id: request.to_owned(),
                result,
            });
        }
    }

    /// Settles a request without carrying it out: tells the Step and puts
    /// it in the Run's events.
    fn answer(
        &mut self,
        key: &PrKey,
        step: &str,
        request: &str,
        effect: Effect,
        result: EffectResult,
    ) -> Result<(), StoreError> {
        self.tell(key, step, request, result.clone());
        self.record(self.active[key].id, step, effect, result)
    }

    fn record(
        &self,
        run: RunId,
        step: &str,
        effect: Effect,
        result: EffectResult,
    ) -> Result<(), StoreError> {
        let event = RunEvent::Effect {
            step: step.to_owned(),
            effect,
            result,
        };
        self.journal.append(run, event).map(drop)
    }

    /// Hands a finished Effect's result to the Step waiting for it.
    pub(super) fn on_effect_finished(&mut self, finished: Finished) {
        let Some(key) = self.key_of(finished.run) else {
            return;
        };
        let run = self.active.get_mut(&key).expect("found above");
        if let Some(running) = run.running.get_mut(&finished.step)
            && running.awaiting.remove(&finished.request)
        {
            running.handle.send(ToStep::EffectResult {
                id: finished.request,
                result: finished.result,
            });
        }
    }

    /// Closes every intent a crash left open. Runs on the first sync, once
    /// the Runs whose head moved while the daemon was down have ended.
    pub(super) fn reconcile_effects(&mut self) -> Result<(), StoreError> {
        for row in self.store.open_effects()? {
            let current = self
                .run(row.run)
                .is_some_and(|run| run.head_sha == row.head_sha);
            self.carry_out(row, Call::Reconcile { current });
        }
        Ok(())
    }

    /// Carries out an open intent on a task of its own, closes it with the
    /// result, and puts that in the Run's events.
    fn carry_out(&self, row: EffectRow, call: Call) {
        let github = Arc::clone(&self.github);
        let journal = Arc::clone(&self.journal);
        let reports = self.reports.clone();
        tokio::spawn(async move {
            let result = perform(github.as_ref(), &row, call).await;
            if let Err(error) = journal.close_effect(&row, result.clone()) {
                eprintln!("slopwatchd: can't record Effect {}: {error}", row.id);
            }
            let _ = reports.send(Input::Effect(Finished {
                run: row.run,
                step: row.step,
                request: row.request,
                result,
            }));
        });
    }
}

/// Calls GitHub for an open intent. A reconciled comment is looked up by
/// its marker first, and one whose Run ended is dropped if it isn't there.
async fn perform(github: &dyn GitHub, row: &EffectRow, call: Call) -> EffectResult {
    if let Call::Reconcile { current } = call {
        if let Effect::Comment { .. } = row.effect {
            match github
                .has_comment(&row.repo, row.number, &marker(row.id))
                .await
            {
                Ok(true) => return EffectResult::Done,
                Ok(false) => {}
                Err(error) => {
                    return EffectResult::Failed {
                        reason: error.to_string(),
                    };
                }
            }
        }
        if !current {
            return EffectResult::Dropped {
                reason: "the Run ended while the daemon was down".to_owned(),
            };
        }
    }
    let done = match &row.effect {
        Effect::Comment { body } => {
            let body = format!("{body}\n\n{}", marker(row.id));
            github.comment(&row.repo, row.number, &body).await
        }
        Effect::Label { name, remove } => {
            github.set_label(&row.repo, row.number, name, !remove).await
        }
        Effect::Rerun { job, .. } => github.rerun_job(&row.repo, *job).await,
        // Both carry the head the Run judged, so GitHub refuses them once
        // the PR has moved on.
        Effect::Rebase { method } => {
            github
                .update_branch(&row.repo, row.number, &row.head_sha, *method)
                .await
        }
        Effect::Merge { method } => {
            return match github
                .merge(&row.repo, row.number, &row.head_sha, *method)
                .await
            {
                Ok(Merged::Merged) => EffectResult::Done,
                Ok(Merged::Enqueued) => EffectResult::Enqueued,
                Err(error) => EffectResult::Failed {
                    reason: error.to_string(),
                },
            };
        }
    };
    match done {
        Ok(()) => EffectResult::Done,
        Err(error) => EffectResult::Failed {
            reason: error.to_string(),
        },
    }
}
