//! The `run/<id>` topic. Every Run has an append-only event journal, and the
//! topic replays it by sequence number: a client that subscribes from
//! scratch gets every event, and one that reconnects names the last
//! sequence number it saw and gets only what it missed (ADR 0010). Folding
//! the events in order with [`RunView::apply`] rebuilds the Run.

use std::fmt;

use serde::{Deserialize, Serialize};
use slopwatch_core::{EndReason, Expr, GateState, StepTerm, Verdict, WaiverCategory};

use crate::step::{Effect, EffectResult, Outputs, Usage};
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
        /// The Gate's terms, which hold together as an AND, for drawing
        /// the Gate node. Empty from a daemon that predates them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gate_terms: Vec<GateTerm>,
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
    /// A running Step's `usage` report for one model call.
    StepUsage {
        step: String,
        usage: Usage,
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
    /// The Step declares `workspace: write`, so it is terminal. Its commit
    /// ends the Run (ADR 0001).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub write: bool,
    /// The Condition the file wrote, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
}

/// One term of the Gate, as the Gate node lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateTerm {
    /// The Step must pass, or also be skipped when it accepts that.
    Step { id: String, accepts_skipped: bool },
    /// An `or:` block. One of its terms must hold.
    AnyOf { terms: Vec<GateTerm> },
    /// Any other expression, as the Pipeline file writes it.
    Other { text: String },
}

impl From<&Expr> for GateTerm {
    fn from(expr: &Expr) -> GateTerm {
        match expr {
            Expr::Step(StepTerm {
                id,
                accepts_skipped,
            }) => GateTerm::Step {
                id: id.clone(),
                accepts_skipped: *accepts_skipped,
            },
            Expr::Any(terms) => GateTerm::AnyOf {
                terms: terms.iter().map(GateTerm::from).collect(),
            },
            other => GateTerm::Other {
                text: other.to_string(),
            },
        }
    }
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
    pub gate_terms: Vec<GateTerm>,
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
    /// What the Step's model calls cost, over every attempt. `None` while
    /// it reported no usage.
    pub cost: Option<Cost>,
}

/// What a Step spent, at list price.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    /// The sum of the calls that reported a price.
    pub usd: f64,
    /// Some call reported no price, so the real cost is more than `usd`.
    pub unknown: bool,
}

impl Cost {
    pub fn add(&mut self, usage: &Usage) {
        match usage.usd {
            Some(usd) => self.usd += usd,
            None => self.unknown = true,
        }
    }
}

impl fmt::Display for Cost {
    /// `$0.0013`, `$0.0013 +?`, or `+?` when no call reported a price.
    /// Below a cent it keeps four decimals, so a Jev call doesn't read as
    /// free.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.unknown && self.usd == 0.0 {
            return f.write_str("+?");
        }
        if self.usd >= 0.01 || self.usd == 0.0 {
            write!(f, "${:.2}", self.usd)?;
        } else {
            write!(f, "${:.4}", self.usd.max(0.0001))?;
        }
        if self.unknown {
            f.write_str(" +?")?;
        }
        Ok(())
    }
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
                gate_terms,
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
                        cost: None,
                    })
                    .collect();
                self.gate_text = gate;
                self.gate_terms = gate_terms;
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
            RunEvent::StepUsage { step, usage } => {
                if let Some(view) = self.step_mut(&step) {
                    view.cost.get_or_insert_default().add(&usage);
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
    use slopwatch_core::{Expr, StepTerm};

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
                write: false,
                condition: None,
            }],
            gate: "[ci]".into(),
            gate_terms: vec![],
        }
    }

    #[test]
    fn usage_adds_up_into_the_steps_cost() {
        let usage = |usd| RunEvent::StepUsage {
            step: "ci".into(),
            usage: Usage {
                model: "typesafe-ai/jev".into(),
                input_tokens: 300,
                output_tokens: 0,
                usd,
            },
        };
        let mut view = RunView::default();
        view.apply(1, started());
        assert_eq!(view.step("ci").unwrap().cost, None);

        view.apply(2, usage(Some(0.0004)));
        view.apply(3, usage(Some(0.0009)));
        let cost = view.step("ci").unwrap().cost.unwrap();
        assert!((cost.usd - 0.0013).abs() < 1e-12);
        assert_eq!(cost.to_string(), "$0.0013");

        view.apply(4, usage(None));
        assert_eq!(
            view.step("ci").unwrap().cost.unwrap().to_string(),
            "$0.0013 +?"
        );
        assert_eq!(
            Cost {
                usd: 0.0,
                unknown: true
            }
            .to_string(),
            "+?"
        );
        assert_eq!(
            Cost {
                usd: 1.5,
                unknown: false
            }
            .to_string(),
            "$1.50"
        );
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
    fn gate_terms_keep_the_any_of_groups_and_spell_out_the_rest() {
        let step = |id: &str, accepts_skipped| {
            Expr::Step(StepTerm {
                id: id.into(),
                accepts_skipped,
            })
        };
        let gate = [
            step("ci", false),
            step("issue", true),
            Expr::Any(vec![step("review", false), step("human", false)]),
            Expr::Not(vec![step("lint", false)]),
        ];

        let terms: Vec<GateTerm> = gate.iter().map(GateTerm::from).collect();

        let term = |id: &str, accepts_skipped| GateTerm::Step {
            id: id.into(),
            accepts_skipped,
        };
        assert_eq!(
            terms,
            [
                term("ci", false),
                term("issue", true),
                GateTerm::AnyOf {
                    terms: vec![term("review", false), term("human", false)],
                },
                GateTerm::Other {
                    text: "{not: [lint]}".into(),
                },
            ]
        );
        assert_eq!(
            serde_json::to_value(&terms[2]).unwrap(),
            json!({ "kind": "any_of", "terms": [
                { "kind": "step", "id": "review", "accepts_skipped": false },
                { "kind": "step", "id": "human", "accepts_skipped": false },
            ] })
        );
    }

    #[test]
    fn a_started_event_from_an_older_daemon_reads_without_the_graph_fields() {
        let older: RunEvent = serde_json::from_value(json!({
            "kind": "started", "repo": "o/r", "number": 7, "head_sha": "abc",
            "base": "main", "base_sha": "def", "gate": "[ci]",
            "steps": [{ "id": "ci", "plugin": "ci", "needs": [], "gated": true }],
        }))
        .unwrap();

        let mut view = RunView::default();
        view.apply(1, older);

        let ci = &view.step("ci").unwrap().info;
        assert!(!ci.write);
        assert_eq!(ci.condition, None);
        assert!(view.gate_terms.is_empty());
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
