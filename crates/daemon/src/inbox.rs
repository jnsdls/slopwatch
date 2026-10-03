//! The Inbox: every open Human Step and Escalation across repos, oldest
//! first.
//!
//! A Human Step belongs to its Run. Each Escalation belongs to a Run, a
//! Watched PR or a cause shared across PRs, and the scope decides what
//! closes it. [`Runs`](crate::Runs) raises and closes entries as Runs start
//! and end; the developer answers a Run entry by retrying its Step, answers
//! a Human Step by approving or rejecting it, and may dismiss a PR entry.
//! Every entry is kept in the store for good, linked to the Runs it
//! touched, and each change also goes to those Runs' event journals, so a
//! Run's record shows its Inbox history. Clients follow the open entries on
//! the `inbox` topic.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use slopwatch_protocol::{
    Actor, BudgetHit, BudgetKind, Cause, Closed, Closing, EntryId, InboxDelta, InboxEntry, PrRef,
    RunEvent, RunId, Scope,
};
use tokio::sync::broadcast;

use crate::notifications::Notifications;
use crate::runs::journal::Journal;
use crate::runs::{RunError, now};
use crate::store::{Store, StoreError};

/// The title of the PR entry a spent PR Budget raises.
pub const OVER_BUDGET: &str = "Over budget";

/// The title of the shared entry a spent daily Budget raises.
pub const DAILY_BUDGET_SPENT: &str = "Daily Budget spent";

/// Deltas a slow subscriber may fall behind by before it gets a fresh
/// snapshot instead.
const BACKLOG: usize = 1024;

pub struct Inbox {
    state: Mutex<State>,
}

struct State {
    store: Store,
    journal: Arc<Journal>,
    /// Each entry that opens gets a banner, and each that closes loses it.
    notifications: Arc<Notifications>,
    open: BTreeMap<EntryId, Open>,
    seq: u64,
    deltas: broadcast::Sender<(u64, InboxDelta)>,
}

struct Open {
    entry: InboxEntry,
    /// The Runs it touched, whose journals hear about each change.
    runs: Vec<RunId>,
}

/// A subscriber's view of the `inbox` topic: the open entries as of `seq`,
/// then every delta after it.
pub struct InboxSubscription {
    pub seq: u64,
    pub snapshot: slopwatch_protocol::Inbox,
    pub deltas: broadcast::Receiver<(u64, InboxDelta)>,
}

impl Inbox {
    /// Loads the entries that were open when the daemon last stopped.
    pub(crate) fn load(
        store: Store,
        journal: Arc<Journal>,
        notifications: Arc<Notifications>,
    ) -> Result<Self, StoreError> {
        let open = store
            .open_entries()?
            .into_iter()
            .map(|(entry, runs)| (entry.id, Open { entry, runs }))
            .collect();
        Ok(Self {
            state: Mutex::new(State {
                store,
                journal,
                notifications,
                open,
                seq: 0,
                deltas: broadcast::channel(BACKLOG).0,
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("no panics while holding the Inbox")
    }

    pub fn subscribe(&self) -> InboxSubscription {
        let state = self.state();
        InboxSubscription {
            seq: state.seq,
            snapshot: state.snapshot(),
            deltas: state.deltas.subscribe(),
        }
    }

    /// The open entries, oldest first.
    pub fn entries(&self) -> Vec<InboxEntry> {
        self.state().snapshot().entries
    }

    /// The developer closes a PR entry without acting on it.
    pub fn dismiss(&self, id: EntryId, actor: &Actor) -> Result<(), RunError> {
        let mut state = self.state();
        let Some(open) = state.open.get(&id) else {
            return Err(RunError::NotFound(format!("No open Inbox entry {id}")));
        };
        if !open.entry.dismissable() {
            return Err(RunError::Invalid(format!(
                "Inbox entry {id} isn't a PR entry, and only a PR entry can be dismissed"
            )));
        }
        let how = Closing::Dismissed {
            actor: actor.clone(),
        };
        Ok(state.close(id, how)?)
    }

    /// Opens a Run entry for an errored Step, unless one is open already.
    pub(crate) fn raise_step_error(
        &self,
        run: RunId,
        pr: PrRef,
        step: &str,
        reason: &str,
    ) -> Result<(), StoreError> {
        let scope = Scope::Run {
            run,
            step: Some(step.to_owned()),
        };
        let mut state = self.state();
        if state.find(|entry| entry.scope == scope).is_some() {
            return Ok(());
        }
        state.raise(
            scope,
            format!("`{step}` errored"),
            vec![reason.to_owned()],
            pr,
            vec![run],
            None,
        )
    }

    /// Closes the Run entries of `run` about `steps`, which the developer
    /// answered with `action`.
    pub(crate) fn answer(
        &self,
        run: RunId,
        steps: &[String],
        action: &str,
        actor: &Actor,
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        let answered: Vec<EntryId> = state.ids(|entry| match &entry.scope {
            Scope::Run {
                run: of,
                step: Some(step),
            } => *of == run && steps.contains(step),
            _ => false,
        });
        for id in answered {
            let how = Closing::Answered {
                action: action.to_owned(),
                actor: actor.clone(),
                note: None,
            };
            state.close(id, how)?;
        }
        Ok(())
    }

    /// Opens a Human Step entry for `step` in `run`, asking `prompt`. One
    /// already open for it, as after a daemon restart, stays as it is, so
    /// it keeps its place in the Inbox.
    pub(crate) fn ask(
        &self,
        run: RunId,
        pr: PrRef,
        step: &str,
        prompt: &str,
    ) -> Result<(), StoreError> {
        let scope = human(run, step);
        let mut state = self.state();
        if state.find(|entry| entry.scope == scope).is_some() {
            return Ok(());
        }
        let reasons = vec![format!("`{step}` waits for you to approve or reject")];
        state.raise(scope, prompt.to_owned(), reasons, pr, vec![run], None)
    }

    /// Closes the open Human Step entry for `step` in `run` as `how`: the
    /// developer's answer once the Step reported, or `StepSettled` for a
    /// Step that settled some other way.
    pub(crate) fn close_human(
        &self,
        run: RunId,
        step: &str,
        how: Closing,
    ) -> Result<(), StoreError> {
        let scope = human(run, step);
        let mut state = self.state();
        let ids = state.ids(|entry| entry.scope == scope);
        for id in ids {
            state.close(id, how.clone())?;
        }
        Ok(())
    }

    /// Closes the Run entries and Human Steps of every Run but `going`,
    /// for a daemon that stopped between ending a Run and closing its
    /// entries.
    pub(crate) fn keep_runs(&self, going: &HashSet<RunId>) -> Result<(), StoreError> {
        let mut state = self.state();
        let ids = state.ids(|entry| entry.run().is_some_and(|run| !going.contains(&run)));
        for id in ids {
            state.close(id, Closing::RunEnded)?;
        }
        Ok(())
    }

    /// Closes every Run entry and Human Step of a Run that ended.
    pub(crate) fn run_ended(&self, run: RunId) -> Result<(), StoreError> {
        let mut state = self.state();
        let ids = state.ids(|entry| entry.run() == Some(run));
        for id in ids {
            state.close(id, Closing::RunEnded)?;
        }
        Ok(())
    }

    /// Puts `reasons` on the PR's one PR entry, opening it if none is
    /// open. `run` is the Run whose end raised them, or for a problem
    /// between Runs, the PR's latest Run if it has one.
    pub(crate) fn raise_pr(
        &self,
        pr: PrRef,
        run: Option<RunId>,
        title: &str,
        reasons: Vec<String>,
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        let Some(id) = state.find(|entry| entry.scope == Scope::Pr && entry.prs.contains(&pr))
        else {
            let runs = run.into_iter().collect();
            return state.raise(Scope::Pr, title.to_owned(), reasons, pr, runs, None);
        };
        let open = state.open.get_mut(&id).expect("found above");
        for reason in reasons {
            if !open.entry.reasons.contains(&reason) {
                open.entry.reasons.push(reason);
            }
        }
        state.changed(id, run)
    }

    /// Closes the PR's PR entry, if one is open.
    pub(crate) fn close_pr(&self, pr: &PrRef, how: Closing) -> Result<(), StoreError> {
        let mut state = self.state();
        let ids = state.ids(|entry| entry.scope == Scope::Pr && entry.prs.contains(pr));
        for id in ids {
            state.close(id, how.clone())?;
        }
        Ok(())
    }

    /// Records that `cause` holds `pr` back: the PR joins the cause's open
    /// entry, or one opens. `latest` is the PR's newest Run, which the
    /// entry then touches. The newest `reasons` replace the old ones.
    pub(crate) fn hold(
        &self,
        cause: Cause,
        pr: PrRef,
        latest: Option<RunId>,
        title: &str,
        reasons: Vec<String>,
    ) -> Result<(), StoreError> {
        let scope = Scope::Cause { cause };
        let mut state = self.state();
        let Some(id) = state.find(|entry| entry.scope == scope) else {
            return state.raise(
                scope,
                title.to_owned(),
                reasons,
                pr,
                latest.into_iter().collect(),
                None,
            );
        };
        let open = state.open.get_mut(&id).expect("found above");
        if open.entry.prs.contains(&pr) && open.entry.reasons == reasons {
            return Ok(());
        }
        if let Err(index) = open.entry.prs.binary_search(&pr) {
            open.entry.prs.insert(index, pr);
        }
        open.entry.reasons = reasons;
        state.changed(id, latest)
    }

    /// Raises the entry a spent Budget asks for, or brings the open one up
    /// to date: the PR's own entry for its Budget, the shared entry for
    /// the day's, which holds `pr` back from then on. `run` is the Run
    /// that ran out, or the PR's latest Run when a Run couldn't start.
    pub(crate) fn over_budget(
        &self,
        pr: PrRef,
        run: Option<RunId>,
        hit: BudgetHit,
    ) -> Result<(), StoreError> {
        let (scope, title, advice) = match hit.kind {
            BudgetKind::Pr => (
                Scope::Pr,
                OVER_BUDGET,
                "Its next Run waits for an outside push, a raise, or \"run anyway once\".",
            ),
            BudgetKind::Daily => (
                Scope::Cause {
                    cause: Cause::DailyBudget,
                },
                DAILY_BUDGET_SPENT,
                "Every PR's next Run waits for local midnight, a raise, or \"run anyway once\".",
            ),
        };
        let reasons = vec![hit.to_string(), advice.to_owned()];
        let mut state = self.state();
        let found = state.find(|entry| {
            entry.scope == scope && (hit.kind == BudgetKind::Daily || entry.prs.contains(&pr))
        });
        let Some(id) = found else {
            let runs = run.into_iter().collect();
            return state.raise(scope, title.to_owned(), reasons, pr, runs, Some(hit));
        };
        let open = state.open.get_mut(&id).expect("found above");
        open.entry.title = title.to_owned();
        // A PR entry that was about something else, such as a Run that
        // ended not shippable, keeps saying so after the Budget's lines.
        if open.entry.budget.is_none() {
            open.entry.reasons.splice(0..0, reasons);
        } else {
            open.entry.reasons = reasons;
        }
        open.entry.budget = Some(hit);
        if let Err(index) = open.entry.prs.binary_search(&pr) {
            open.entry.prs.insert(index, pr);
        }
        state.changed(id, run)
    }

    /// The open entry `id`.
    pub fn entry(&self, id: EntryId) -> Option<InboxEntry> {
        self.state().open.get(&id).map(|open| open.entry.clone())
    }

    /// Closes the open entry `id`, which the developer answered with
    /// `action`.
    pub(crate) fn answer_entry(
        &self,
        id: EntryId,
        action: &str,
        actor: &Actor,
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        if !state.open.contains_key(&id) {
            return Ok(());
        }
        let how = Closing::Answered {
            action: action.to_owned(),
            actor: actor.clone(),
            note: None,
        };
        state.close(id, how)
    }

    /// The PRs `cause`'s open entry holds back, if one is open.
    pub(crate) fn held_by(&self, cause: &Cause) -> Vec<PrRef> {
        self.state()
            .open
            .values()
            .find(|open| matches!(&open.entry.scope, Scope::Cause { cause: held } if held == cause))
            .map(|open| open.entry.prs.clone())
            .unwrap_or_default()
    }

    /// The causes with an open entry.
    pub(crate) fn open_causes(&self) -> Vec<Cause> {
        self.state()
            .open
            .values()
            .filter_map(|open| match &open.entry.scope {
                Scope::Cause { cause } => Some(cause.clone()),
                _ => None,
            })
            .collect()
    }

    /// The daemon saw `cause` cleared.
    pub(crate) fn clear(&self, cause: Cause) -> Result<(), StoreError> {
        let scope = Scope::Cause { cause };
        let mut state = self.state();
        let ids = state.ids(|entry| entry.scope == scope);
        for id in ids {
            state.close(id, Closing::CauseCleared)?;
        }
        Ok(())
    }

    /// `pr` no longer hits the causes `which` picks. An entry left holding
    /// nothing closes as `how`.
    pub(crate) fn release(
        &self,
        pr: &PrRef,
        how: Closing,
        which: impl Fn(&Cause) -> bool,
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        let ids = state.ids(|entry| match &entry.scope {
            Scope::Cause { cause } => which(cause) && entry.prs.contains(pr),
            _ => false,
        });
        for id in ids {
            state.drop_pr(id, pr, how.clone())?;
        }
        Ok(())
    }

    /// Closes the PR entries of PRs that aren't watched any more, and
    /// takes them off every cause.
    pub(crate) fn keep_watched(&self, watched: &HashSet<PrRef>) -> Result<(), StoreError> {
        let mut state = self.state();
        let left: Vec<(EntryId, Vec<PrRef>)> = state
            .open
            .values()
            .filter(|open| open.entry.run().is_none())
            .filter_map(|open| {
                let gone: Vec<PrRef> = open
                    .entry
                    .prs
                    .iter()
                    .filter(|pr| !watched.contains(pr))
                    .cloned()
                    .collect();
                (!gone.is_empty()).then_some((open.entry.id, gone))
            })
            .collect();
        for (id, gone) in left {
            for pr in gone {
                state.drop_pr(id, &pr, Closing::LeftWatched)?;
            }
        }
        Ok(())
    }
}

impl State {
    fn snapshot(&self) -> slopwatch_protocol::Inbox {
        slopwatch_protocol::Inbox {
            entries: self.open.values().map(|open| open.entry.clone()).collect(),
        }
    }

    fn find(&self, matches: impl Fn(&InboxEntry) -> bool) -> Option<EntryId> {
        self.open
            .values()
            .find(|open| matches(&open.entry))
            .map(|open| open.entry.id)
    }

    fn ids(&self, matches: impl Fn(&InboxEntry) -> bool) -> Vec<EntryId> {
        self.open
            .values()
            .filter(|open| matches(&open.entry))
            .map(|open| open.entry.id)
            .collect()
    }

    fn raise(
        &mut self,
        scope: Scope,
        title: String,
        reasons: Vec<String>,
        pr: PrRef,
        runs: Vec<RunId>,
        budget: Option<BudgetHit>,
    ) -> Result<(), StoreError> {
        let entry = InboxEntry {
            id: EntryId(0),
            scope,
            title,
            reasons,
            prs: vec![pr],
            raised_at: now(),
            budget,
            closed: None,
        };
        let id = self.store.insert_entry(&entry, &runs)?;
        let entry = InboxEntry { id, ..entry };
        self.open.insert(
            id,
            Open {
                entry: entry.clone(),
                runs,
            },
        );
        self.publish(id)?;
        self.notifications.entry_opened(&entry)
    }

    /// Stores and announces what changed in an open entry, which now also
    /// touches `run`.
    fn changed(&mut self, id: EntryId, run: Option<RunId>) -> Result<(), StoreError> {
        let open = self.open.get_mut(&id).expect("only open entries change");
        let run = run.filter(|run| !open.runs.contains(run));
        if let Some(run) = run {
            open.runs.push(run);
        }
        self.store.put_entry(&open.entry, run)?;
        self.publish(id)
    }

    /// Takes `pr` off an entry, and closes the entry as `how` once it holds
    /// no PR.
    fn drop_pr(&mut self, id: EntryId, pr: &PrRef, how: Closing) -> Result<(), StoreError> {
        let open = self.open.get_mut(&id).expect("only open entries change");
        open.entry.prs.retain(|held| held != pr);
        if open.entry.prs.is_empty() {
            // A closed entry keeps the last PR it held, for the record.
            open.entry.prs.push(pr.clone());
            return self.close(id, how);
        }
        self.changed(id, None)
    }

    fn close(&mut self, id: EntryId, how: Closing) -> Result<(), StoreError> {
        let open = self.open.get_mut(&id).expect("only open entries close");
        open.entry.closed = Some(Closed { at: now(), how });
        self.store.put_entry(&open.entry, None)?;
        self.publish(id)?;
        self.open.remove(&id);
        self.notifications.entry_closed(id)
    }

    /// Sends the entry as it stands to subscribers and to the journal of
    /// every Run it touched.
    fn publish(&mut self, id: EntryId) -> Result<(), StoreError> {
        let open = &self.open[&id];
        for run in &open.runs {
            self.journal.append(
                *run,
                RunEvent::Inbox {
                    entry: open.entry.clone(),
                },
            )?;
        }
        self.seq += 1;
        // No subscribers is fine: the next one starts from a snapshot.
        let _ = self.deltas.send((
            self.seq,
            InboxDelta::Put {
                entry: Box::new(open.entry.clone()),
            },
        ));
        Ok(())
    }
}

/// The scope of the Human Step `step` in `run`.
fn human(run: RunId, step: &str) -> Scope {
    Scope::Human {
        run,
        step: step.to_owned(),
    }
}
