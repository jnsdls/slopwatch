use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::expr::{Expr, StepTerm};

/// `fix_rounds` when the Pipeline doesn't set it.
pub const FIX_ROUNDS_DEFAULT: u32 = 3;
/// The most `fix_rounds` a Pipeline may set.
pub const FIX_ROUNDS_CEILING: u32 = 10;
/// A Watched PR's Budget when the Pipeline doesn't set `budget_usd`, in
/// list-price US dollars.
pub const PR_BUDGET_DEFAULT: f64 = 10.0;
/// Built-in Plugin names. A third-party Plugin can't take one.
pub const BUILTIN_PLUGINS: &[&str] = &["jev", "ci", "claude", "codex", "fix", "human", "merge"];
/// The id that names the Gate in `needs` and Conditions.
pub const GATE: &str = "gate";

/// A loaded, validated Pipeline.
#[derive(Debug, Clone)]
pub struct Pipeline {
    pub(crate) steps: BTreeMap<String, Step>,
    pub(crate) gate: Vec<Expr>,
    pub(crate) fix_rounds: u32,
    /// What Steps may spend on one Watched PR since its last outside push,
    /// in list-price US dollars.
    pub(crate) budget_usd: f64,
    /// Every Step id plus [`GATE`], each after everything it reads.
    pub(crate) order: Vec<String>,
}

impl Pipeline {
    pub fn steps(&self) -> impl Iterator<Item = &Step> {
        self.steps.values()
    }

    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.get(id)
    }

    /// Every Step, each after the Steps it needs.
    pub fn ordered_steps(&self) -> impl Iterator<Item = &Step> {
        self.order.iter().filter_map(|id| self.steps.get(id))
    }

    /// Whether the Gate reads Step `id`. A Step it doesn't read is advisory.
    pub fn gate_reads(&self, id: &str) -> bool {
        let mut found = false;
        for term in &self.gate {
            term.walk(&mut |expr| {
                if let Expr::Step(StepTerm { id: read, .. }) = expr {
                    found |= read == id;
                }
            });
        }
        found
    }

    /// The Gate's terms, which hold together as an AND.
    pub fn gate_terms(&self) -> &[Expr] {
        &self.gate
    }

    pub fn fix_rounds(&self) -> u32 {
        self.fix_rounds
    }

    /// The Watched PR's Budget, in list-price US dollars.
    pub fn budget_usd(&self) -> f64 {
        self.budget_usd
    }
}

/// One Step of a Pipeline, with its Library Step and `with:` overrides
/// already resolved.
#[derive(Debug, Clone)]
pub struct Step {
    /// The Pipeline id, the Step's key under `steps:`.
    pub id: String,
    /// What the Pipeline node wrote in `uses:`.
    pub uses: Uses,
    /// The Plugin the Step runs, after resolving any Library Step.
    pub plugin: String,
    pub workspace: Workspace,
    pub builtin: bool,
    /// The Library Step's `with:` overlaid key by key with the node's.
    pub config: Map<String, Value>,
    pub timeout: Option<Duration>,
    pub stall_after: Option<Duration>,
    /// What the Step may spend in one attempt, in list-price US dollars,
    /// as the Pipeline or its Library Step sets it. `None` leaves its
    /// Plugin's default.
    pub budget_usd: Option<f64>,
    /// Step ids, or [`GATE`], this Step reads and starts after.
    pub needs: Vec<String>,
    /// The Condition the file wrote, if any.
    pub when: Option<Expr>,
    pub(crate) default_condition: Expr,
}

impl Step {
    /// A Step that declares `workspace: write` is terminal.
    pub fn is_write(&self) -> bool {
        self.workspace == Workspace::Write
    }

    pub fn is_merge(&self) -> bool {
        self.builtin && self.plugin == "merge"
    }

    pub fn needs_gate(&self) -> bool {
        self.needs.iter().any(|n| n == GATE)
    }

    /// The Condition in force: the one the file wrote, or the default.
    pub fn condition(&self) -> &Expr {
        self.when.as_ref().unwrap_or(&self.default_condition)
    }

    /// The default Condition is "every upstream Step passed". A write Step
    /// that needs the Gate defaults to `not: [gate]` instead, so a fixer
    /// starts once the Gate fails; its other needs only have to settle.
    pub(crate) fn default_condition_for(needs: &[String], workspace: Workspace) -> Expr {
        if workspace == Workspace::Write && needs.iter().any(|n| n == GATE) {
            return Expr::Not(vec![Expr::Gate]);
        }
        Expr::All(
            needs
                .iter()
                .map(|n| {
                    if n == GATE {
                        Expr::Gate
                    } else {
                        Expr::Step(StepTerm {
                            id: n.clone(),
                            accepts_skipped: false,
                        })
                    }
                })
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Uses {
    /// A bare name: a Plugin used inline.
    Plugin(String),
    /// `lib/<name>`: a Library Step.
    Library(String),
}

/// The workspace a Plugin's manifest declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Workspace {
    None,
    Read,
    Write,
}
