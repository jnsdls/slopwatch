use std::fmt;

use serde::{Deserialize, Serialize};

/// The one-word part of an Outcome. A Step reports `pass`, `fail` or
/// `inconclusive`; the daemon assigns the rest when the Step didn't report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
    Inconclusive,
    Error,
    Cancelled,
    Skipped,
    Missing,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Inconclusive => "inconclusive",
            Verdict::Error => "error",
            Verdict::Cancelled => "cancelled",
            Verdict::Skipped => "skipped",
            Verdict::Missing => "missing",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Verdict> {
        Some(match s {
            "pass" => Verdict::Pass,
            "fail" => Verdict::Fail,
            "inconclusive" => Verdict::Inconclusive,
            "error" => Verdict::Error,
            "cancelled" => Verdict::Cancelled,
            "skipped" => Verdict::Skipped,
            "missing" => Verdict::Missing,
            _ => return None,
        })
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where one Step stands within a Run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepState {
    /// Not started yet.
    #[default]
    Pending,
    Running,
    Settled(Verdict),
}

impl StepState {
    pub fn verdict(self) -> Option<Verdict> {
        match self {
            StepState::Settled(v) => Some(v),
            _ => None,
        }
    }
}

/// What the Gate says. It is determined as early as its logic allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateState {
    Pass,
    Fail,
    Pending,
}

impl fmt::Display for GateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            GateState::Pass => "pass",
            GateState::Fail => "fail",
            GateState::Pending => "pending",
        })
    }
}

/// Why a Run stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The Merge Step merged the PR.
    Merged,
    /// The Gate passed and the Pipeline has no Merge Step.
    Shippable,
    NotShippable,
    /// slopwatch pushed to the PR, such as a Fix commit or a Merge rebase.
    Pushed,
    /// A push or a base change from outside the Run.
    Superseded,
    /// Merged or closed on GitHub by someone else.
    Closed,
    /// The Watched PR's or the day's Budget ran out.
    OverBudget,
    /// The developer ended it.
    Cancelled,
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EndReason::Merged => "merged",
            EndReason::Shippable => "shippable",
            EndReason::NotShippable => "not shippable",
            EndReason::Pushed => "pushed",
            EndReason::Superseded => "superseded",
            EndReason::Closed => "closed",
            EndReason::OverBudget => "over budget",
            EndReason::Cancelled => "cancelled",
        })
    }
}
