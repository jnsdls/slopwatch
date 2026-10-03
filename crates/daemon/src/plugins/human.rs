//! The built-in `human` Plugin: a Human Step. It asks the developer the
//! Step's `prompt`, which the daemon puts in the Inbox, and reports what
//! they answer: pass on approve, fail on reject, with their note for later
//! Steps to read.
//!
//! Only the developer can answer, so it has no timeout and no stall
//! watchdog. The answer covers the Run's head SHA only (ADR 0001): a push
//! ends the Run, and the next Run asks again.

use std::io::{BufRead, Write};

use serde_json::json;
use slopwatch_core::{Verdict, Workspace};
use slopwatch_protocol::step::{FromStep, Manifest, Outcome, Outputs, STEP_DIALECT, ToStep};
use slopwatch_protocol::{Actor, Answer};

/// The Plugin's name. Built-in names are reserved (ADR 0012), so a Step that
/// `uses: human` always runs this one, the only Plugin that may ask.
pub const ID: &str = "human";

pub fn manifest() -> Manifest {
    Manifest {
        id: ID.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        dialect: STEP_DIALECT,
        features: vec![],
        config_schema: json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What the developer is asked to approve or reject",
                },
            },
            "additionalProperties": false,
        }),
        workspace: Workspace::None,
        effects: vec![],
        secrets: vec![],
        // Step contract: a Human Step has neither.
        timeout: None,
        stall_after: None,
        concurrency: None,
    }
}

/// What the Step asks: its `prompt`, or a stock question naming it.
pub fn prompt(step: &str, config: &serde_json::Map<String, serde_json::Value>) -> String {
    match config.get("prompt").and_then(|prompt| prompt.as_str()) {
        Some(prompt) if !prompt.trim().is_empty() => prompt.trim().to_owned(),
        _ => format!("Approve `{step}`?"),
    }
}

/// The Outcome an answer gives, with the note for later Steps.
pub fn outcome(answer: Answer, note: Option<String>) -> Outcome {
    let verdict = match answer {
        Answer::Approve => Verdict::Pass,
        Answer::Reject => Verdict::Fail,
    };
    Outcome {
        verdict,
        outputs: Outputs {
            note,
            ..Outputs::default()
        },
    }
}

/// Runs one session: asks once the daemon says start, then reports the
/// answer. Returns when it has reported, when cancelled, or when the
/// daemon hangs up.
pub fn run(input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        match serde_json::from_str::<ToStep>(&line) {
            Ok(ToStep::Start(start)) => {
                let prompt = prompt(&start.step, &start.config);
                eprintln!("human: asking \"{prompt}\"");
                send(&mut output, &FromStep::Ask { prompt })?;
                send(
                    &mut output,
                    &FromStep::Progress {
                        message: Some("waiting for you".into()),
                    },
                )?;
            }
            Ok(ToStep::Answer {
                answer,
                note,
                actor,
            }) => {
                let Actor::Developer { via } = &actor;
                eprintln!("human: the developer answered {answer} through {via}");
                send(&mut output, &FromStep::Outcome(outcome(answer, note)))?;
                return Ok(());
            }
            Ok(ToStep::Cancel) => return Ok(()),
            Ok(ToStep::PrUpdated { .. } | ToStep::EffectResult { .. }) => {}
            Err(error) => {
                eprintln!("human: can't read a message from the daemon: {error}");
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

    #[test]
    fn it_asks_the_prompt_or_names_the_step() {
        let config = json!({ "prompt": "  Does the copy read right?  " });
        assert_eq!(
            prompt("copy", config.as_object().unwrap()),
            "Does the copy read right?"
        );
        assert_eq!(
            prompt("sign-off", &Default::default()),
            "Approve `sign-off`?"
        );
        let blank = json!({ "prompt": " " });
        assert_eq!(prompt("x", blank.as_object().unwrap()), "Approve `x`?");
    }

    #[test]
    fn approve_passes_reject_fails_and_the_note_rides_along() {
        let approved = outcome(Answer::Approve, Some("rename the flag".into()));
        assert_eq!(approved.verdict, Verdict::Pass);
        assert_eq!(approved.outputs.note.as_deref(), Some("rename the flag"));

        let rejected = outcome(Answer::Reject, None);
        assert_eq!(rejected.verdict, Verdict::Fail);
        assert_eq!(rejected.outputs.note, None);
    }

    #[test]
    fn a_session_asks_then_reports_the_answer() {
        let start = json!({
            "type": "start", "run": 3, "step": "sign-off",
            "config": { "prompt": "Ship it?" },
            "snapshot": {
                "repo": "o/r", "number": 1, "title": "t", "body": "", "url": "",
                "author": "me", "head_sha": "abc", "base": "main", "draft": false,
                "labels": [], "checks": { "state": "none", "runs": [] },
            },
            "upstream": {},
        });
        let answer = ToStep::Answer {
            answer: Answer::Approve,
            note: Some("go".into()),
            actor: Actor::Developer { via: "gui".into() },
        };
        let input = format!("{start}\n{}\n", serde_json::to_string(&answer).unwrap());
        let mut output = Vec::new();

        run(input.as_bytes(), &mut output).unwrap();

        let said: Vec<FromStep> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            said[0],
            FromStep::Ask {
                prompt: "Ship it?".into()
            }
        );
        assert_eq!(
            said.last(),
            Some(&FromStep::Outcome(outcome(
                Answer::Approve,
                Some("go".into())
            )))
        );
    }

    #[test]
    fn a_cancelled_session_reports_nothing() {
        let mut output = Vec::new();
        run(&b"{\"type\":\"cancel\"}\n"[..], &mut output).unwrap();
        assert!(output.is_empty());
    }
}
