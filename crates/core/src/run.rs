//! Evaluating a Pipeline against the current state of one Run: what the Gate
//! says, which Steps may start, and which are skipped and why.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;

use crate::expr::{Env, Expr, StepTerm, Tri};
use crate::pipeline::{GATE, Pipeline, Step};
use crate::verdict::{GateState, StepState, Verdict};

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
    /// Each Step's state. A Step that isn't listed is pending.
    pub steps: HashMap<String, StepState>,
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
    /// What the Gate says given the Run's state, counting the skips the
    /// state already implies. The same as [`Plan::gate`].
    pub fn gate(&self, state: &RunState) -> GateState {
        self.plan(state).gate
    }

    /// The Steps a Gate override waives, sorted: in every Gate term that
    /// fails, each Step that makes it fail and whose settled Verdict a
    /// Waiver may still cover. A Step under `not:` is left alone, since
    /// counting it as pass can't help that term.
    pub fn failing_waivable_steps(&self, state: &RunState) -> Vec<String> {
        let env = self.evaluate(state).0;
        let mut waives = BTreeSet::new();
        for term in &self.gate {
            collect_failing(term, &env, state, &mut waives);
        }
        waives.into_iter().collect()
    }

    /// Whether the Gate passes only because of the Run's Waivers.
    pub fn passes_by_waiver(&self, state: &RunState) -> bool {
        let unwaived = RunState {
            waived: HashSet::new(),
            ..state.clone()
        };
        self.gate(state) == GateState::Pass && self.gate(&unwaived) != GateState::Pass
    }

    /// Decides every pending Step. A skip counts as settled for the Steps
    /// after it, so one call returns the whole skip cascade.
    pub fn plan(&self, state: &RunState) -> Plan {
        let (env, decisions) = self.evaluate(state);
        Plan {
            gate: env.gate,
            decisions,
        }
    }

    /// One evaluation pass: the decisions for pending Steps, and the Run's
    /// state with the skips they imply.
    fn evaluate<'a>(&'a self, state: &'a RunState) -> (Working<'a>, Vec<(String, Decision)>) {
        let mut env = Working::new(state);
        let mut decisions = Vec::new();
        for node in &self.order {
            if node == GATE {
                env.evaluate_gate(&self.gate);
                continue;
            }
            let step = &self.steps[node];
            if state.steps.get(node).copied().unwrap_or_default() != StepState::Pending {
                continue;
            }
            let decision = self.decide(step, &env, state);
            if matches!(decision, Decision::Skip(_)) {
                env.steps
                    .insert(node.as_str(), StepState::Settled(Verdict::Skipped));
            }
            decisions.push((node.clone(), decision));
        }
        (env, decisions)
    }

    fn decide(&self, step: &Step, env: &Working<'_>, state: &RunState) -> Decision {
        match step.condition().eval(env) {
            Tri::False => Decision::Skip(skip_reason(step, env)),
            Tri::True if step.is_write() && state.fix_round >= self.fix_rounds => {
                Decision::Skip(SkipReason::RoundCap)
            }
            Tri::True if upstream_settled(step, env) => Decision::Start,
            _ => Decision::Wait,
        }
    }
}

/// Adds the waivable Steps that make `term` false. `and`/`or` fail only
/// through their failing parts; `not:` fails through passing ones, which a
/// Waiver can't change.
fn collect_failing(
    term: &Expr,
    env: &Working<'_>,
    state: &RunState,
    waives: &mut BTreeSet<String>,
) {
    if term.eval(env) != Tri::False {
        return;
    }
    match term {
        Expr::Step(StepTerm { id, .. }) => {
            let waivable = matches!(
                state.steps.get(id),
                Some(StepState::Settled(verdict)) if *verdict != Verdict::Pass
            );
            if waivable && !state.waived.contains(id) {
                waives.insert(id.clone());
            }
        }
        Expr::All(items) | Expr::Any(items) => {
            for item in items {
                collect_failing(item, env, state, waives);
            }
        }
        _ => {}
    }
}

fn upstream_settled(step: &Step, env: &Working<'_>) -> bool {
    step.needs.iter().all(|need| {
        if need == GATE {
            env.gate != GateState::Pending
        } else {
            matches!(env.step_state(need), StepState::Settled(_))
        }
    })
}

/// Why a false Condition is false. Under the default Condition that names the
/// term that decided it: an upstream Step, or the Gate.
fn skip_reason(step: &Step, env: &Working<'_>) -> SkipReason {
    let default = match (&step.when, step.condition()) {
        (None, Expr::All(terms)) => terms.as_slice(),
        (None, not_gate @ Expr::Not(_)) => std::slice::from_ref(not_gate),
        _ => return SkipReason::Condition(step.condition().to_string()),
    };
    for term in default {
        if term.eval(env) != Tri::False {
            continue;
        }
        return match term {
            Expr::Step(StepTerm { id, .. }) => SkipReason::Upstream {
                step: id.clone(),
                verdict: env.step_state(id).verdict().unwrap_or(Verdict::Missing),
            },
            _ => SkipReason::Gate(env.gate),
        };
    }
    SkipReason::Condition(step.condition().to_string())
}

/// The Run's state as one evaluation pass sees it, with skips decided so far
/// counted as settled.
struct Working<'a> {
    steps: HashMap<&'a str, StepState>,
    waived: &'a HashSet<String>,
    pr: &'a PrFacts,
    gate: GateState,
}

impl<'a> Working<'a> {
    fn new(state: &'a RunState) -> Working<'a> {
        Working {
            steps: state
                .steps
                .iter()
                .map(|(id, state)| (id.as_str(), *state))
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
    fn step_state(&self, id: &str) -> StepState {
        match self.steps.get(id).copied().unwrap_or_default() {
            StepState::Settled(_) if self.waived.contains(id) => StepState::Settled(Verdict::Pass),
            state => state,
        }
    }

    fn gate(&self) -> GateState {
        self.gate
    }

    fn pr(&self) -> &PrFacts {
        self.pr
    }
}
