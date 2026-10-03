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
pub enum Status {
    /// Not started yet.
    #[default]
    Pending,
    Running,
    Settled(Verdict),
}

impl Status {
    pub fn verdict(self) -> Option<Verdict> {
        match self {
            Status::Settled(v) => Some(v),
            _ => None,
        }
    }
}

/// What the Gate says. It is determined as early as its logic allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
