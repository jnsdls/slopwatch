//! The `run/<id>` topic. Every Run has an append-only event journal, and the
//! topic replays it by sequence number: a client that subscribes from
//! scratch gets every event, and one that reconnects names the last
//! sequence number it saw and gets only what it missed (ADR 0010). Folding
//! the events in order with [`RunView::apply`] rebuilds the Run.

use std::fmt;

use serde::{Deserialize, Serialize};
use slopwatch_core::{EndReason, GateState, Verdict};

use crate::RepoName;
use crate::step::Outputs;

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
    Gate {
        state: GateState,
    },
    Ended {
        reason: EndReason,
    },
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct StepView {
    pub info: StepInfo,
    pub status: StepStatus,
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
                    })
                    .collect();
                self.gate_text = gate;
                self.gate = Some(GateState::Pending);
            }
            RunEvent::StepStarted { step } => {
                if let Some(view) = self.step_mut(&step) {
                    view.status = StepStatus::Running;
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
            RunEvent::Gate { state } => self.gate = Some(state),
            RunEvent::Ended { reason } => self.end = Some(reason),
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
        view.apply(2, RunEvent::StepStarted { step: "ci".into() });
        assert_eq!(view.step("ci").unwrap().status, StepStatus::Running);

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
    fn events_name_their_kind_on_the_wire() {
        let event = RunEvent::Ended {
            reason: EndReason::NotShippable,
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
