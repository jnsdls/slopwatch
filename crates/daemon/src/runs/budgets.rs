//! Budgets: caps, in list-price US dollars, on what Steps spend per Step,
//! per Watched PR since its last outside push, and per day.
//!
//! Every priced `usage` a Step reports goes on record. A Step past its own
//! Budget settles `error(budget)` and stops, and the Run goes on. Once a
//! Run's PR Budget or the day's runs out, its Steps that spend stop with
//! `error(budget)`, Steps that don't, such as CI, finish, no new Step
//! starts, and the Run ends over budget. The PR Budget raises the PR's
//! entry, and the day's holds the PR on one shared entry. While a Budget
//! is spent no Run starts on the PRs it holds, same-SHA Runs included.
//!
//! A Run's PR Budget counts its Budget window: the Runs since the PR's
//! last outside push. slopwatch's own pushes, and same-SHA Runs, stay in
//! the window. The day starts at local midnight, which clears the daily
//! entry and starts the PRs it held on the same SHA.
//!
//! The developer answers an over budget entry by raising the Budget, for
//! the window on a PR or in the daemon's settings for the day, or by
//! letting one more Run past it with "run anyway once". Either starts the
//! held PRs again.

use slopwatch_core::{EndReason, Pipeline, Verdict};
use slopwatch_protocol::step::{Manifest, Outputs};
use slopwatch_protocol::{
    Actor, BudgetHit, BudgetKind, Cause, Cents, DaemonSettings, EntryId, PrRef, RepoName, RunId,
    Scope,
};

use super::{Engine, PrKey, RunError, now, pr_ref};
use crate::github::OpenPr;
use crate::store::StoreError;

/// A PR held back by a spent Budget, and the head and root base SHA it
/// was held on. A sync leaves it alone until one of them moves.
pub(super) struct BudgetHold {
    pub(super) head_sha: String,
    pub(super) root_sha: String,
    pub(super) hit: BudgetHit,
}

/// How a Step that a spent PR or daily Budget stopped settles.
fn stopped(kind: BudgetKind) -> String {
    match kind {
        BudgetKind::Pr => "error(budget): the PR's Budget is spent".to_owned(),
        BudgetKind::Daily => "error(budget): the day's Budget is spent".to_owned(),
    }
}

/// What a Step can spend in one attempt: what the Pipeline sets, or else
/// its Plugin's manifest. `None` leaves it without a Budget of its own.
pub(super) fn step_budget(step: &slopwatch_core::Step, manifest: Option<&Manifest>) -> Option<f64> {
    step.budget_usd
        .or_else(|| manifest.and_then(|manifest| manifest.budget_usd))
}

impl Engine {
    /// The PR Budget of the window `window` opened: the one the developer
    /// raised it to, or the Pipeline's.
    fn pr_budget(
        &self,
        repo: &RepoName,
        number: u64,
        window: RunId,
        pipeline: &Pipeline,
    ) -> Result<f64, StoreError> {
        Ok(self
            .store
            .raised_pr_budget(repo, number, window)?
            .unwrap_or(pipeline.budget_usd()))
    }

    /// What Steps spent since local midnight.
    pub(super) fn spent_today(&self) -> Result<f64, StoreError> {
        self.store.spent_since(local_midnight(now()))
    }

    /// The first Budget, the PR's then the day's, that a Run in the window
    /// `window` has run out of, skipping the ones `lifted`. `None` for the
    /// window a Run would open, which has spent nothing.
    fn spent_budget(
        &self,
        repo: &RepoName,
        number: u64,
        window: Option<RunId>,
        pipeline: &Pipeline,
        lifted: &[BudgetKind],
    ) -> Result<Option<BudgetHit>, StoreError> {
        if let Some(window) = window
            && !lifted.contains(&BudgetKind::Pr)
        {
            let spent = self.store.window_spent(window)?;
            let budget = self.pr_budget(repo, number, window, pipeline)?;
            if spent >= budget {
                return Ok(Some(hit(BudgetKind::Pr, spent, budget)));
            }
        }
        if let Some(daily) = self.settings.daily_budget
            && !lifted.contains(&BudgetKind::Daily)
        {
            let spent = self.spent_today()?;
            if spent >= daily.usd() {
                return Ok(Some(hit(BudgetKind::Daily, spent, daily.usd())));
            }
        }
        Ok(None)
    }

    /// The Budget window a new Run on `pr` belongs to: its latest Run's,
    /// when that Run judged the same head or slopwatch pushed the new one,
    /// such as a Fix commit or a rebase. `None` when an outside push opens
    /// a new window.
    pub(super) fn budget_window(
        &self,
        repo: &RepoName,
        pr: &OpenPr,
    ) -> Result<Option<RunId>, StoreError> {
        let Some(latest) = self.store.latest_run(repo, pr.number)? else {
            return Ok(None);
        };
        let ours = latest.head_sha == pr.head_sha
            || latest.end == Some(EndReason::Pushed)
            || self.store.pushed_by_slopwatch(repo, &pr.head_sha)?;
        Ok(latest.budget_window.filter(|_| ours))
    }

    /// Whether a spent Budget holds `pr` back from a Run in `window`
    /// under `pipeline`. If so the PR waits on its entry, and the sync
    /// leaves it alone until its head or root base moves.
    pub(super) fn hold_if_spent(
        &mut self,
        key: &PrKey,
        pr: &OpenPr,
        window: Option<RunId>,
        pipeline: &Pipeline,
    ) -> Result<bool, StoreError> {
        let (repo, number) = key;
        let lifted = self.store.lifts(repo, *number)?;
        let Some(hit) = self.spent_budget(repo, *number, window, pipeline, &lifted)? else {
            return Ok(false);
        };
        let latest = self.store.latest_run_id(repo, *number)?;
        self.inbox.over_budget(pr_ref(repo, *number), latest, hit)?;
        self.budget_held.insert(
            key.clone(),
            BudgetHold {
                head_sha: pr.head_sha.clone(),
                root_sha: pr.root().sha,
                hit,
            },
        );
        self.publish(repo, *number)?;
        Ok(true)
    }

    /// Whether the sync should leave `pr` alone: a spent Budget held it on
    /// this head and root base.
    pub(super) fn budget_holds(&self, key: &PrKey, pr: &OpenPr) -> bool {
        self.budget_held
            .get(key)
            .is_some_and(|held| held.head_sha == pr.head_sha && held.root_sha == pr.root().sha)
    }

    /// What the row of a PR a spent Budget holds says.
    pub(super) fn budget_message(&self, key: &PrKey) -> Option<String> {
        self.budget_held
            .get(key)
            .map(|held| format!("Over budget. {}", held.hit))
    }

    /// What `budget_usd` a Step about to start gets: the tightest of its
    /// own Budget and what's left of the PR's and the day's.
    pub(super) fn start_budget(
        &self,
        key: &PrKey,
        own: Option<f64>,
    ) -> Result<Option<f64>, StoreError> {
        let run = &self.active[key];
        let mut caps: Vec<f64> = own.into_iter().collect();
        if !run.lifted.contains(&BudgetKind::Pr) {
            let budget = self.pr_budget(&run.repo, run.number, run.budget_window, &run.pipeline)?;
            caps.push(budget - self.store.window_spent(run.budget_window)?);
        }
        if let Some(daily) = self.settings.daily_budget
            && !run.lifted.contains(&BudgetKind::Daily)
        {
            caps.push(daily.usd() - self.spent_today()?);
        }
        Ok(caps.into_iter().reduce(f64::min).map(|cap| cap.max(0.0)))
    }

    /// Records `usd`, what one model call of `step` cost, and stops what
    /// it pushed past a Budget: the Step itself past its own, and every
    /// Run whose PR's or the day's Budget it spent.
    pub(super) fn on_usage(&mut self, key: &PrKey, step: &str, usd: f64) -> Result<(), StoreError> {
        let run = self.active.get_mut(key).expect("only active Runs report");
        let id = run.id;
        self.store.add_usage(id, step, usd, now())?;
        let running = run.running.get_mut(step).expect("the report matched it");
        running.spent += usd;
        running.costed = true;
        let over = running
            .budget
            .filter(|&budget| !running.reported && running.spent >= budget)
            .map(|budget| (running.spent, budget));
        if let Some((spent, budget)) = over {
            let reason = format!(
                "error(budget): spent {} of its {} Budget",
                Cents::from_usd(spent),
                Cents::from_usd(budget)
            );
            self.settle(key, step, Verdict::Error, Some(reason), Outputs::default())?;
            if let Some(running) = self.active[key].running.get(step) {
                running.handle.cancel();
            }
        }
        // The spend may have run out the PR's Budget, which other Runs on
        // the PR don't share, or the day's, which every Run does.
        let keys: Vec<PrKey> = self.active.keys().cloned().collect();
        for other in keys {
            if self.check_budgets(&other)? || other == *key {
                self.advance(&other)?;
            }
        }
        Ok(())
    }

    /// Checks whether the Run going on `key` has run out of its PR's or the
    /// day's Budget. If it has, its Steps that spend stop with
    /// `error(budget)` and nothing more starts, so the Run ends over budget
    /// once its other Steps settle. Returns whether it ran out just now.
    pub(super) fn check_budgets(&mut self, key: &PrKey) -> Result<bool, StoreError> {
        let Some(run) = self.active.get(key) else {
            return Ok(false);
        };
        if run.over_budget.is_some() {
            return Ok(false);
        }
        let Some(hit) = self.spent_budget(
            &run.repo,
            run.number,
            Some(run.budget_window),
            &run.pipeline,
            &run.lifted,
        )?
        else {
            return Ok(false);
        };
        let run = self.active.get_mut(key).expect("checked above");
        run.over_budget = Some(hit);
        let id = run.id;
        self.ready.retain(|(run, _)| *run != id);
        let spending: Vec<String> = run
            .running
            .iter()
            .filter(|(_, running)| running.costed && !running.reported)
            .map(|(step, _)| step.clone())
            .collect();
        for step in spending {
            self.settle(
                key,
                &step,
                Verdict::Error,
                Some(stopped(hit.kind)),
                Outputs::default(),
            )?;
            if let Some(running) = self.active[key].running.get(&step) {
                running.handle.cancel();
            }
        }
        Ok(true)
    }

    /// The developer raises the Budget the open entry `id` ran out of to
    /// `to`, and the PRs it held start again.
    pub(super) fn raise_budget(
        &mut self,
        id: EntryId,
        to: Cents,
        actor: &Actor,
    ) -> Result<(), RunError> {
        let (entry, hit) = self.over_budget_entry(id)?;
        if to <= hit.spent {
            return Err(RunError::Invalid(format!(
                "Raise it past what's spent, {}",
                hit.spent
            )));
        }
        let action = format!("raise the Budget to {to}");
        match hit.kind {
            BudgetKind::Pr => {
                let pr = &entry.prs[0];
                let window = self
                    .store
                    .latest_run(&pr.repo, pr.number)?
                    .and_then(|latest| latest.budget_window)
                    .ok_or_else(|| RunError::Invalid(format!("{pr} has no Run to raise")))?;
                self.store
                    .raise_pr_budget(&pr.repo, pr.number, window, to.usd())?;
            }
            BudgetKind::Daily => {
                self.settings = DaemonSettings {
                    daily_budget: Some(to),
                };
                self.store.put_daemon_settings(&self.settings)?;
            }
        }
        self.inbox.answer_entry(id, &action, actor)?;
        self.start_held(&entry.prs)
    }

    /// The developer lets one more Run of each PR the open entry `id` holds
    /// past the Budget it ran out of.
    pub(super) fn run_anyway_once(&mut self, id: EntryId, actor: &Actor) -> Result<(), RunError> {
        let (entry, hit) = self.over_budget_entry(id)?;
        for pr in &entry.prs {
            self.store.add_lift(&pr.repo, pr.number, hit.kind)?;
        }
        self.inbox.answer_entry(id, "run anyway once", actor)?;
        self.start_held(&entry.prs)
    }

    /// The developer replaced the daemon's settings. A daily Budget raised
    /// past today's spend, or turned off, clears its entry.
    pub(super) fn set_settings(&mut self, settings: DaemonSettings) -> Result<(), RunError> {
        if settings.daily_budget == Some(Cents(0)) {
            return Err(RunError::Invalid(
                "A daily Budget of $0 would hold every PR. Turn it off instead.".to_owned(),
            ));
        }
        self.store.put_daemon_settings(&settings)?;
        self.settings = settings;
        Ok(self.clear_daily_budget()?)
    }

    /// Clears the daily Budget's entry once it no longer holds, as at local
    /// midnight or after a raise, and starts the PRs it held.
    pub(super) fn clear_daily_budget(&mut self) -> Result<(), StoreError> {
        let cause = Cause::DailyBudget;
        if !self.inbox.open_causes().contains(&cause) || !self.watching.fresh() {
            return Ok(());
        }
        let spent = match self.settings.daily_budget {
            Some(daily) => self.spent_today()? >= daily.usd(),
            None => false,
        };
        if spent {
            return Ok(());
        }
        let held = self.inbox.held_by(&cause);
        self.inbox.clear(cause)?;
        match self.start_held(&held) {
            Err(RunError::Store(error)) => Err(error),
            _ => Ok(()),
        }
    }

    /// The open entry `id`, if a spent Budget raised it, with the Budget.
    fn over_budget_entry(
        &self,
        id: EntryId,
    ) -> Result<(slopwatch_protocol::InboxEntry, BudgetHit), RunError> {
        if self.stopped {
            return Err(RunError::Invalid("The daemon is restarting".to_owned()));
        }
        let entry = self
            .inbox
            .entry(id)
            .ok_or_else(|| RunError::NotFound(format!("No open Inbox entry {id}")))?;
        let hit = entry
            .budget
            .filter(|_| matches!(entry.scope, Scope::Pr | Scope::Cause { .. }))
            .ok_or_else(|| {
                RunError::Invalid(format!("Inbox entry {id} isn't about a spent Budget"))
            })?;
        Ok((entry, hit))
    }

    /// Lets the PRs a Budget held start again: a PR whose latest Run ended
    /// on its head gets a same-SHA Run, and one whose head moved gets its
    /// Run from the next sync.
    fn start_held(&mut self, prs: &[PrRef]) -> Result<(), RunError> {
        for pr in prs {
            let key = (pr.repo.clone(), pr.number);
            self.budget_held.remove(&key);
            self.publish(&pr.repo, pr.number)?;
            if self.active.contains_key(&key) {
                continue;
            }
            let Some(latest) = self.store.latest_run_id(&pr.repo, pr.number)? else {
                continue;
            };
            let stored = self
                .store
                .run(latest)?
                .ok_or_else(|| RunError::NotFound(format!("No Run {latest}")))?;
            match self.same_sha_pr(&stored) {
                Ok(open) => self.start_same_sha(stored, &open)?,
                Err(RunError::Store(error)) => return Err(RunError::Store(error)),
                Err(RunError::NotFound(why) | RunError::Invalid(why)) => {
                    eprintln!("slopwatchd: {pr} waits for the next sync: {why}");
                }
            }
        }
        Ok(())
    }
}

fn hit(kind: BudgetKind, spent: f64, budget: f64) -> BudgetHit {
    BudgetHit {
        kind,
        spent: Cents::from_usd(spent),
        budget: Cents::from_usd(budget),
    }
}

/// Local midnight of the day `now` falls in, in seconds since the epoch.
pub(crate) fn local_midnight(now: i64) -> i64 {
    // SAFETY: `localtime_r` and `mktime` only read and write the `tm` we
    // hand them, which lives on this stack frame.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let at = now as libc::time_t;
        if libc::localtime_r(&at, &mut tm).is_null() {
            return now - now.rem_euclid(86_400);
        }
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        // Let mktime work out whether midnight was in summer time.
        tm.tm_isdst = -1;
        libc::mktime(&mut tm) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_midnight_starts_the_day_now_falls_in() {
        let now = super::now();
        let midnight = local_midnight(now);
        assert!(midnight <= now);
        // A day with a clock change runs 23 or 25 hours.
        assert!(now - midnight < 25 * 3600, "{now} {midnight}");
        assert_eq!(local_midnight(midnight), midnight);
    }
}
