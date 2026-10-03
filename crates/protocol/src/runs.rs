//! The `run/<id>` topic. Every Run has an append-only event journal, and the
//! topic replays it by sequence number: a client that subscribes from
//! scratch gets every event, and one that reconnects names the last
//! sequence number it saw and gets only what it missed (ADR 0010). Folding
//! the events in order with [`RunView::apply`] rebuilds the Run.

use std::fmt;

use serde::{Deserialize, Serialize};
use slopwatch_core::{EndReason, GateState, Verdict, WaiverCategory};

use crate::step::{Effect, EffectResult, Outputs};
use crate::{Actor, InboxEntry, RepoName};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub u64);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One entry in a Run's event journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    /// Always the first event.
    Started {
        repo: RepoName,
        number: u64,
        head_sha: String,
        /// The root base branch the Pipeline came from.
        base: String,
        /// The commit on `base` the Pipeline was read from (ADR 0007).
        base_sha: String,
        /// The Pipeline's Steps, each after the Steps it needs.
        steps: Vec<StepInfo>,
        /// The Gate as the Pipeline file writes it.
        gate: String,
    },
    StepStarted {
        step: String,
        /// Which attempt this is, counting from 1. Each has its own Step
        /// log.
        #[serde(default = "first_attempt")]
        attempt: u32,
    },
    /// A running Step's `progress` line.
    StepProgress {
        step: String,
        message: String,
    },
    /// The Step has its Verdict, from the Step itself or from the daemon.
    StepSettled {
        step: String,
        verdict: Verdict,
        /// Why the daemon assigned the Verdict: the skip reason, or what
        /// went wrong for an error.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default)]
        outputs: Outputs,
        /// The earlier Run on the same head SHA whose Outcome this is. The
        /// Step didn't run in this Run (ADR 0007).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reused_from: Option<RunId>,
    },
    /// The developer retried `step`, an errored Step. It and `dependents`,
    /// every Step after it, go back to pending and run again in this Run.
    StepRetried {
        step: String,
        #[serde(default)]
        dependents: Vec<String>,
    },
    /// A Waiver covers `step` in this Run: the developer waived it here,
    /// or earlier on the same head SHA. The Gate counts its Verdict as pass.
    StepWaived {
        step: String,
        waiver: Waiver,
    },
    Gate {
        state: GateState,
    },
    /// What became of an Effect a Step requested. An Effect still in
    /// flight when the Run ends, or one a Step requests after it ended,
    /// lands after `ended`.
    Effect {
        step: String,
        effect: Effect,
        result: EffectResult,
    },
    Ended {
        reason: EndReason,
        /// The Gate passed only because of Waivers: the Run reads
        /// "shippable (waived)".
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        waived: bool,
    },
    /// An Inbox entry that touched the Run opened, changed or closed. It
    /// replaces an earlier one with the same id, and may come after
    /// `ended`: a PR entry the Run's end raised closes later.
    Inbox {
        entry: InboxEntry,
    },
    /// The Run's detail is gone: its Step logs, and every journal event
    /// except the ones that rebuild its record. Only `inbox` events come
    /// after it, for an entry that closes later.
    Pruned {
        /// Seconds since the Unix epoch.
        at: i64,
    },
}

fn first_attempt() -> u32 {
    1
}

/// The developer's ruling that one Step's settled, non-pass Verdict counts
/// as pass for one head SHA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiver {
    pub category: WaiverCategory,
    pub reason: String,
    pub actor: Actor,
}

/// A Step as the Run lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepInfo {
    pub id: String,
    pub plugin: String,
    pub needs: Vec<String>,
    /// The Gate reads this Step. A Step it doesn't read is advisory.
    pub gated: bool,
}

/// One Run, rebuilt from its events.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunView {
    /// The sequence number of the last event applied, 0 before any.
    pub seq: u64,
    pub repo: Option<RepoName>,
    pub number: u64,
    pub head_sha: String,
    pub base: String,
    pub base_sha: String,
    pub steps: Vec<StepView>,
    pub gate_text: String,
    pub gate: Option<GateState>,
    pub end: Option<EndReason>,
    /// When the Run's detail was pruned, in seconds since the Unix epoch.
    pub pruned_at: Option<i64>,
    /// The Run ended shippable only because of Waivers.
    pub waived: bool,
    /// The Inbox entries that touched the Run, oldest first.
    pub inbox: Vec<InboxEntry>,
    /// Every Effect the Run's Steps requested, in the order they finished.
    pub effects: Vec<EffectView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectView {
    pub step: String,
    pub effect: Effect,
    pub result: EffectResult,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StepView {
    pub info: StepInfo,
    pub status: StepStatus,
    /// The latest attempt, 0 while the Step hasn't started.
    pub attempt: u32,
    /// The latest `progress` line of the running attempt.
    pub progress: Option<String>,
    /// The Waiver that counts the Step's Verdict as pass, if any.
    pub waiver: Option<Waiver>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepStatus {
    Pending,
    Running,
    Settled {
        verdict: Verdict,
        reason: Option<String>,
        outputs: Outputs,
        reused_from: Option<RunId>,
    },
}

impl RunView {
    /// Applies the event with sequence number `seq`. One the view already
    /// has is dropped, so a replay that overlaps live events is harmless.
    pub fn apply(&mut self, seq: u64, event: RunEvent) {
        if seq <= self.seq {
            return;
        }
        self.seq = seq;
        match event {
            RunEvent::Started {
                repo,
                number,
                head_sha,
                base,
                base_sha,
                steps,
                gate,
            } => {
                self.repo = Some(repo);
                self.number = number;
                self.head_sha = head_sha;
                self.base = base;
                self.base_sha = base_sha;
                self.steps = steps
                    .into_iter()
                    .map(|info| StepView {
                        info,
                        status: StepStatus::Pending,
                        attempt: 0,
                        progress: None,
                        waiver: None,
                    })
                    .collect();
                self.gate_text = gate;
                self.gate = Some(GateState::Pending);
            }
            RunEvent::StepStarted { step, attempt } => {
                if let Some(view) = self.step_mut(&step) {
                    view.status = StepStatus::Running;
                    view.attempt = attempt;
                    view.progress = None;
                }
            }
            RunEvent::StepProgress { step, message } => {
                if let Some(view) = self.step_mut(&step) {
                    view.progress = Some(message);
                }
            }
            RunEvent::StepSettled {
                step,
                verdict,
                reason,
                outputs,
                reused_from,
            } => {
                if let Some(view) = self.step_mut(&step) {
                    view.status = StepStatus::Settled {
                        verdict,
                        reason,
                        outputs,
                        reused_from,
                    };
                }
            }
            RunEvent::StepRetried { step, dependents } => {
                for id in std::iter::once(step).chain(dependents) {
                    if let Some(view) = self.step_mut(&id) {
                        view.status = StepStatus::Pending;
                    }
                }
            }
            RunEvent::StepWaived { step, waiver } => {
                if let Some(view) = self.step_mut(&step) {
                    view.waiver = Some(waiver);
                }
            }
            RunEvent::Gate { state } => self.gate = Some(state),
            RunEvent::Effect {
                step,
                effect,
                result,
            } => self.effects.push(EffectView {
                step,
                effect,
                result,
            }),
            RunEvent::Pruned { at } => self.pruned_at = Some(at),
            RunEvent::Ended { reason, waived } => {
                self.end = Some(reason);
                self.waived = waived;
            }
            RunEvent::Inbox { entry } => {
                match self.inbox.binary_search_by(|known| known.id.cmp(&entry.id)) {
                    Ok(index) => self.inbox[index] = entry,
                    Err(index) => self.inbox.insert(index, entry),
                }
            }
        }
    }

    pub fn step(&self, id: &str) -> Option<&StepView> {
        self.steps.iter().find(|view| view.info.id == id)
    }

    fn step_mut(&mut self, id: &str) -> Option<&mut StepView> {
        self.steps.iter_mut().find(|view| view.info.id == id)
    }
}

/// A Run as the PR list and the Run history chips show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: RunId,
    pub head_sha: String,
    pub gate: GateState,
    /// `None` while the Run is going.
    pub end: Option<EndReason>,
    /// It ended shippable only because of Waivers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waived: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started() -> RunEvent {
        RunEvent::Started {
            repo: RepoName::new("o", "r"),
            number: 7,
            head_sha: "abc".into(),
            base: "main".into(),
            base_sha: "def".into(),
            steps: vec![StepInfo {
                id: "ci".into(),
                plugin: "ci".into(),
                needs: vec![],
                gated: true,
            }],
            gate: "[ci]".into(),
        }
    }

    #[test]
    fn events_fold_into_the_run() {
        let mut view = RunView::default();

        view.apply(1, started());
        view.apply(
            2,
            RunEvent::StepStarted {
                step: "ci".into(),
                attempt: 1,
            },
        );
        assert_eq!(view.step("ci").unwrap().status, StepStatus::Running);
        assert_eq!(view.step("ci").unwrap().attempt, 1);

        view.apply(
            3,
            RunEvent::StepSettled {
                step: "ci".into(),
                verdict: Verdict::Pass,
                reason: None,
                outputs: Outputs::default(),
                reused_from: None,
            },
        );
        view.apply(
            4,
            RunEvent::Gate {
                state: GateState::Pass,
            },
        );
        view.apply(
            5,
            RunEvent::Ended {
                reason: EndReason::Shippable,
                waived: false,
            },
        );

        assert!(matches!(
            view.step("ci").unwrap().status,
            StepStatus::Settled {
                verdict: Verdict::Pass,
                ..
            }
        ));
        assert_eq!(view.gate, Some(GateState::Pass));
        assert_eq!(view.end, Some(EndReason::Shippable));
        assert_eq!(view.seq, 5);
    }

    #[test]
    fn a_retry_puts_the_step_and_its_dependents_back_to_pending() {
        let mut view = RunView::default();
        view.apply(1, started());
        view.apply(
            2,
            RunEvent::StepSettled {
                step: "ci".into(),
                verdict: Verdict::Error,
                reason: Some("error(crash): exited with status 1 before reporting".into()),
                outputs: Outputs::default(),
                reused_from: None,
            },
        );

        view.apply(
            3,
            RunEvent::StepRetried {
                step: "ci".into(),
                dependents: vec![],
            },
        );

        assert_eq!(view.step("ci").unwrap().status, StepStatus::Pending);
        assert_eq!(
            serde_json::to_value(RunEvent::StepRetried {
                step: "ci".into(),
                dependents: vec!["review".into()],
            })
            .unwrap(),
            json!({ "kind": "step_retried", "step": "ci", "dependents": ["review"] })
        );
    }

    #[test]
    fn a_waiver_marks_its_step_and_a_waived_pass_marks_the_run() {
        let mut view = RunView::default();
        view.apply(1, started());
        let waiver = Waiver {
            category: WaiverCategory::FalsePositive,
            reason: "the check is flaky".into(),
            actor: Actor::Developer { via: "gui".into() },
        };
        let event = RunEvent::StepWaived {
            step: "ci".into(),
            waiver: waiver.clone(),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({
                "kind": "step_waived",
                "step": "ci",
                "waiver": {
                    "category": "false_positive",
                    "reason": "the check is flaky",
                    "actor": { "kind": "developer", "via": "gui" },
                },
            })
        );

        view.apply(2, event);
        view.apply(
            3,
            serde_json::from_value(json!({
                "kind": "ended", "reason": "shippable", "waived": true,
            }))
            .unwrap(),
        );

        assert_eq!(view.step("ci").unwrap().waiver, Some(waiver));
        assert!(view.waived);
        let older: RunEvent =
            serde_json::from_value(json!({ "kind": "ended", "reason": "shippable" })).unwrap();
        assert_eq!(
            older,
            RunEvent::Ended {
                reason: EndReason::Shippable,
                waived: false,
            }
        );
    }

    #[test]
    fn inbox_events_keep_each_entry_that_touched_the_run_in_its_last_state() {
        use crate::{Closed, Closing, EntryId, PrRef, Scope};
        let entry = |id: u64, closed: Option<Closing>| RunEvent::Inbox {
            entry: InboxEntry {
                id: EntryId(id),
                scope: Scope::Pr,
                title: "Not shippable".into(),
                reasons: vec![],
                prs: vec![PrRef {
                    repo: RepoName::new("o", "r"),
                    number: 7,
                }],
                raised_at: 1,
                closed: closed.map(|how| Closed { at: 2, how }),
            },
        };
        let mut view = RunView::default();
        view.apply(1, started());
        view.apply(2, entry(4, None));
        view.apply(3, entry(3, None));
        view.apply(4, entry(4, Some(Closing::NextRunStarted)));

        let ids: Vec<u64> = view.inbox.iter().map(|entry| entry.id.0).collect();
        assert_eq!(ids, [3, 4], "oldest first");
        assert!(view.inbox[0].closed.is_none());
        assert_eq!(
            view.inbox[1].closed.as_ref().map(|closed| &closed.how),
            Some(&Closing::NextRunStarted)
        );
    }

    #[test]
    fn an_event_the_view_already_has_is_dropped() {
        let mut view = RunView::default();
        view.apply(1, started());
        view.apply(
            2,
            RunEvent::Gate {
                state: GateState::Fail,
            },
        );

        view.apply(
            2,
            RunEvent::Gate {
                state: GateState::Pass,
            },
        );

        assert_eq!(view.gate, Some(GateState::Fail));
    }

    #[test]
    fn a_step_started_before_attempts_were_journalled_reads_as_the_first() {
        let event: RunEvent =
            serde_json::from_value(json!({ "kind": "step_started", "step": "ci" })).unwrap();

        assert_eq!(
            event,
            RunEvent::StepStarted {
                step: "ci".into(),
                attempt: 1
            }
        );
    }

    #[test]
    fn events_name_their_kind_on_the_wire() {
        let event = RunEvent::Ended {
            reason: EndReason::NotShippable,
            waived: false,
        };

        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({ "kind": "ended", "reason": "not_shippable" })
        );
    }

    #[test]
    fn a_reused_outcome_names_its_run_and_an_older_event_reads_as_not_reused() {
        let event = RunEvent::StepSettled {
            step: "ci".into(),
            verdict: Verdict::Pass,
            reason: None,
            outputs: Outputs::default(),
            reused_from: Some(RunId(3)),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["reused_from"], json!(3));

        let older: RunEvent = serde_json::from_value(
            json!({ "kind": "step_settled", "step": "ci", "verdict": "pass" }),
        )
        .unwrap();
        assert!(matches!(
            older,
            RunEvent::StepSettled {
                reused_from: None,
                ..
            }
        ));
    }
}
