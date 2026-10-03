//! The built-in `ci` Plugin. It waits for the checks GitHub reports on the
//! Run's head commit and reports pass once they all pass, or fail with one
//! Finding per failing check.
//!
//! It never asks GitHub itself: the daemon's poll feeds it `pr_updated`
//! snapshots (ADR 0003). A commit with no checks yet keeps it waiting,
//! because a fresh push often shows none for a while.

use std::io::{BufRead, Write};

use serde_json::json;
use slopwatch_core::{Verdict, Workspace};
use slopwatch_protocol::step::{
    CheckState, Checks, ChecksState, Finding, FromStep, Manifest, Outcome, Outputs, STEP_DIALECT,
    Severity, ToStep,
};

pub fn manifest() -> Manifest {
    Manifest {
        id: "ci".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        dialect: STEP_DIALECT,
        features: vec![],
        config_schema: json!({ "type": "object", "additionalProperties": false }),
        workspace: Workspace::None,
        effects: vec![],
        secrets: vec![],
        // Step contract: CI gets 90 minutes and no stall watchdog.
        timeout: Some("90m".into()),
        stall_after: None,
        // Waiting on GitHub costs nothing, so only the global cap applies.
        concurrency: None,
    }
}

/// The Outcome the checks call for, or `None` while they're still going.
pub fn judge(checks: &Checks) -> Option<Outcome> {
    match checks.state {
        ChecksState::None | ChecksState::Pending => None,
        ChecksState::Success => Some(Outcome {
            verdict: Verdict::Pass,
            outputs: Outputs {
                note: Some(match checks.runs.len() {
                    1 => "1 check passed".to_owned(),
                    n => format!("{n} checks passed"),
                }),
                ..Outputs::default()
            },
        }),
        ChecksState::Failure => Some(Outcome {
            verdict: Verdict::Fail,
            outputs: Outputs {
                findings: checks
                    .runs
                    .iter()
                    .filter(|check| check.state == CheckState::Failure)
                    .map(|check| Finding {
                        severity: Severity::Error,
                        message: match &check.url {
                            Some(url) => format!("Check `{}` failed: {url}", check.name),
                            None => format!("Check `{}` failed", check.name),
                        },
                        file: None,
                        line: None,
                    })
                    .collect(),
                ..Outputs::default()
            },
        }),
    }
}

/// Runs one session: reads the daemon's messages from `input` and writes
/// the Outcome to `output` once the checks settle. Returns when it has
/// reported, when cancelled, or when the daemon hangs up.
pub fn run(input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        let checks = match serde_json::from_str::<ToStep>(&line) {
            Ok(ToStep::Start(start)) => start.snapshot.checks,
            Ok(ToStep::PrUpdated { snapshot }) => snapshot.checks,
            Ok(ToStep::Cancel) => return Ok(()),
            Err(error) => {
                eprintln!("ci: can't read a message from the daemon: {error}");
                return Ok(());
            }
        };
        if let Some(outcome) = judge(&checks) {
            let message = serde_json::to_string(&FromStep::Outcome(outcome))?;
            writeln!(output, "{message}")?;
            output.flush()?;
            return Ok(());
        }
        let waiting = checks
            .runs
            .iter()
            .filter(|check| check.state == CheckState::Pending)
            .count();
        eprintln!("ci: waiting for {waiting} of {} checks", checks.runs.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::step::Check;

    fn check(name: &str, state: CheckState) -> Check {
        Check {
            name: name.into(),
            state,
            url: None,
        }
    }

    #[test]
    fn waits_while_checks_are_pending_or_absent() {
        assert_eq!(judge(&Checks::default()), None);
        assert_eq!(
            judge(&Checks {
                state: ChecksState::Pending,
                runs: vec![check("test", CheckState::Pending)],
            }),
            None
        );
    }

    #[test]
    fn passes_once_every_check_passes() {
        let outcome = judge(&Checks {
            state: ChecksState::Success,
            runs: vec![
                check("test", CheckState::Success),
                check("lint", CheckState::Skipped),
            ],
        })
        .unwrap();

        assert_eq!(outcome.verdict, Verdict::Pass);
        assert_eq!(outcome.outputs.note.as_deref(), Some("2 checks passed"));
    }

    #[test]
    fn fails_with_a_finding_per_failing_check() {
        let outcome = judge(&Checks {
            state: ChecksState::Failure,
            runs: vec![
                check("test", CheckState::Failure),
                check("lint", CheckState::Success),
            ],
        })
        .unwrap();

        assert_eq!(outcome.verdict, Verdict::Fail);
        assert_eq!(outcome.outputs.findings.len(), 1);
        assert_eq!(outcome.outputs.findings[0].message, "Check `test` failed");
    }

    #[test]
    fn a_session_reports_once_an_update_settles_the_checks() {
        let pending = r#"{"type":"pr_updated","snapshot":{"repo":"o/r","number":1,"title":"","body":"","url":"","author":"me","head_sha":"abc","base":"main","draft":false,"labels":[],"checks":{"state":"pending","runs":[]}}}"#;
        let passed = pending.replace(r#""state":"pending""#, r#""state":"success""#);
        let input = format!("{pending}\n{passed}\n");
        let mut output = Vec::new();

        run(input.as_bytes(), &mut output).unwrap();

        let line = String::from_utf8(output).unwrap();
        let FromStep::Outcome(outcome) = serde_json::from_str(line.trim()).unwrap() else {
            panic!("expected an outcome, got {line}");
        };
        assert_eq!(outcome.verdict, Verdict::Pass);
    }

    #[test]
    fn a_cancelled_session_reports_nothing() {
        let mut output = Vec::new();

        run(&b"{\"type\":\"cancel\"}\n"[..], &mut output).unwrap();

        assert!(output.is_empty());
    }
}
