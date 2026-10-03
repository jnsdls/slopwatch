//! The built-in `ci` Plugin. It waits for the checks GitHub reports on the
//! Run's head commit and reports pass once they all pass, or fail with one
//! Finding per failing check.
//!
//! It never asks GitHub itself: the daemon's poll feeds it `pr_updated`
//! snapshots (ADR 0003). A commit with no checks yet keeps it waiting,
//! because a fresh push often shows none for a while.
//!
//! A failed GitHub Actions job gets one rerun through the `rerun` Effect
//! before `ci` reports fail, so a flake never reaches a write Step
//! (ADR 0008). It asks once nothing else is pending, because GitHub won't
//! rerun a job while its workflow still runs. Other checks can't be rerun
//! with the developer's token, so one of them failing fails `ci` at once.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use serde_json::json;
use slopwatch_core::{Verdict, Workspace};
use slopwatch_protocol::step::{
    Check, CheckState, Checks, ChecksState, Effect, EffectKind, EffectResult, Finding, FromStep,
    Manifest, Outcome, Outputs, STEP_DIALECT, Severity, ToStep,
};

pub fn manifest() -> Manifest {
    Manifest {
        id: "ci".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        dialect: STEP_DIALECT,
        features: vec![],
        config_schema: json!({ "type": "object", "additionalProperties": false }),
        workspace: Workspace::None,
        effects: vec![EffectKind::Rerun],
        secrets: vec![],
        // Step contract: CI gets 90 minutes and no stall watchdog.
        timeout: Some("90m".into()),
        stall_after: None,
        // Waiting on GitHub costs nothing, so only the global cap applies.
        concurrency: None,
    }
}

/// What the checks call for next.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    Wait,
    /// Rerun these checks' jobs, as `(check, job)`.
    Rerun(Vec<(String, u64)>),
    Report(Outcome),
}

/// One session's judgement, which remembers the reruns it asked for.
#[derive(Debug, Default)]
pub struct Ci {
    /// By check name.
    reruns: BTreeMap<String, Rerun>,
}

#[derive(Debug)]
struct Rerun {
    /// The failed job it reran. The rerun runs as a new job.
    job: u64,
    /// Why the daemon didn't rerun it. `None` while asked or once done.
    failed: Option<String>,
}

impl Ci {
    /// The request id for rerunning `check`'s job `job`. A respawned
    /// session asking for the same job gets the same id, so the daemon
    /// repeats nothing.
    pub fn request_id(check: &str, job: u64) -> String {
        format!("rerun:{job}:{check}")
    }

    /// Notes what became of a rerun this session asked for.
    pub fn effect_result(&mut self, id: &str, result: &EffectResult) {
        let Some(rerun) = self
            .reruns
            .iter_mut()
            .find(|(check, rerun)| Self::request_id(check, rerun.job) == id)
            .map(|(_, rerun)| rerun)
        else {
            return;
        };
        rerun.failed = match result {
            EffectResult::Done => None,
            EffectResult::Dropped { reason }
            | EffectResult::Refused { reason }
            | EffectResult::Failed { reason } => Some(reason.clone()),
        };
    }

    pub fn judge(&mut self, checks: &Checks) -> Next {
        let mut failed = Vec::new();
        let mut rerunnable = Vec::new();
        // A check whose rerun hasn't shown up yet counts as pending.
        let mut pending = checks.runs.iter().any(|c| c.state == CheckState::Pending);
        for check in checks
            .runs
            .iter()
            .filter(|c| c.state == CheckState::Failure)
        {
            match (self.reruns.get(&check.name), check.actions_job) {
                (Some(rerun), Some(job)) if rerun.job == job && rerun.failed.is_none() => {
                    pending = true;
                }
                (None, Some(job)) => rerunnable.push((check.name.clone(), job)),
                _ => failed.push(check),
            }
        }
        if !failed.is_empty() {
            return Next::Report(self.fail(&failed));
        }
        if !rerunnable.is_empty() {
            if pending {
                return Next::Wait;
            }
            for (check, job) in &rerunnable {
                self.reruns.insert(
                    check.clone(),
                    Rerun {
                        job: *job,
                        failed: None,
                    },
                );
            }
            return Next::Rerun(rerunnable);
        }
        match checks.state {
            _ if pending => Next::Wait,
            ChecksState::None | ChecksState::Pending => Next::Wait,
            ChecksState::Success => Next::Report(self.pass(checks)),
            // GitHub says something failed that the listed checks don't
            // show, as when there are more than one page of them.
            ChecksState::Failure => Next::Report(self.fail(&[])),
        }
    }

    fn pass(&self, checks: &Checks) -> Outcome {
        let mut note = match checks.runs.len() {
            1 => "1 check passed".to_owned(),
            n => format!("{n} checks passed"),
        };
        if !self.reruns.is_empty() {
            let names: Vec<String> = self.reruns.keys().map(|name| format!("`{name}`")).collect();
            note.push_str(&format!(" after rerunning {}", names.join(", ")));
        }
        Outcome {
            verdict: Verdict::Pass,
            outputs: Outputs {
                note: Some(note),
                ..Outputs::default()
            },
        }
    }

    fn fail(&self, failed: &[&Check]) -> Outcome {
        let findings = failed
            .iter()
            .map(|check| {
                let mut message = format!("Check `{}` failed", check.name);
                match self.reruns.get(&check.name) {
                    Some(Rerun {
                        failed: Some(why), ..
                    }) => message.push_str(&format!(" and couldn't be rerun ({why})")),
                    Some(_) => message.push_str(" again on its rerun"),
                    None => {}
                }
                if let Some(url) = &check.url {
                    message.push_str(&format!(": {url}"));
                }
                Finding {
                    severity: Severity::Error,
                    message,
                    file: None,
                    line: None,
                }
            })
            .collect();
        Outcome {
            verdict: Verdict::Fail,
            outputs: Outputs {
                findings,
                ..Outputs::default()
            },
        }
    }
}

/// Runs one session: reads the daemon's messages from `input`, asks for
/// reruns and writes the Outcome to `output` once the checks settle.
/// Returns when it has reported, when cancelled, or when the daemon hangs
/// up.
pub fn run(input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    let mut ci = Ci::default();
    let mut checks = None;
    for line in input.lines() {
        let line = line?;
        match serde_json::from_str::<ToStep>(&line) {
            Ok(ToStep::Start(start)) => checks = Some(start.snapshot.checks),
            Ok(ToStep::PrUpdated { snapshot }) => checks = Some(snapshot.checks),
            Ok(ToStep::EffectResult { id, result }) => ci.effect_result(&id, &result),
            Ok(ToStep::Cancel) => return Ok(()),
            Err(error) => {
                eprintln!("ci: can't read a message from the daemon: {error}");
                return Ok(());
            }
        }
        let Some(checks) = &checks else { continue };
        match ci.judge(checks) {
            Next::Wait => {
                let waiting = checks
                    .runs
                    .iter()
                    .filter(|check| check.state == CheckState::Pending)
                    .count();
                let status = format!("Waiting for {waiting} of {} checks", checks.runs.len());
                // The Step log keeps every change, the PR pane shows the
                // latest.
                eprintln!("ci: {status}");
                send(
                    &mut output,
                    &FromStep::Progress {
                        message: Some(status),
                    },
                )?;
            }
            Next::Rerun(reruns) => {
                for (check, job) in reruns {
                    eprintln!("ci: `{check}` failed, rerunning it once");
                    let id = Ci::request_id(&check, job);
                    send(
                        &mut output,
                        &FromStep::Effect {
                            id,
                            effect: Effect::Rerun { check, job },
                        },
                    )?;
                }
            }
            Next::Report(outcome) => {
                send(&mut output, &FromStep::Outcome(outcome))?;
                return Ok(());
            }
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

    fn check(name: &str, state: CheckState) -> Check {
        Check {
            name: name.into(),
            state,
            url: None,
            actions_job: None,
        }
    }

    fn job(name: &str, state: CheckState, job: u64) -> Check {
        Check {
            actions_job: Some(job),
            ..check(name, state)
        }
    }

    fn checks(state: ChecksState, runs: Vec<Check>) -> Checks {
        Checks { state, runs }
    }

    fn verdict(next: Next) -> Verdict {
        match next {
            Next::Report(outcome) => outcome.verdict,
            other => panic!("expected an Outcome, got {other:?}"),
        }
    }

    #[test]
    fn waits_while_checks_are_pending_or_absent() {
        let mut ci = Ci::default();
        assert_eq!(ci.judge(&Checks::default()), Next::Wait);
        assert_eq!(
            ci.judge(&checks(
                ChecksState::Pending,
                vec![check("test", CheckState::Pending)]
            )),
            Next::Wait
        );
    }

    #[test]
    fn passes_once_every_check_passes() {
        let Next::Report(outcome) = Ci::default().judge(&checks(
            ChecksState::Success,
            vec![
                check("test", CheckState::Success),
                check("lint", CheckState::Skipped),
            ],
        )) else {
            panic!("expected an Outcome");
        };

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.outputs.note.as_deref(), Some("2 checks passed"));
    }

    #[test]
    fn a_failed_check_outside_actions_fails_at_once_with_a_finding_per_failure() {
        let Next::Report(outcome) = Ci::default().judge(&checks(
            ChecksState::Failure,
            vec![
                check("status", CheckState::Failure),
                job("build", CheckState::Pending, 7),
                check("lint", CheckState::Success),
            ],
        )) else {
            panic!("expected an Outcome");
        };

        assert_eq!(outcome.verdict, Verdict::Fail);
        assert_eq!(outcome.outputs.findings.len(), 1);
        assert_eq!(outcome.outputs.findings[0].message, "Check `status` failed");
    }

    #[test]
    fn a_failed_actions_job_is_rerun_once_nothing_else_is_pending() {
        let mut ci = Ci::default();

        assert_eq!(
            ci.judge(&checks(
                ChecksState::Failure,
                vec![
                    job("test", CheckState::Failure, 7),
                    job("build", CheckState::Pending, 8)
                ],
            )),
            Next::Wait,
            "GitHub won't rerun a job while its workflow runs"
        );
        let failed = checks(
            ChecksState::Failure,
            vec![
                job("test", CheckState::Failure, 7),
                job("build", CheckState::Success, 8),
            ],
        );
        assert_eq!(ci.judge(&failed), Next::Rerun(vec![("test".into(), 7)]));
        assert_eq!(
            ci.judge(&failed),
            Next::Wait,
            "the rerun hasn't shown up yet"
        );
        ci.effect_result(&Ci::request_id("test", 7), &EffectResult::Done);
        assert_eq!(ci.judge(&failed), Next::Wait);

        let rerunning = checks(
            ChecksState::Pending,
            vec![
                job("test", CheckState::Pending, 9),
                job("build", CheckState::Success, 8),
            ],
        );
        assert_eq!(ci.judge(&rerunning), Next::Wait);
        let Next::Report(outcome) = ci.judge(&checks(
            ChecksState::Success,
            vec![
                job("test", CheckState::Success, 9),
                job("build", CheckState::Success, 8),
            ],
        )) else {
            panic!("expected an Outcome");
        };
        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(
            outcome.outputs.note.as_deref(),
            Some("2 checks passed after rerunning `test`")
        );
    }

    #[test]
    fn a_rerun_that_fails_again_fails_ci() {
        let mut ci = Ci::default();
        ci.judge(&checks(
            ChecksState::Failure,
            vec![job("test", CheckState::Failure, 7)],
        ));

        let Next::Report(outcome) = ci.judge(&checks(
            ChecksState::Failure,
            vec![job("test", CheckState::Failure, 9)],
        )) else {
            panic!("expected an Outcome");
        };

        assert_eq!(outcome.verdict, Verdict::Fail);
        assert_eq!(
            outcome.outputs.findings[0].message,
            "Check `test` failed again on its rerun"
        );
    }

    #[test]
    fn a_rerun_the_daemon_wont_do_fails_ci() {
        let mut ci = Ci::default();
        let failed = checks(
            ChecksState::Failure,
            vec![job("test", CheckState::Failure, 7)],
        );
        ci.judge(&failed);

        ci.effect_result(
            &Ci::request_id("test", 7),
            &EffectResult::Refused {
                reason: "already rerun on this SHA".into(),
            },
        );

        let Next::Report(outcome) = ci.judge(&failed) else {
            panic!("expected an Outcome");
        };
        assert_eq!(
            outcome.outputs.findings[0].message,
            "Check `test` failed and couldn't be rerun (already rerun on this SHA)"
        );
    }

    #[test]
    fn a_failing_rollup_with_no_failed_check_listed_fails() {
        assert_eq!(
            verdict(Ci::default().judge(&checks(ChecksState::Failure, vec![]))),
            Verdict::Fail
        );
    }

    fn snapshot(state: &str, runs: &str) -> String {
        format!(
            r#"{{"type":"pr_updated","snapshot":{{"repo":"o/r","number":1,"title":"","body":"","url":"","author":"me","head_sha":"abc","base":"main","draft":false,"labels":[],"checks":{{"state":"{state}","runs":[{runs}]}}}}}}"#
        )
    }

    fn sent(output: Vec<u8>) -> Vec<FromStep> {
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_session_reports_once_an_update_settles_the_checks() {
        let input = format!("{}\n{}\n", snapshot("pending", ""), snapshot("success", ""));
        let mut output = Vec::new();

        run(input.as_bytes(), &mut output).unwrap();

        let sent = sent(output);
        assert_eq!(
            sent[0],
            FromStep::Progress {
                message: Some("Waiting for 0 of 0 checks".into())
            }
        );
        let [_, FromStep::Outcome(outcome)] = &sent[..] else {
            panic!("expected an outcome, got {sent:?}");
        };
        assert_eq!(outcome.verdict, Verdict::Pass);
    }

    #[test]
    fn a_session_asks_for_a_rerun_and_fails_when_the_daemon_wont() {
        let failed = snapshot(
            "failure",
            r#"{"name":"test","state":"failure","actions_job":7}"#,
        );
        let refused = r#"{"type":"effect_result","id":"rerun:7:test","result":{"status":"failed","reason":"403"}}"#;
        let input = format!("{failed}\n{refused}\n");
        let mut output = Vec::new();

        run(input.as_bytes(), &mut output).unwrap();

        let sent = sent(output);
        assert_eq!(
            sent[0],
            FromStep::Effect {
                id: "rerun:7:test".into(),
                effect: Effect::Rerun {
                    check: "test".into(),
                    job: 7,
                },
            }
        );
        let FromStep::Outcome(outcome) = &sent[1] else {
            panic!("expected an outcome, got {sent:?}");
        };
        assert_eq!(outcome.verdict, Verdict::Fail);
    }

    #[test]
    fn a_cancelled_session_reports_nothing() {
        let mut output = Vec::new();

        run(&b"{\"type\":\"cancel\"}\n"[..], &mut output).unwrap();

        assert!(output.is_empty());
    }
}
