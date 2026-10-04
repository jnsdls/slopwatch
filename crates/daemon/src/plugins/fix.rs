//! The built-in `fix` Plugin: the write Step that acts on the Findings
//! behind the Gate's failing, unwaived terms, and on the log tails of
//! failed GitHub Actions jobs. It runs Claude Code or Codex headless in a
//! worktree of the PR's head and lets it edit files there. It never
//! commits: the daemon judges what it leaves and commits it (ADR 0008).
//!
//! A Step's `with:` keys:
//!
//! - `agent`: `claude` (the default) or `codex`, the CLI that fixes.
//! - `prompt`: more instructions, added after the problems.
//! - `model`, `effort`, `auth`, `repo_config` and `cli`, as for the
//!   review Plugins ([`review`](super::review)).
//!
//! The agent answers with a summary and whether it fixed the problems.
//! The Step passes when it did, and the summary becomes the commit's
//! message.

use std::io::BufReader;

use serde_json::{Map, Value, json};
use slopwatch_core::{Verdict, Workspace};
use slopwatch_protocol::step::{
    CI_LOGS, FromStep, Manifest, Outcome, Outputs, STEP_DIALECT, SecretSpec, Start,
};

use super::claude::{self, Claude};
use super::codex::{self, Codex};
use super::review::{self, Config, Job};

pub const ID: &str = "fix";

/// The agents a fix Step can run.
const AGENTS: &[&str] = &["claude", "codex"];

/// Every key a fix Step's `with:` takes.
const KEYS: &[&str] = &[
    "agent",
    "prompt",
    "model",
    "effort",
    "auth",
    "repo_config",
    "cli",
];

pub fn manifest() -> Manifest {
    Manifest {
        id: ID.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        dialect: STEP_DIALECT,
        features: vec![CI_LOGS.into()],
        config_schema: config_schema(),
        workspace: Workspace::Write,
        effects: vec![],
        // Only `auth: api_key` needs the agent's key.
        secrets: vec![
            SecretSpec::optional(claude::API_KEY),
            SecretSpec::optional(codex::API_KEY),
        ],
        // Step contract: agent Steps get 30 minutes and a 5 minute stall.
        timeout: Some("30m".into()),
        stall_after: Some("5m".into()),
        // Cost budgets: an agent Step may spend $2 a Run by default.
        budget_usd: Some(2.0),
        concurrency: None,
    }
}

/// The JSON Schema for a fix Step's `with:`.
fn config_schema() -> Value {
    let text = json!({ "type": "string", "minLength": 1 });
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "agent": { "enum": AGENTS, "default": "claude" },
            "prompt": text,
            "model": text,
            "effort": text,
            "auth": { "enum": ["subscription", "api_key"], "default": "subscription" },
            "repo_config": { "type": "boolean", "default": false },
            "cli": text,
        },
    })
}

/// Reads a fix Step's `with:`: the agent it names, and the session config
/// for it, or what's wrong with it. `default_cli` gives the agent's
/// executable, by the agent's name, when the Step names none.
pub fn parse(
    with: &Map<String, Value>,
    default_cli: impl FnOnce(&str) -> String,
) -> Result<(String, Config), String> {
    if let Some(key) = with.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return Err(format!(
            "`with:` has an unknown key `{key}`. It takes {}.",
            KEYS.iter()
                .map(|key| format!("`{key}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let agent = match with.get("agent") {
        None | Some(Value::Null) => "claude".to_owned(),
        Some(Value::String(agent)) if AGENTS.contains(&agent.as_str()) => agent.clone(),
        Some(other) => {
            return Err(format!("`agent` is `claude` or `codex`, not {other}"));
        }
    };
    // The review keys read the same way. A fix needs no prompt, and has
    // no Findings of its own to fail on.
    let mut review = with.clone();
    review.remove("agent");
    let extra = review.remove("prompt");
    review.insert("prompt".into(), Value::String("fix".into()));
    let mut config = Config::parse(&review, &default_cli(&agent))?;
    config.job = Job::Fix;
    config.prompt = match extra {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) if !text.trim().is_empty() => text,
        Some(_) => return Err("`prompt` must be a non-empty string".to_owned()),
    };
    Ok((agent, config))
}

/// The JSON Schema the fixer's answer holds to.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "fixed"],
        "properties": {
            "summary": {
                "type": "string",
                "description": "What you changed, as a commit message: a headline under 72 \
                    characters, then a blank line and more if needed.",
            },
            "fixed": {
                "type": "boolean",
                "description": "Whether you changed files to fix the problems.",
            },
        },
    })
}

/// What the fixer is told: the PR, the problems behind the Gate's failing
/// terms, the failed CI jobs' log tails, and the rules for its changes.
pub fn prompt(config: &Config, start: &Start) -> String {
    let snapshot = &start.snapshot;
    let mut text = format!(
        "You are fixing pull request #{} in {}: \"{}\".\n\n",
        snapshot.number, snapshot.repo, snapshot.title
    );
    if !snapshot.body.trim().is_empty() {
        text.push_str(&format!(
            "Its description:\n<description>\n{}\n</description>\n\n",
            snapshot.body.trim()
        ));
    }
    text.push_str(
        "The working directory is a checkout of the PR's head commit. Fix the problems below by \
         editing files there, and follow the repo's AGENTS.md or CLAUDE.md if it has one.\n\n\
         Rules for your changes:\n\
         - Change only what fixing the problems needs.\n\
         - Don't commit, push or make branches. slopwatch commits what you leave in the \
         working directory.\n\
         - Don't change anything under .github/ or .slopwatch/, other CI configs, or lockfiles. \
         A change to any of them refuses the whole commit.\n\
         - Don't change file modes, and don't add symlinks or submodules. They can't be \
         committed.\n\
         - Don't leave scratch files behind.\n\n",
    );
    let failing: Vec<&String> = if start.gate_failing.is_empty() {
        start
            .upstream
            .iter()
            .filter(|(_, outcome)| outcome.verdict != Verdict::Pass)
            .map(|(id, _)| id)
            .collect()
    } else {
        start.gate_failing.iter().collect()
    };
    text.push_str("The problems:\n");
    let mut any = false;
    for id in failing {
        let Some(outcome) = start.upstream.get(id) else {
            continue;
        };
        any = true;
        text.push_str(&format!("\nStep `{id}` ended {}.", outcome.verdict));
        if let Some(note) = outcome
            .outputs
            .note
            .as_deref()
            .filter(|n| !n.trim().is_empty())
        {
            text.push_str(&format!(" {}", note.trim()));
        }
        text.push('\n');
        for finding in &outcome.outputs.findings {
            let place = match (&finding.file, finding.line) {
                (Some(file), Some(line)) => format!(" {file}:{line}"),
                (Some(file), None) => format!(" {file}"),
                _ => String::new(),
            };
            text.push_str(&format!(
                "- {}{place}: {}\n",
                finding.severity, finding.message
            ));
        }
    }
    for log in &start.ci_logs {
        any = true;
        text.push_str(&format!(
            "\nThe CI check `{}` failed. The end of its log:\n```\n{}\n```\n",
            log.check, log.tail
        ));
    }
    if !any {
        text.push_str("\nNo Step reported details. Look at the PR for what fails.\n");
    }
    if !config.prompt.trim().is_empty() {
        text.push_str(&format!("\nMore instructions:\n{}\n", config.prompt.trim()));
    }
    text.push_str(
        "\nAnswer with the JSON object the output schema describes: `summary` says what you \
         changed, as a commit message, and `fixed` is true if you changed files to fix the \
         problems, false if you couldn't fix them.",
    );
    text
}

/// Turns the fixer's answer into the Step's Outcome.
pub fn outcome(answer: &Value) -> Result<Outcome, String> {
    let fixed = answer
        .get("fixed")
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("the answer doesn't say whether it fixed anything: {answer}"))?;
    let summary = answer
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|summary| !summary.is_empty());
    Ok(Outcome {
        verdict: if fixed { Verdict::Pass } else { Verdict::Fail },
        outputs: Outputs {
            note: summary.map(str::to_owned),
            ..Outputs::default()
        },
    })
}

/// Serves `slopwatchd plugin fix run`.
pub fn run() -> std::io::Result<()> {
    let mut input = BufReader::new(std::io::stdin());
    let mut output = std::io::stdout().lock();
    let Some(start) = review::read_start(&mut input, ID)? else {
        return Ok(());
    };
    let (agent, config) = match parse(&start.config, review::default_cli) {
        Ok(parsed) => parsed,
        Err(reason) => return review::send(&mut output, &FromStep::Error { reason }),
    };
    match agent.as_str() {
        "codex" => review::session(Codex::default(), &config, &start, input, &mut output),
        _ => review::session(Claude::default(), &config, &start, input, &mut output),
    }
}

/// Whether `config` runs on an API key, for tests.
#[cfg(test)]
fn on_api_key(config: &Config) -> bool {
    config.auth == review::Auth::ApiKey
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::RunId;
    use slopwatch_protocol::step::{CiLog, Finding, Severity};
    use std::collections::BTreeMap;

    fn with(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_config_picks_its_agent_and_reads_the_review_keys() {
        let parse = |value: Value| parse(&with(value), str::to_owned);
        let (agent, config) = parse(json!({})).unwrap();
        assert_eq!(agent, "claude");
        assert_eq!(config.job, Job::Fix);
        assert_eq!(config.cli, "claude");
        assert!(config.prompt.is_empty());

        let (agent, config) = parse(json!({
            "agent": "codex", "auth": "api_key", "model": "gpt-5.4", "prompt": "Keep it small.",
        }))
        .unwrap();
        assert_eq!(agent, "codex");
        assert_eq!(config.cli, "codex");
        assert!(on_api_key(&config));
        assert_eq!(config.prompt, "Keep it small.");

        assert!(
            parse(json!({ "agent": "gemini" }))
                .unwrap_err()
                .contains("gemini")
        );
        assert!(
            parse(json!({ "fail_on": "error" }))
                .unwrap_err()
                .contains("fail_on")
        );
    }

    #[test]
    fn the_agent_runs_the_cli_its_settings_name_unless_the_step_names_one() {
        let set = |agent: &str| format!("/opt/{agent}-from-settings");
        let (_, config) = parse(&with(json!({ "agent": "codex" })), set).unwrap();
        assert_eq!(config.cli, "/opt/codex-from-settings");
        let (_, config) = parse(&with(json!({ "cli": "/usr/local/bin/claude" })), set).unwrap();
        assert_eq!(config.cli, "/usr/local/bin/claude");
    }

    #[test]
    fn the_prompt_lists_the_failing_steps_findings_and_the_ci_log_tails() {
        let (_, config) = parse(&with(json!({ "prompt": "Add a test." })), str::to_owned).unwrap();
        let failing = Outcome {
            verdict: Verdict::Fail,
            outputs: Outputs {
                findings: vec![Finding {
                    severity: Severity::Error,
                    message: "Divides by zero on an empty list.".into(),
                    file: Some("stats.py".into()),
                    line: Some(6),
                }],
                note: Some("1 finding.".into()),
                ..Outputs::default()
            },
        };
        let waived = Outcome::new(Verdict::Fail);
        let start = Start {
            run: RunId(1),
            step: "fix".into(),
            config: Map::new(),
            snapshot: review::tests::snapshot(),
            upstream: BTreeMap::from([
                ("review".to_owned(), failing),
                ("lint".to_owned(), waived),
                ("ci".to_owned(), Outcome::new(Verdict::Fail)),
            ]),
            gate_failing: vec!["review".into(), "ci".into()],
            ci_logs: vec![CiLog {
                check: "test".into(),
                job: 7,
                tail: "assert 1 == 2".into(),
            }],
            budget_usd: None,
        };

        let text = prompt(&config, &start);

        assert!(text.contains("pull request #7 in o/r"));
        assert!(text.contains("Step `review` ended fail. 1 finding."));
        assert!(text.contains("- error stats.py:6: Divides by zero on an empty list."));
        assert!(text.contains("Step `ci` ended fail."));
        assert!(
            !text.contains("`lint`"),
            "a waived Step isn't a problem to fix"
        );
        assert!(text.contains("The CI check `test` failed"));
        assert!(text.contains("assert 1 == 2"));
        assert!(text.contains("More instructions:\nAdd a test."));
        assert!(text.contains(".github/"));
    }

    #[test]
    fn the_answer_passes_when_it_fixed_something() {
        let fixed = outcome(&json!({ "summary": "Guard the empty list.", "fixed": true })).unwrap();
        assert_eq!(fixed.verdict, Verdict::Pass);
        assert_eq!(fixed.outputs.note.as_deref(), Some("Guard the empty list."));
        let stuck = outcome(&json!({ "summary": "", "fixed": false })).unwrap();
        assert_eq!(stuck.verdict, Verdict::Fail);
        assert_eq!(stuck.outputs.note, None);
        assert!(outcome(&json!({ "summary": "x" })).is_err());
    }
}
