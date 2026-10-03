//! Evaluating a Pipeline against the current state of one Run: what the Gate
//! says, which Steps may start, and which are skipped and why.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::expr::{Env, Expr, StepTerm, Tri};
use crate::pipeline::{GATE, Pipeline, Step};
use crate::verdict::{GateState, Status, Verdict};

/// Facts about the PR a Condition can read, from the head-SHA snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrFacts {
    /// Paths changed by the PR.
    pub files: Vec<String>,
    pub labels: Vec<String>,
    pub base: String,
    pub draft: bool,
    pub author: String,
}

/// What a Run knows right now.
#[derive(Debug, Clone, Default)]
pub struct RunState {
    /// Each Step's status. A Step that isn't listed is pending.
    pub statuses: HashMap<String, Status>,
    /// Steps whose settled, non-pass Verdict a Waiver counts as pass.
    pub waived: HashSet<String>,
    pub pr: PrFacts,
    /// Consecutive Fix rounds this Run continues: 0 when an outside push
    /// started it, 1 when the first write-Step commit did, and so on.
    pub fix_round: u32,
}

/// What to do next with each pending Step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The Gate, counting the skips in this plan.
    pub gate: GateState,
    /// One decision per pending Step, in an order where each comes after
    /// what it reads.
    pub decisions: Vec<(String, Decision)>,
}

impl Plan {
    pub fn decision(&self, step: &str) -> Option<&Decision> {
        self.decisions
            .iter()
            .find(|(id, _)| id == step)
            .map(|(_, d)| d)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Its upstream has settled and its Condition is true.
    Start,
    /// Its Condition or upstream isn't determined yet.
    Wait,
    /// Its Condition is false: the daemon marks it skipped.
    Skip(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Under the default Condition, an upstream Step didn't pass.
    Upstream { step: String, verdict: Verdict },
    /// Under the default Condition, the Gate went the other way: a Step after
    /// the Gate needs it to pass, a write Step after it needs it to fail.
    Gate(GateState),
    /// The Condition the file wrote is false.
    Condition(String),
    /// The Fix round cap is reached, so write Steps don't run.
    RoundCap,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::Upstream { step, verdict } => {
                write!(f, "upstream Step `{step}` ended {verdict}")
            }
            SkipReason::Gate(state) => write!(f, "the Gate is {state}"),
            SkipReason::Condition(condition) => write!(f, "its Condition `{condition}` is false"),
            SkipReason::RoundCap => f.write_str("round cap"),
        }
    }
}

impl Pipeline {
    /// What the Gate says given the Run's state.
    pub fn gate(&self, state: &RunState) -> GateState {
        let mut env = Working::new(state);
        env.evaluate_gate(&self.gate);
        env.gate
    }

    /// Decides every pending Step. A skip counts as settled for the Steps
    /// after it, so one call returns the whole skip cascade.
    pub fn plan(&self, state: &RunState) -> Plan {
        let mut env = Working::new(state);
        let mut decisions = Vec::new();
        for node in &self.order {
            if node == GATE {
                env.evaluate_gate(&self.gate);
                continue;
            }
            let step = &self.steps[node];
            if state.statuses.get(node).copied().unwrap_or_default() != Status::Pending {
                continue;
            }
            let decision = self.decide(step, &env, state);
            if matches!(decision, Decision::Skip(_)) {
                env.statuses
                    .insert(node.as_str(), Status::Settled(Verdict::Skipped));
            }
            decisions.push((node.clone(), decision));
        }
        Plan {
            gate: env.gate,
            decisions,
        }
    }

    fn decide(&self, step: &Step, env: &Working<'_>, state: &RunState) -> Decision {
        if step.is_write() && state.fix_round >= self.fix_rounds {
            return Decision::Skip(SkipReason::RoundCap);
        }
        match step.condition().eval(env) {
            Tri::False => Decision::Skip(skip_reason(step, env)),
            Tri::True if upstream_settled(step, env) => Decision::Start,
            _ => Decision::Wait,
        }
    }
}

fn upstream_settled(step: &Step, env: &Working<'_>) -> bool {
    step.needs.iter().all(|need| {
        if need == GATE {
            env.gate != GateState::Pending
        } else {
            matches!(env.status(need), Status::Settled(_))
        }
    })
}

/// Why a false Condition is false. Under the default Condition that names the
/// upstream Step (or the Gate) that decided it.
fn skip_reason(step: &Step, env: &Working<'_>) -> SkipReason {
    if let Some(when) = &step.when {
        return SkipReason::Condition(when.to_string());
    }
    if step.is_write() && step.needs_gate() {
        return SkipReason::Gate(env.gate);
    }
    for need in &step.needs {
        if need == GATE {
            if env.gate == GateState::Fail {
                return SkipReason::Gate(GateState::Fail);
            }
            continue;
        }
        let term = StepTerm {
            id: need.clone(),
            accepts_skipped: false,
        };
        let status = env.status(need);
        if let (Tri::False, Status::Settled(verdict)) = (term.eval(status), status) {
            return SkipReason::Upstream {
                step: need.clone(),
                verdict,
            };
        }
    }
    SkipReason::Condition(step.condition().to_string())
}

/// The Run's state as one evaluation pass sees it, with skips decided so far
/// counted as settled.
struct Working<'a> {
    statuses: HashMap<&'a str, Status>,
    waived: &'a HashSet<String>,
    pr: &'a PrFacts,
    gate: GateState,
}

impl<'a> Working<'a> {
    fn new(state: &'a RunState) -> Working<'a> {
        Working {
            statuses: state
                .statuses
                .iter()
                .map(|(id, status)| (id.as_str(), *status))
                .collect(),
            waived: &state.waived,
            pr: &state.pr,
            gate: GateState::Pending,
        }
    }

    fn evaluate_gate(&mut self, terms: &[Expr]) {
        self.gate = match crate::expr::all(terms, self) {
            Tri::True => GateState::Pass,
            Tri::False => GateState::Fail,
            Tri::Unknown => GateState::Pending,
        };
    }
}

impl Env for Working<'_> {
    fn status(&self, id: &str) -> Status {
        match self.statuses.get(id).copied().unwrap_or_default() {
            Status::Settled(_) if self.waived.contains(id) => Status::Settled(Verdict::Pass),
            status => status,
        }
    }

    fn gate(&self) -> GateState {
        self.gate
    }

    fn pr(&self) -> &PrFacts {
        self.pr
    }
}
