//! The Step contract (ADR 0003): what the daemon and one Step process say
//! to each other.
//!
//! A Plugin executable answers `<exe> describe` with its [`Manifest`] as one
//! JSON object on stdout. `<exe> run` starts a session: one JSON object per
//! line both ways over stdio. The daemon sends [`ToStep::Start`] first, then
//! [`ToStep::PrUpdated`] and [`ToStep::Cancel`] as they happen. The Step
//! sends [`FromStep`] messages, exactly one of them an outcome, then exits.
//! Stdout carries protocol messages only. Stderr is free-form and goes to
//! the Step log.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use slopwatch_core::{Verdict, Workspace};

use crate::RepoName;
use crate::runs::RunId;

/// The Step protocol dialect. A Plugin's manifest must name exactly this
/// one. Additions an older peer can ignore are feature strings instead.
pub const STEP_DIALECT: u32 = 1;

/// What `<exe> describe` prints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// The Plugin's name, the word a Pipeline writes in `uses:`.
    pub id: String,
    pub version: String,
    pub dialect: u32,
    #[serde(default)]
    pub features: Vec<String>,
    /// JSON Schema for the Step's `with:`.
    #[serde(default)]
    pub config_schema: Value,
    pub workspace: Workspace,
    /// Effects the Step may request.
    #[serde(default)]
    pub effects: Vec<String>,
    /// Env var names of the Secrets the Step needs.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Defaults a Pipeline may override, written like `90m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_after: Option<String>,
    /// At most this many of the Plugin's Steps run at once, across every
    /// Run. `None` leaves only the daemon's global cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
}

/// A message from the daemon to a Step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToStep {
    Start(Start),
    /// The daemon's poll saw the PR change. The snapshot is still pinned to
    /// the Run's head SHA: a push ends the Run instead.
    PrUpdated {
        snapshot: PrSnapshot,
    },
    /// Stop now. Whatever the Step reports afterwards, its Verdict is
    /// cancelled.
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Start {
    pub run: RunId,
    /// The Step's id in the Pipeline.
    pub step: String,
    /// The Step's resolved `with:`.
    pub config: Map<String, Value>,
    pub snapshot: PrSnapshot,
    /// The Outcomes of every Step upstream of this one, by Step id.
    pub upstream: BTreeMap<String, Outcome>,
}

/// A message from a Step to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromStep {
    /// A heartbeat, with an optional line for the PR pane.
    Progress {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// A line for the Step log.
    Log { message: String },
    /// The Step's one Outcome. It exits after sending it.
    Outcome(Outcome),
}

/// What one Step reports for one Run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub verdict: Verdict,
    #[serde(default)]
    pub outputs: Outputs,
}

impl Outcome {
    pub fn new(verdict: Verdict) -> Self {
        Self {
            verdict,
            outputs: Outputs::default(),
        }
    }
}

/// An Outcome's named outputs. A Step keeps raw values here and applies its
/// own threshold to get its Verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Outputs {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probability: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
}

/// One specific problem a Step reports about the PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// The PR as the daemon last polled it, pinned to the Run's head SHA. A
/// wider snapshot, such as the diff, is a feature string (ADR 0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSnapshot {
    pub repo: RepoName,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub author: String,
    pub head_sha: String,
    /// The base branch's name.
    pub base: String,
    pub draft: bool,
    pub labels: Vec<String>,
    pub checks: Checks,
}

/// The checks GitHub reports on the head commit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checks {
    /// GitHub's rollup over every check and status.
    pub state: ChecksState,
    pub runs: Vec<Check>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    /// The commit has no checks yet.
    #[default]
    None,
    Pending,
    /// Every check passed, or ended neutral or skipped.
    Success,
    Failure,
}

/// One check run or commit status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Pending,
    Success,
    Failure,
    Neutral,
    Skipped,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_to_a_step_are_one_tagged_object_each() {
        let start = ToStep::Start(Start {
            run: RunId(4),
            step: "ci".into(),
            config: Map::new(),
            snapshot: PrSnapshot {
                repo: RepoName::new("o", "r"),
                number: 7,
                title: "Add the thing".into(),
                body: String::new(),
                url: String::new(),
                author: "me".into(),
                head_sha: "abc".into(),
                base: "main".into(),
                draft: false,
                labels: vec![],
                checks: Checks::default(),
            },
            upstream: BTreeMap::new(),
        });

        let wire = serde_json::to_value(&start).unwrap();

        assert_eq!(wire["type"], "start");
        assert_eq!(wire["run"], 4);
        assert_eq!(wire["snapshot"]["head_sha"], "abc");
        assert_eq!(wire["snapshot"]["checks"]["state"], "none");
        assert_eq!(
            serde_json::to_value(ToStep::Cancel).unwrap(),
            json!({ "type": "cancel" })
        );
    }

    #[test]
    fn an_outcome_carries_a_verdict_and_outputs() {
        let line = r#"{"type":"outcome","verdict":"fail","outputs":{"findings":[{"severity":"error","message":"test failed"}]}}"#;

        let FromStep::Outcome(outcome) = serde_json::from_str(line).unwrap() else {
            panic!("expected an outcome");
        };

        assert_eq!(outcome.verdict, Verdict::Fail);
        assert_eq!(outcome.outputs.findings[0].message, "test failed");
        assert_eq!(outcome.outputs.findings[0].severity, Severity::Error);
    }

    #[test]
    fn a_manifest_reads_with_defaults_for_what_it_leaves_out() {
        let manifest: Manifest = serde_json::from_value(json!({
            "id": "ci", "version": "1.0.0", "dialect": 1, "workspace": "none",
        }))
        .unwrap();

        assert_eq!(manifest.workspace, Workspace::None);
        assert!(manifest.effects.is_empty());
        assert_eq!(manifest.timeout, None);
    }
}
