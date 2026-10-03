//! The built-in `merge` Plugin. It lands the PR once the Gate passes,
//! directly or through the base's merge queue, and only at the head SHA the
//! Gate judged: it asks for the `merge` Effect, and the daemon sends the
//! Run's head SHA with the call (ADR 0003, ADR 0011).
//!
//! Before merging it checks how far behind its base the PR is. A behind PR
//! on a base without a merge queue gets the `rebase` Effect instead, and
//! the push that follows ends the Run, so the next Run judges and lands the
//! rebased SHA (ADR 0004). A branch that requires signed commits gets its
//! base merged in instead, since GitHub can't sign rebased commits.
//!
//! It reads where the PR stands from the snapshot's merge state, which the
//! daemon fills in while the Step runs ([`MERGE_STATE`]). A draft never
//! merges, and a PR GitHub blocks, such as for a missing review, waits until
//! the Step's timeout. A conflict, a refused merge and a merge queue that
//! drops the PR fail the Step with a Finding, which leaves the Run not
//! shippable.

use std::io::{BufRead, Write};

use serde::Deserialize;
use serde_json::{Value, json};
use slopwatch_core::{Verdict, Workspace};
use slopwatch_protocol::step::{
    Effect, EffectKind, EffectResult, Finding, FromStep, MERGE_STATE, Manifest, MergeMethod,
    MergeState, MergeStatus, Outcome, Outputs, PrSnapshot, STEP_DIALECT, Severity, ToStep,
    UpdateMethod,
};

/// How long Merge waits, most of it on a PR GitHub blocks, before the
/// daemon ends it `error(timeout)`. A Pipeline can set its own `timeout`.
pub const TIMEOUT: &str = "1h";

pub fn manifest() -> Manifest {
    Manifest {
        id: "merge".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        dialect: STEP_DIALECT,
        features: vec![MERGE_STATE.into()],
        config_schema: json!({
            "type": "object",
            "properties": {
                "method": { "enum": ["merge", "squash", "rebase"] },
                "update": { "enum": ["rebase", "merge"] },
            },
            "additionalProperties": false,
        }),
        workspace: Workspace::None,
        effects: vec![EffectKind::Rebase, EffectKind::Merge],
        secrets: vec![],
        timeout: Some(TIMEOUT.into()),
        stall_after: None,
        concurrency: None,
    }
}

/// The Step's `with:`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// How a direct merge lands the PR. Squash unless set. A merge queue
    /// uses its own method.
    #[serde(default)]
    pub method: Option<MergeMethod>,
    /// How a behind PR catches up with its base: rebase unless set, or a
    /// merge of the base where the branch requires signed commits.
    #[serde(default)]
    pub update: Option<UpdateMethod>,
}

/// The request ids Merge uses. A respawned session asks under the same
/// ones, so the daemon repeats nothing.
pub const MERGE_REQUEST: &str = "merge";
pub const REBASE_REQUEST: &str = "rebase";

/// What the PR calls for next.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Nothing to do yet, and why, for the PR pane.
    Wait(String),
    /// Ask the daemon for this Effect under this request id.
    Ask(&'static str, Effect),
    Report(Outcome),
}

/// One session's judgement, which remembers what it asked for.
#[derive(Debug, Default)]
pub struct Merge {
    config: Config,
    asked: Option<Asked>,
    /// The merge queue took the PR, and no snapshot has come since. The
    /// merge state in hand was read before, so it can't tell an ejection
    /// from a PR the queue hadn't taken yet.
    stale_after_enqueue: bool,
}

/// The Effect a session asked for, with the daemon's answer once it came.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asked {
    Rebase(Option<EffectResult>),
    Merge(Option<EffectResult>),
}

impl Asked {
    fn request_id(&self) -> &'static str {
        match self {
            Asked::Rebase(_) => REBASE_REQUEST,
            Asked::Merge(_) => MERGE_REQUEST,
        }
    }
}

impl Merge {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Notes what became of an Effect this session asked for.
    pub fn effect_result(&mut self, id: &str, result: &EffectResult) {
        let Some(asked) = &mut self.asked else {
            return;
        };
        if asked.request_id() != id {
            return;
        }
        if *result == EffectResult::Enqueued {
            self.stale_after_enqueue = true;
        }
        match asked {
            Asked::Rebase(answer) | Asked::Merge(answer) => *answer = Some(result.clone()),
        }
    }

    /// Notes that a new snapshot came. After the merge queue took the PR,
    /// the daemon sends the merge state it reads next, even unchanged.
    pub fn snapshot_updated(&mut self) {
        self.stale_after_enqueue = false;
    }

    pub fn judge(&mut self, snapshot: &PrSnapshot) -> Next {
        let state = snapshot.merge.as_ref();
        if state.is_some_and(|state| state.merged) {
            return Next::Report(pass("Merged"));
        }
        match &self.asked {
            None => self.decide(snapshot),
            Some(Asked::Merge(None)) => Next::Wait("Merging".into()),
            Some(Asked::Merge(Some(EffectResult::Done))) => Next::Report(pass("Merged")),
            Some(Asked::Merge(Some(EffectResult::Enqueued))) => self.queued(state),
            Some(Asked::Merge(Some(result))) => Next::Report(fail(format!(
                "GitHub didn't merge the PR: {}",
                reason(result)
            ))),
            Some(Asked::Rebase(None)) => Next::Wait("Updating the branch with its base".into()),
            // The push ends the Run, and the next one judges what it made.
            Some(Asked::Rebase(Some(EffectResult::Done))) => {
                Next::Wait("Waiting for GitHub to push the updated branch".into())
            }
            Some(Asked::Rebase(Some(result))) => Next::Report(fail(format!(
                "Couldn't update the branch with its base `{}`: {}",
                snapshot.base,
                reason(result)
            ))),
        }
    }

    /// What a PR nothing was asked for yet calls for.
    fn decide(&mut self, snapshot: &PrSnapshot) -> Next {
        // A Stack lands from the bottom, and the daemon moves this PR onto
        // its parent's base once the parent merges (ADR 0011).
        if let Some(parent) = snapshot.stacked_on {
            return Next::Report(Outcome {
                verdict: Verdict::Inconclusive,
                outputs: Outputs {
                    findings: vec![finding(Severity::Info, format!("Stacked on #{parent}"))],
                    ..Outputs::default()
                },
            });
        }
        if snapshot.draft {
            return Next::Report(Outcome {
                verdict: Verdict::Inconclusive,
                outputs: Outputs {
                    findings: vec![finding(
                        Severity::Info,
                        "The PR is a draft, so it doesn't merge".into(),
                    )],
                    ..Outputs::default()
                },
            });
        }
        let Some(state) = &snapshot.merge else {
            return Next::Wait("Asking GitHub whether the PR can merge".into());
        };
        if state.conflicts || state.status == MergeStatus::Dirty {
            return Next::Report(fail(format!(
                "The PR conflicts with its base `{}`",
                snapshot.base
            )));
        }
        let behind = state.behind_by.is_some_and(|behind| behind > 0);
        if behind && !state.merge_queue {
            let method = self.config.update.unwrap_or(if state.requires_signatures {
                UpdateMethod::Merge
            } else {
                UpdateMethod::Rebase
            });
            self.asked = Some(Asked::Rebase(None));
            return Next::Ask(REBASE_REQUEST, Effect::Rebase { method });
        }
        match state.status {
            MergeStatus::Clean | MergeStatus::Unstable | MergeStatus::HasHooks => {}
            // A merge queue takes a PR its base has moved past.
            MergeStatus::Behind if state.merge_queue => {}
            MergeStatus::Blocked => {
                return Next::Wait(
                    "GitHub blocks the merge, such as for a missing review or required check"
                        .into(),
                );
            }
            MergeStatus::Behind
            | MergeStatus::Draft
            | MergeStatus::Dirty
            | MergeStatus::Unknown => {
                return Next::Wait(
                    "Waiting for GitHub to work out whether the PR can merge".into(),
                );
            }
        }
        self.merge(state)
    }

    fn merge(&mut self, state: &MergeState) -> Next {
        let method = if state.merge_queue {
            None
        } else {
            let method = self.config.method.unwrap_or(MergeMethod::Squash);
            if !state.methods.contains(&method) {
                return Next::Report(fail(format!(
                    "The repo doesn't allow {method} merges; set `method:` to one it does"
                )));
            }
            Some(method)
        };
        self.asked = Some(Asked::Merge(None));
        Next::Ask(MERGE_REQUEST, Effect::Merge { method })
    }

    /// A PR the merge queue took: it waits for GitHub to merge it, and a
    /// PR the queue drops again fails.
    fn queued(&self, state: Option<&MergeState>) -> Next {
        match state {
            _ if self.stale_after_enqueue => Next::Wait("Waiting for the merge queue".into()),
            Some(state) if state.in_merge_queue => Next::Wait("In the merge queue".into()),
            Some(_) => Next::Report(fail(
                "The merge queue removed the PR without merging it".into(),
            )),
            None => Next::Wait("Waiting for the merge queue".into()),
        }
    }
}

fn reason(result: &EffectResult) -> &str {
    match result {
        EffectResult::Dropped { reason }
        | EffectResult::Refused { reason }
        | EffectResult::Failed { reason } => reason,
        EffectResult::Done | EffectResult::Enqueued => "",
    }
}

fn finding(severity: Severity, message: String) -> Finding {
    Finding {
        severity,
        message,
        file: None,
        line: None,
    }
}

fn pass(note: &str) -> Outcome {
    Outcome {
        verdict: Verdict::Pass,
        outputs: Outputs {
            note: Some(note.to_owned()),
            ..Outputs::default()
        },
    }
}

fn fail(message: String) -> Outcome {
    Outcome {
        verdict: Verdict::Fail,
        outputs: Outputs {
            findings: vec![finding(Severity::Error, message)],
            ..Outputs::default()
        },
    }
}

/// Runs one session: reads the daemon's messages from `input`, asks for a
/// rebase or a merge, and writes the Outcome to `output` once there is one.
/// Returns when it has reported, when cancelled, or when the daemon hangs
/// up.
pub fn run(input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    let mut merge = None;
    let mut snapshot = None;
    let mut waiting = None;
    for line in input.lines() {
        let line = line?;
        match serde_json::from_str::<ToStep>(&line) {
            Ok(ToStep::Start(start)) => {
                match serde_json::from_value::<Config>(Value::Object(start.config)) {
                    Ok(config) => merge = Some(Merge::new(config)),
                    Err(error) => {
                        let outcome = fail(format!("The Step's `with:` doesn't read: {error}"));
                        return send(&mut output, &FromStep::Outcome(outcome));
                    }
                }
                snapshot = Some(start.snapshot);
            }
            Ok(ToStep::PrUpdated { snapshot: update }) => {
                if let Some(merge) = &mut merge {
                    merge.snapshot_updated();
                }
                snapshot = Some(update);
            }
            Ok(ToStep::EffectResult { id, result }) => {
                if let Some(merge) = &mut merge {
                    merge.effect_result(&id, &result);
                }
            }
            Ok(ToStep::Cancel) => return Ok(()),
            // `merge` never asks, so no answer comes.
            Ok(ToStep::Answer { .. }) => continue,
            Err(error) => {
                eprintln!("merge: can't read a message from the daemon: {error}");
                return Ok(());
            }
        }
        let (Some(merge), Some(snapshot)) = (&mut merge, &snapshot) else {
            continue;
        };
        match merge.judge(snapshot) {
            Next::Wait(status) => {
                if waiting.as_ref() != Some(&status) {
                    eprintln!("merge: {status}");
                    send(
                        &mut output,
                        &FromStep::Progress {
                            message: Some(status.clone()),
                        },
                    )?;
                    waiting = Some(status);
                }
            }
            Next::Ask(id, effect) => {
                eprintln!("merge: asking for {}", effect.kind());
                waiting = None;
                send(
                    &mut output,
                    &FromStep::Effect {
                        id: id.to_owned(),
                        effect,
                    },
                )?;
            }
            Next::Report(outcome) => return send(&mut output, &FromStep::Outcome(outcome)),
        }
    }
    Ok(())
}

fn send(output: &mut impl Write, message: &FromStep) -> std::io::Result<()> {
    let line = serde_json::to_string(message)?;
    writeln!(output, "{line}")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::RepoName;
    use slopwatch_protocol::step::Checks;

    fn snapshot(merge: Option<MergeState>) -> PrSnapshot {
        PrSnapshot {
            repo: RepoName::new("o", "r"),
            number: 1,
            title: String::new(),
            body: String::new(),
            url: String::new(),
            author: "me".into(),
            head_sha: "abc".into(),
            base: "main".into(),
            draft: false,
            labels: vec![],
            checks: Checks::default(),
            merge,
            diff: None,
            linked_issues: vec![],
            stacked_on: None,
        }
    }

    fn clean() -> MergeState {
        MergeState {
            status: MergeStatus::Clean,
            behind_by: Some(0),
            methods: vec![MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase],
            ..MergeState::default()
        }
    }

    fn finding_of(next: Next) -> (Verdict, String) {
        match next {
            Next::Report(outcome) => (outcome.verdict, outcome.outputs.findings[0].message.clone()),
            other => panic!("expected an Outcome, got {other:?}"),
        }
    }

    #[test]
    fn waits_for_the_merge_state_then_squashes_a_clean_pr() {
        let mut merge = Merge::default();

        assert!(matches!(merge.judge(&snapshot(None)), Next::Wait(_)));
        assert_eq!(
            merge.judge(&snapshot(Some(clean()))),
            Next::Ask(
                MERGE_REQUEST,
                Effect::Merge {
                    method: Some(MergeMethod::Squash)
                }
            )
        );
        assert_eq!(
            merge.judge(&snapshot(Some(clean()))),
            Next::Wait("Merging".into()),
            "asks once"
        );
        merge.effect_result(MERGE_REQUEST, &EffectResult::Done);
        let Next::Report(outcome) = merge.judge(&snapshot(Some(clean()))) else {
            panic!("expected an Outcome");
        };
        assert_eq!(outcome.verdict, Verdict::Pass);
    }

    #[test]
    fn a_draft_never_merges() {
        let mut draft = snapshot(Some(clean()));
        draft.draft = true;

        let (verdict, message) = finding_of(Merge::default().judge(&draft));

        assert_eq!(verdict, Verdict::Inconclusive);
        assert!(message.contains("draft"), "{message}");
    }

    #[test]
    fn a_stacked_pr_waits_for_its_parent_without_merging() {
        let mut stacked = snapshot(Some(clean()));
        stacked.stacked_on = Some(4);

        let mut merge = Merge::default();
        let (verdict, message) = finding_of(merge.judge(&stacked));

        assert_eq!(verdict, Verdict::Inconclusive);
        assert_eq!(message, "Stacked on #4");
        stacked.merge = None;
        assert!(
            matches!(Merge::default().judge(&stacked), Next::Report(_)),
            "it doesn't wait for a merge state it won't use"
        );
    }

    #[test]
    fn a_behind_pr_without_a_merge_queue_is_rebased_or_merged_where_signatures_are_required() {
        let behind = MergeState {
            behind_by: Some(2),
            ..clean()
        };
        assert_eq!(
            Merge::default().judge(&snapshot(Some(behind.clone()))),
            Next::Ask(
                REBASE_REQUEST,
                Effect::Rebase {
                    method: UpdateMethod::Rebase
                }
            )
        );

        let signed = MergeState {
            requires_signatures: true,
            ..behind.clone()
        };
        let mut merge = Merge::default();
        assert_eq!(
            merge.judge(&snapshot(Some(signed))),
            Next::Ask(
                REBASE_REQUEST,
                Effect::Rebase {
                    method: UpdateMethod::Merge
                }
            )
        );
        merge.effect_result(REBASE_REQUEST, &EffectResult::Done);
        assert!(
            matches!(merge.judge(&snapshot(Some(behind.clone()))), Next::Wait(_)),
            "the push ends the Run"
        );

        let forced = Merge::new(Config {
            update: Some(UpdateMethod::Merge),
            ..Config::default()
        })
        .judge(&snapshot(Some(behind.clone())));
        assert_eq!(
            forced,
            Next::Ask(
                REBASE_REQUEST,
                Effect::Rebase {
                    method: UpdateMethod::Merge
                }
            )
        );

        let queued = MergeState {
            merge_queue: true,
            ..behind
        };
        assert_eq!(
            Merge::default().judge(&snapshot(Some(queued))),
            Next::Ask(MERGE_REQUEST, Effect::Merge { method: None }),
            "a merge queue takes a behind PR as it is"
        );
    }

    #[test]
    fn a_conflict_or_a_failed_rebase_fails_with_a_finding() {
        let conflicting = MergeState {
            conflicts: true,
            behind_by: Some(1),
            ..clean()
        };
        let (verdict, message) = finding_of(Merge::default().judge(&snapshot(Some(conflicting))));
        assert_eq!(verdict, Verdict::Fail);
        assert_eq!(message, "The PR conflicts with its base `main`");

        let mut merge = Merge::default();
        let behind = MergeState {
            behind_by: Some(1),
            ..clean()
        };
        merge.judge(&snapshot(Some(behind.clone())));
        merge.effect_result(
            REBASE_REQUEST,
            &EffectResult::Failed {
                reason: "GitHub refused: merge conflict between base and head".into(),
            },
        );
        let (verdict, message) = finding_of(merge.judge(&snapshot(Some(behind))));
        assert_eq!(verdict, Verdict::Fail);
        assert!(message.contains("merge conflict"), "{message}");
    }

    #[test]
    fn a_blocked_pr_waits() {
        let blocked = MergeState {
            status: MergeStatus::Blocked,
            ..clean()
        };

        let Next::Wait(why) = Merge::default().judge(&snapshot(Some(blocked))) else {
            panic!("expected to wait");
        };
        assert!(why.contains("blocks"), "{why}");
    }

    #[test]
    fn an_enqueued_pr_passes_once_merged_and_fails_once_ejected() {
        let queue = MergeState {
            merge_queue: true,
            ..clean()
        };
        let mut merge = Merge::default();
        merge.judge(&snapshot(Some(queue.clone())));
        merge.effect_result(MERGE_REQUEST, &EffectResult::Enqueued);

        assert!(
            matches!(merge.judge(&snapshot(Some(queue.clone()))), Next::Wait(_)),
            "a merge state read before the queue took it"
        );
        merge.snapshot_updated();
        let in_queue = MergeState {
            in_merge_queue: true,
            ..queue.clone()
        };
        assert_eq!(
            merge.judge(&snapshot(Some(in_queue))),
            Next::Wait("In the merge queue".into())
        );
        let (verdict, message) = finding_of(merge.judge(&snapshot(Some(queue.clone()))));
        assert_eq!(verdict, Verdict::Fail);
        assert!(message.contains("merge queue removed"), "{message}");

        let merged = MergeState {
            merged: true,
            ..queue
        };
        let Next::Report(outcome) = merge.judge(&snapshot(Some(merged))) else {
            panic!("expected an Outcome");
        };
        assert_eq!(outcome.verdict, Verdict::Pass);
    }

    #[test]
    fn a_merge_method_the_repo_disallows_fails() {
        let squash_only = MergeState {
            methods: vec![MergeMethod::Squash],
            ..clean()
        };

        let (verdict, message) = finding_of(
            Merge::new(Config {
                method: Some(MergeMethod::Rebase),
                ..Config::default()
            })
            .judge(&snapshot(Some(squash_only))),
        );

        assert_eq!(verdict, Verdict::Fail);
        assert!(message.contains("rebase merges"), "{message}");
    }

    #[test]
    fn a_refused_merge_fails_with_the_reason() {
        let mut merge = Merge::default();
        merge.judge(&snapshot(Some(clean())));
        merge.effect_result(
            MERGE_REQUEST,
            &EffectResult::Failed {
                reason: "GitHub refused: Pull Request has merge conflicts".into(),
            },
        );

        let (verdict, message) = finding_of(merge.judge(&snapshot(Some(clean()))));

        assert_eq!(verdict, Verdict::Fail);
        assert!(message.contains("merge conflicts"), "{message}");
    }

    #[test]
    fn a_session_asks_to_merge_and_reports_what_github_did() {
        let state = serde_json::to_string(&clean()).unwrap();
        let start = format!(
            r#"{{"type":"start","run":1,"step":"merge","config":{{"method":"merge"}},"upstream":{{}},"snapshot":{{"repo":"o/r","number":1,"title":"","body":"","url":"","author":"me","head_sha":"abc","base":"main","draft":false,"labels":[],"checks":{{"state":"success","runs":[]}},"merge":{state}}}}}"#
        );
        let done = r#"{"type":"effect_result","id":"merge","result":{"status":"done"}}"#;
        let input = format!("{start}\n{done}\n");
        let mut output = Vec::new();

        run(input.as_bytes(), &mut output).unwrap();

        let sent: Vec<FromStep> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            sent[0],
            FromStep::Effect {
                id: MERGE_REQUEST.into(),
                effect: Effect::Merge {
                    method: Some(MergeMethod::Merge)
                },
            }
        );
        let FromStep::Outcome(outcome) = &sent[1] else {
            panic!("expected an Outcome, got {sent:?}");
        };
        assert_eq!(outcome.verdict, Verdict::Pass);
    }

    #[test]
    fn a_session_with_an_unknown_setting_fails() {
        let start = r#"{"type":"start","run":1,"step":"merge","config":{"strategy":"yolo"},"upstream":{},"snapshot":{"repo":"o/r","number":1,"title":"","body":"","url":"","author":"me","head_sha":"abc","base":"main","draft":false,"labels":[],"checks":{"state":"none","runs":[]}}}"#;
        let mut output = Vec::new();

        run(format!("{start}\n").as_bytes(), &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(r#""verdict":"fail""#), "{text}");
    }
}
