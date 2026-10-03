//! The Step contract (ADR 0003): what the daemon and one Step process say
//! to each other.
//!
//! A Plugin executable answers `<exe> describe` with its [`Manifest`] as one
//! JSON object on stdout. `<exe> run` starts a session: one JSON object per
//! line both ways over stdio. The daemon sends [`ToStep::Start`] first, then
//! [`ToStep::PrUpdated`] and [`ToStep::Cancel`] as they happen. The Step
//! sends [`FromStep`] messages, exactly one of them an outcome, then exits.
//! A Step changes GitHub only by sending [`FromStep::Effect`], and the
//! daemon answers each with [`ToStep::EffectResult`]. The built-in `human`
//! Plugin sends [`FromStep::Ask`] to put a question to the developer, and
//! the daemon passes their [`ToStep::Answer`] on.
//! Stdout carries protocol messages only. Stderr is free-form and goes to
//! the Step log.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use slopwatch_core::{Verdict, Workspace};

use crate::logs::LogLevel;
use crate::runs::RunId;
use crate::{Actor, Answer, RepoName};

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
    /// Effects the Step may request. Requesting any other is a protocol
    /// error.
    #[serde(default)]
    pub effects: Vec<EffectKind>,
    /// The Secrets the Step gets as env vars, by env var name. Written as
    /// a plain name for a required Secret, or `{"name": ..., "optional":
    /// true}`. A Step whose required Secret isn't set errors without
    /// spawning.
    #[serde(default)]
    pub secrets: Vec<SecretSpec>,
    /// Defaults a Pipeline may override, written like `90m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_after: Option<String>,
    /// What one of its Steps may spend in one attempt, in list-price US
    /// dollars, unless the Pipeline sets `budget_usd`. `None` leaves its
    /// Steps without a Budget of their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// At most this many of the Plugin's Steps run at once, across every
    /// Run. `None` leaves only the daemon's global cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
}

/// A Secret a manifest asks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretSpec {
    /// The env var name the Step reads it from, and the Secret's name.
    pub name: String,
    /// The Step runs without it, and checks for itself whether it got it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl SecretSpec {
    pub fn required(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            optional: false,
        }
    }

    pub fn optional(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            optional: true,
        }
    }
}

impl<'de> Deserialize<'de> for SecretSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            Name(String),
            Spec {
                name: String,
                #[serde(default)]
                optional: bool,
            },
        }
        Ok(match Written::deserialize(deserializer)? {
            Written::Name(name) => SecretSpec::required(name),
            Written::Spec { name, optional } => SecretSpec { name, optional },
        })
    }
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
    /// What became of the Effect the Step requested under `id`.
    EffectResult {
        id: String,
        result: EffectResult,
    },
    /// The developer's answer to the Step's [`FromStep::Ask`], with the
    /// note they wrote, if any, and who answered.
    Answer {
        answer: Answer,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        actor: Actor,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Start {
    pub run: RunId,
    /// The Step's id in the Pipeline.
    pub step: String,
    /// The Step's resolved `with:`.
    pub config: Map<String, Value>,
    pub snapshot: PrSnapshot,
    /// The Outcomes of every Step upstream of this one, by Step id. For a
    /// Step that needs the Gate, that includes every Step the Gate reads.
    pub upstream: BTreeMap<String, Outcome>,
    /// For a Step that needs the Gate: the Steps behind the Gate's failing
    /// terms that no Waiver covers, which a fixer acts on. Empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate_failing: Vec<String>,
    /// The log tails of the head commit's failed GitHub Actions jobs, for a
    /// Step whose manifest lists [`CI_LOGS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ci_logs: Vec<CiLog>,
    /// The most the Step may spend, in list-price USD: the tightest of its
    /// own Budget and what's left of the PR's and the day's. A Step that
    /// can cap its own spend, as `claude --max-budget-usd` does, should.
    /// The daemon stops it once its reported usage crosses the line
    /// anyway. `None` sets no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
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
    /// What one call to a model cost. A Step sends one per call, as it
    /// goes, so usage before a cancel or a crash still counts.
    Usage(Usage),
    /// A line for the Step log.
    Log {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<LogLevel>,
    },
    /// Asks the daemon to change something on GitHub. `id` is the Step's
    /// own name for the request, unique within the Step. Sending the same
    /// `id` and Effect again, as a Step respawned after a daemon restart
    /// does, doesn't repeat the change: the answer is the first request's
    /// result. The same `id` with a different Effect is refused.
    Effect { id: String, effect: Effect },
    /// Puts `prompt` to the developer as a Human Step in the Inbox. The
    /// daemon sends [`ToStep::Answer`] once they approve or reject. Asking
    /// again, as a Step respawned after a daemon restart does, keeps the
    /// entry that's already open. Only the built-in `human` Plugin may ask,
    /// and any other Step that does gets `error(protocol)`.
    Ask { prompt: String },
    /// The Step couldn't judge the PR, as when the CLI it drives isn't
    /// logged in. The daemon settles it `error` with `reason`. It takes
    /// the place of an Outcome, and the Step exits after sending it.
    Error { reason: String },
    /// The Step's one Outcome. It exits after sending it.
    Outcome(Outcome),
}

/// What one model call used. `usd` is the list price when the provider
/// reports it. Without it the daemon prices the tokens from its own table,
/// and a model the table doesn't know leaves the cost unknown, never zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub model: String,
    /// Every input token, those read from a prompt cache included.
    #[serde(default)]
    pub input_tokens: u64,
    /// The share of `input_tokens` read from a prompt cache, which costs
    /// less.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// A change on GitHub that a Step asks for and the daemon carries out with
/// the developer's token, only while the Step's Run is still current
/// (ADR 0003). The list is closed: anything else is a protocol error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Posts a comment on the PR.
    Comment { body: String },
    /// Adds a label to the PR, or removes it.
    Label {
        name: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        remove: bool,
    },
    /// Reruns the failed GitHub Actions job `job` behind the check named
    /// `check` on the Run's head commit (see [`Check::actions_job`]). Each
    /// check gets one rerun per head SHA (ADR 0008).
    Rerun { check: String, job: u64 },
    /// Brings the PR's branch up to date with its base through GitHub's
    /// `updatePullRequestBranch`, expecting the Run's head SHA (ADR 0004).
    /// The push it makes ends the Run as pushed.
    Rebase { method: UpdateMethod },
    /// Lands the PR through GitHub's async merge API, directly or through
    /// the base's merge queue (ADR 0011). The daemon sends the Run's head
    /// SHA with it, so only the SHA the Gate judged can land. `method`
    /// applies to a direct merge; a merge queue uses its own.
    Merge {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<MergeMethod>,
    },
}

impl Effect {
    pub fn kind(&self) -> EffectKind {
        match self {
            Effect::Comment { .. } => EffectKind::Comment,
            Effect::Label { .. } => EffectKind::Label,
            Effect::Rerun { .. } => EffectKind::Rerun,
            Effect::Rebase { .. } => EffectKind::Rebase,
            Effect::Merge { .. } => EffectKind::Merge,
        }
    }
}

/// How the `rebase` Effect brings a branch up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMethod {
    /// Replays the PR's commits onto the base. GitHub can't sign them.
    Rebase,
    /// Merges the base into the PR, in a commit GitHub signs.
    Merge,
}

/// How a direct merge lands the PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl std::fmt::Display for MergeMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MergeMethod::Merge => "merge",
            MergeMethod::Squash => "squash",
            MergeMethod::Rebase => "rebase",
        })
    }
}

/// An [`Effect`] by name, as a manifest declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    Comment,
    Label,
    Rerun,
    Rebase,
    Merge,
}

impl std::fmt::Display for EffectKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EffectKind::Comment => "comment",
            EffectKind::Label => "label",
            EffectKind::Rerun => "rerun",
            EffectKind::Rebase => "rebase",
            EffectKind::Merge => "merge",
        })
    }
}

/// What became of a requested Effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EffectResult {
    /// GitHub has it. For `merge`, the PR is merged.
    Done,
    /// For `merge`: the base's merge queue took the PR, and GitHub merges
    /// it once the queue's checks pass. The snapshot's [`MergeState`]
    /// follows it from there.
    Enqueued,
    /// The Step's Run had ended, or the Step had settled, so the daemon
    /// didn't carry it out.
    Dropped { reason: String },
    /// The daemon won't carry it out, such as a second rerun of one check
    /// on one SHA.
    Refused { reason: String },
    /// GitHub refused it or couldn't be reached. The daemon doesn't retry.
    Failed { reason: String },
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

/// How bad a Finding is, least first, so `>=` reads "at least as bad".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        })
    }
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
    /// Where the PR stands for merging. The daemon asks GitHub for it only
    /// while a Step whose manifest lists [`MERGE_STATE`] runs, so it's
    /// `None` until then, and for a while after such a Step starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeState>,
    /// A file holding the PR's unified diff against where it branched from
    /// its base, at the Run's head SHA. The daemon writes it when the Run
    /// starts, only if a Step in the Pipeline lists [`PR_DIFF`], since a
    /// diff can run to megabytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<PathBuf>,
    /// The issues the PR says it closes, as GitHub links them, read when
    /// the Run starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_issues: Vec<LinkedIssue>,
    /// The open PR whose head branch is this PR's base, when the PR is in a
    /// Stack and not at its bottom. Such a PR doesn't merge until its parent
    /// has (ADR 0011).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stacked_on: Option<u64>,
}

/// The manifest feature that asks for [`PrSnapshot::merge`].
pub const MERGE_STATE: &str = "merge_state";

/// The manifest feature that asks for [`PrSnapshot::diff`].
pub const PR_DIFF: &str = "pr_diff";

/// The manifest feature that asks for [`Start::ci_logs`].
pub const CI_LOGS: &str = "ci_logs";

/// The end of a failed GitHub Actions job's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiLog {
    /// The check's name, as [`Check::name`] has it.
    pub check: String,
    /// The Actions job.
    pub job: u64,
    /// The log's last lines.
    pub tail: String,
}

/// The env var a Step gets the config directory the developer set for its
/// Plugin in, when they set one. The Plugin hands it to the CLI it runs.
pub const CONFIG_DIR_ENV: &str = "SLOPWATCH_CONFIG_DIR";

/// An issue the PR closes when it merges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedIssue {
    /// The issue's repo, which can differ from the PR's.
    pub repo: RepoName,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
}

/// What GitHub says about merging the PR at the Run's head SHA.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeState {
    /// The PR is merged. A merged PR leaves the poll, so this is how a
    /// Step hears that the merge queue landed it.
    #[serde(default)]
    pub merged: bool,
    /// GitHub's `mergeStateStatus`.
    pub status: MergeStatus,
    /// The head conflicts with the base.
    #[serde(default)]
    pub conflicts: bool,
    /// Commits on the base that the head doesn't have. `None` when GitHub
    /// couldn't compare them, as after the base branch was deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind_by: Option<u64>,
    /// The base branch has a merge queue.
    #[serde(default)]
    pub merge_queue: bool,
    #[serde(default)]
    pub in_merge_queue: bool,
    /// The base branch only takes signed commits.
    #[serde(default)]
    pub requires_signatures: bool,
    /// The direct merge methods the repo allows.
    #[serde(default)]
    pub methods: Vec<MergeMethod>,
}

/// GitHub's `mergeStateStatus` for a PR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStatus {
    /// Mergeable, and every requirement is met.
    Clean,
    /// Mergeable, with a check outside the requirements failing.
    Unstable,
    /// Mergeable, with pre-receive hooks.
    HasHooks,
    /// Out of date, on a branch that requires an up-to-date head.
    Behind,
    /// Something outside the Run holds it, such as a missing review.
    Blocked,
    /// Conflicts with the base.
    Dirty,
    /// The PR is a draft.
    Draft,
    /// GitHub hasn't worked it out yet.
    #[default]
    Unknown,
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
    /// The GitHub Actions job behind a check run, which the `rerun` Effect
    /// reruns. `None` for other apps' check runs and for commit statuses,
    /// which the developer's token can't rerun.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions_job: Option<u64>,
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
                merge: None,
                diff: None,
                linked_issues: vec![],
                stacked_on: None,
            },
            upstream: BTreeMap::new(),
            gate_failing: vec![],
            ci_logs: vec![],
            budget_usd: None,
        });

        let wire = serde_json::to_value(&start).unwrap();

        assert_eq!(wire["type"], "start");
        assert!(wire.get("budget_usd").is_none(), "no budget, no key");
        assert!(wire.get("gate_failing").is_none() && wire.get("ci_logs").is_none());
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
    fn usage_reads_with_or_without_a_price() {
        let priced =
            r#"{"type":"usage","model":"typesafe-ai/jev","input_tokens":303,"usd":0.0000127}"#;
        let unpriced = r#"{"type":"usage","model":"gpt-5","input_tokens":10,"output_tokens":4}"#;

        let FromStep::Usage(priced) = serde_json::from_str(priced).unwrap() else {
            panic!("expected usage");
        };
        let FromStep::Usage(unpriced) = serde_json::from_str(unpriced).unwrap() else {
            panic!("expected usage");
        };

        assert_eq!(priced.usd, Some(0.0000127));
        assert_eq!(priced.output_tokens, 0);
        assert_eq!(unpriced.usd, None);
        assert_eq!(unpriced.output_tokens, 4);
        assert_eq!(unpriced.cached_input_tokens, 0);
    }

    #[test]
    fn a_step_reports_its_own_error_with_a_reason() {
        let error: FromStep =
            serde_json::from_str(r#"{"type":"error","reason":"Not logged in"}"#).unwrap();

        assert_eq!(
            error,
            FromStep::Error {
                reason: "Not logged in".into()
            }
        );
        assert!(Severity::Error > Severity::Warning);
        assert_eq!(Severity::Warning.to_string(), "warning");
    }

    #[test]
    fn a_snapshot_carries_the_diff_file_and_the_linked_issues() {
        let snapshot: PrSnapshot = serde_json::from_value(json!({
            "repo": "o/r", "number": 1, "title": "", "body": "", "url": "",
            "author": "me", "head_sha": "abc", "base": "main", "draft": false,
            "labels": [], "checks": { "state": "none", "runs": [] },
            "diff": "/data/worktrees/4/pr.diff",
            "linked_issues": [{ "repo": "o/r", "number": 3, "title": "Crash", "body": "It crashes", "url": "u" }],
        }))
        .unwrap();

        assert_eq!(
            snapshot.diff.as_deref(),
            Some(std::path::Path::new("/data/worktrees/4/pr.diff"))
        );
        assert_eq!(snapshot.linked_issues[0].number, 3);
        assert_eq!(snapshot.linked_issues[0].repo, RepoName::new("o", "r"));
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
        assert!(manifest.secrets.is_empty());
    }

    #[test]
    fn a_manifest_names_a_required_secret_plainly_and_an_optional_one_with_a_flag() {
        let manifest: Manifest = serde_json::from_value(json!({
            "id": "jev", "version": "1", "dialect": 1, "workspace": "none",
            "secrets": ["JEV_API_KEY", { "name": "EXTRA", "optional": true }, { "name": "B" }],
        }))
        .unwrap();

        assert_eq!(
            manifest.secrets,
            vec![
                SecretSpec::required("JEV_API_KEY"),
                SecretSpec::optional("EXTRA"),
                SecretSpec::required("B"),
            ]
        );
        assert_eq!(
            serde_json::to_value(&manifest.secrets).unwrap(),
            json!([{ "name": "JEV_API_KEY" }, { "name": "EXTRA", "optional": true }, { "name": "B" }])
        );
    }

    #[test]
    fn an_effect_request_names_its_kind_and_the_result_its_status() {
        let line = r#"{"type":"effect","id":"rerun:test","effect":{"kind":"rerun","check":"test","job":42}}"#;

        let request: FromStep = serde_json::from_str(line).unwrap();

        assert_eq!(
            request,
            FromStep::Effect {
                id: "rerun:test".into(),
                effect: Effect::Rerun {
                    check: "test".into(),
                    job: 42,
                },
            }
        );
        assert_eq!(
            serde_json::to_value(ToStep::EffectResult {
                id: "c".into(),
                result: EffectResult::Refused {
                    reason: "no".into()
                },
            })
            .unwrap(),
            json!({
                "type": "effect_result",
                "id": "c",
                "result": { "status": "refused", "reason": "no" },
            })
        );
    }

    #[test]
    fn merge_and_rebase_are_effects_and_a_merge_can_come_back_enqueued() {
        let merge: FromStep = serde_json::from_str(
            r#"{"type":"effect","id":"merge","effect":{"kind":"merge","method":"squash"}}"#,
        )
        .unwrap();
        let rebase: FromStep = serde_json::from_str(
            r#"{"type":"effect","id":"rebase","effect":{"kind":"rebase","method":"merge"}}"#,
        )
        .unwrap();

        let FromStep::Effect { effect: merge, .. } = merge else {
            panic!("expected an Effect");
        };
        assert_eq!(
            merge,
            Effect::Merge {
                method: Some(MergeMethod::Squash)
            }
        );
        assert_eq!(merge.kind(), EffectKind::Merge);
        let FromStep::Effect { effect: rebase, .. } = rebase else {
            panic!("expected an Effect");
        };
        assert_eq!(
            rebase,
            Effect::Rebase {
                method: UpdateMethod::Merge
            }
        );
        assert_eq!(
            serde_json::to_value(EffectResult::Enqueued).unwrap(),
            json!({ "status": "enqueued" })
        );
    }

    #[test]
    fn a_snapshot_without_a_merge_state_reads_and_writes_without_one() {
        let snapshot: PrSnapshot = serde_json::from_value(json!({
            "repo": "o/r", "number": 1, "title": "", "body": "", "url": "",
            "author": "me", "head_sha": "abc", "base": "main", "draft": false,
            "labels": [], "checks": { "state": "none", "runs": [] },
        }))
        .unwrap();

        assert_eq!(snapshot.merge, None);
        assert_eq!(snapshot.diff, None);
        assert!(snapshot.linked_issues.is_empty());
        let wire = serde_json::to_value(&snapshot).unwrap();
        for key in ["merge", "diff", "linked_issues"] {
            assert!(wire.get(key).is_none(), "{key}");
        }
        let state: MergeState =
            serde_json::from_value(json!({ "status": "blocked", "behind_by": 2 })).unwrap();
        assert_eq!(state.status, MergeStatus::Blocked);
        assert_eq!(state.behind_by, Some(2));
    }

    #[test]
    fn a_step_asks_with_a_prompt_and_hears_the_answer_with_its_note_and_actor() {
        let ask: FromStep = serde_json::from_str(r#"{"type":"ask","prompt":"Ship it?"}"#).unwrap();
        assert_eq!(
            ask,
            FromStep::Ask {
                prompt: "Ship it?".into()
            }
        );

        let answer = ToStep::Answer {
            answer: Answer::Approve,
            note: Some("rename the flag".into()),
            actor: Actor::Developer { via: "gui".into() },
        };
        assert_eq!(
            serde_json::to_value(&answer).unwrap(),
            json!({
                "type": "answer",
                "answer": "approve",
                "note": "rename the flag",
                "actor": { "kind": "developer", "via": "gui" },
            })
        );
        let bare = ToStep::Answer {
            answer: Answer::Reject,
            note: None,
            actor: Actor::Developer { via: "gui".into() },
        };
        assert!(
            serde_json::to_value(bare).unwrap().get("note").is_none(),
            "no note, no key"
        );
    }

    #[test]
    fn an_effect_outside_the_closed_list_doesnt_read() {
        let line = r#"{"type":"effect","id":"x","effect":{"kind":"deploy"}}"#;

        assert!(serde_json::from_str::<FromStep>(line).is_err());
        assert!(
            serde_json::from_value::<Manifest>(json!({
                "id": "x", "version": "1", "dialect": 1, "workspace": "none",
                "effects": ["deploy"],
            }))
            .is_err()
        );
    }
}
