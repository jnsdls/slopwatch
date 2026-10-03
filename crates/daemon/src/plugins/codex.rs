//! The built-in `codex` Plugin: Codex as a reviewer, through `codex exec
//! --json` in its read-only sandbox, with `--output-schema` holding the
//! answer to the review's shape. [`review`](super::review) has the `with:`
//! keys.
//!
//! Codex's counterpart to Claude's `--bare` is a set of flags:
//! `--ignore-user-config` drops `config.toml`, with its MCP servers and its
//! list of trusted projects, `--ignore-rules` drops execpolicy rules, and
//! `--disable hooks` and `--disable plugins` turn those off. A repo's own
//! `.codex/` config loads only in a trusted project, and with the user
//! config gone nothing is trusted. Instruction files (`AGENTS.md`) still
//! load. `--ephemeral` keeps the session off disk. `repo_config: true`
//! drops all of it but `--ephemeral`.
//!
//! Codex reports tokens, not dollars, so the daemon prices them from its
//! table by the Step's `model`. A Step that leaves `model` to Codex's
//! default can't be priced, and its cost reads as unknown.
//!
//! Subscription Steps run one at a time, since concurrent runs can race
//! on a token refresh in `auth.json` (Secrets for Steps, #30).

use std::path::Path;
use std::process::{Command, ExitStatus};

use serde_json::Value;
use slopwatch_core::Workspace;
use slopwatch_protocol::step::{Manifest, PR_DIFF, STEP_DIALECT, SecretSpec, Start, Usage};

use super::review::{self, Agent, Auth, Config, Event, Finished, Job};

/// The Secret an `auth: api_key` Step runs on. Codex reads it as
/// `CODEX_API_KEY`.
pub const API_KEY: &str = "OPENAI_API_KEY";

pub fn manifest() -> Manifest {
    Manifest {
        id: "codex".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        dialect: STEP_DIALECT,
        features: vec![PR_DIFF.into()],
        config_schema: review::config_schema(),
        workspace: Workspace::Read,
        effects: vec![],
        secrets: vec![SecretSpec::optional(API_KEY)],
        timeout: Some("30m".into()),
        stall_after: Some("5m".into()),
        // Cost budgets: an agent Step may spend $2 a Run by default.
        budget_usd: Some(2.0),
        concurrency: Some(1),
    }
}

#[derive(Debug, Default)]
pub struct Codex {
    model: Option<String>,
    /// The latest agent message, which is the answer once the turn ends.
    message: Option<String>,
    failure: Option<String>,
    usage: Vec<Usage>,
}

impl Agent for Codex {
    const NAME: &'static str = "codex";
    const API_KEY: &'static str = API_KEY;

    fn command(&mut self, config: &Config, _start: &Start, schema: &Path) -> Command {
        self.model = config.model.clone();
        let mut command = Command::new(&config.cli);
        command.args([
            "exec",
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            match config.job {
                Job::Review => "read-only",
                // A fixer edits the worktree, and nothing outside it.
                Job::Fix => "workspace-write",
            },
            "--color",
            "never",
        ]);
        command.arg("--output-schema").arg(schema);
        if !config.repo_config {
            command.args([
                "--ignore-user-config",
                "--ignore-rules",
                "--disable",
                "hooks",
                "--disable",
                "plugins",
            ]);
        }
        if let Some(model) = &config.model {
            command.args(["--model", model]);
        }
        if let Some(effort) = &config.effort {
            command.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
        }
        if config.auth == Auth::ApiKey
            && let Ok(key) = std::env::var(API_KEY)
        {
            command.env("CODEX_API_KEY", key);
        }
        // The prompt comes on stdin.
        command.arg("-");
        command
    }

    fn event(&mut self, line: &str) -> Vec<Event> {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return vec![Event::Log(line.to_owned())];
        };
        let text = |value: &Value, pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "thread.started" => vec![Event::Log(format!(
                "thread {}",
                text(&event, "/thread_id").unwrap_or_default()
            ))],
            "item.started"
                if text(&event, "/item/type").as_deref() == Some("command_execution") =>
            {
                let command = text(&event, "/item/command").unwrap_or_default();
                vec![Event::Progress(format!("Running {}", shorten(&command)))]
            }
            "item.completed" => match text(&event, "/item/type").as_deref() {
                Some("agent_message") => {
                    self.message = text(&event, "/item/text");
                    vec![]
                }
                Some("reasoning") | Some("error") => {
                    let said = text(&event, "/item/text").or_else(|| text(&event, "/item/message"));
                    said.map(Event::Log).into_iter().collect()
                }
                _ => vec![],
            },
            "turn.completed" => {
                let count = |key: &str| {
                    event
                        .pointer(&format!("/usage/{key}"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                };
                self.usage.push(Usage {
                    // Without the Step's `model`, Codex picked its default,
                    // which can't be priced.
                    model: self
                        .model
                        .clone()
                        .unwrap_or_else(|| "codex default".to_owned()),
                    input_tokens: count("input_tokens"),
                    cached_input_tokens: count("cached_input_tokens"),
                    // Reasoning tokens are part of the output count.
                    output_tokens: count("output_tokens"),
                    usd: None,
                });
                vec![Event::Log(line.to_owned())]
            }
            "turn.failed" | "error" => {
                let why = text(&event, "/error/message")
                    .or_else(|| text(&event, "/message"))
                    .unwrap_or_else(|| line.to_owned());
                self.failure = Some(why.clone());
                vec![Event::Log(why)]
            }
            _ => vec![],
        }
    }

    fn finish(&mut self, status: ExitStatus) -> Finished {
        let usage = std::mem::take(&mut self.usage);
        let answer = match (self.failure.take(), self.message.take()) {
            (Some(why), _) => Err(format!("codex failed: {why}")),
            (None, Some(message)) => serde_json::from_str::<Value>(&message)
                .map_err(|_| format!("codex answered with something other than JSON: {message}")),
            (None, None) => Err(format!("codex exited ({status}) without a review")),
        };
        Finished {
            answer: Some(answer),
            usage,
        }
    }
}

fn shorten(text: &str) -> String {
    match text.char_indices().nth(120) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// Serves `slopwatchd plugin codex run`.
pub fn run() -> std::io::Result<()> {
    review::run(Codex::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use slopwatch_protocol::RunId;
    use std::collections::BTreeMap;

    fn step_start(with: Value) -> (Config, Start) {
        let config = Config::parse(with.as_object().unwrap(), "codex").unwrap();
        let start = Start {
            run: RunId(1),
            step: "review".into(),
            config: with.as_object().unwrap().clone(),
            snapshot: review::tests::snapshot(),
            upstream: BTreeMap::new(),
            gate_failing: vec![],
            ci_logs: vec![],
            budget_usd: None,
        };
        (config, start)
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_review_runs_read_only_without_user_or_repo_config() {
        let (config, start) =
            step_start(json!({ "prompt": "x", "model": "gpt-5.4", "effort": "low" }));

        let args = args_of(&Codex::default().command(&config, &start, Path::new("/s.json")));

        for flag in [
            "--ignore-user-config",
            "--ignore-rules",
            "--ephemeral",
            "--json",
        ] {
            assert!(args.contains(&flag.to_owned()), "{flag} in {args:?}");
        }
        assert!(args.windows(2).any(|w| w == ["--disable", "hooks"]));
        assert!(args.windows(2).any(|w| w == ["--disable", "plugins"]));
        assert!(args.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert!(args.windows(2).any(|w| w == ["--output-schema", "/s.json"]));
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-5.4"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["-c", "model_reasoning_effort=\"low\""])
        );
        assert_eq!(args.last().map(String::as_str), Some("-"));

        let (config, start) = step_start(json!({ "prompt": "x", "repo_config": true }));
        let open = args_of(&Codex::default().command(&config, &start, Path::new("/s.json")));
        assert!(!open.contains(&"--ignore-user-config".to_owned()));
    }

    #[test]
    fn the_last_agent_message_is_the_review_and_the_turn_its_usage() {
        let mut codex = Codex {
            model: Some("gpt-5.4".into()),
            ..Codex::default()
        };
        let lines = [
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"item.started","item":{"id":"i0","type":"command_execution","command":"rg unwrap","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"{\"summary\":\"ok\",\"findings\":[]}"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"cache_write_input_tokens":60,"output_tokens":9,"reasoning_output_tokens":4}}"#,
        ];
        let events: Vec<Event> = lines.iter().flat_map(|line| codex.event(line)).collect();

        assert!(events.contains(&Event::Progress("Running rg unwrap".into())));
        let finished = codex.finish(success());
        assert_eq!(
            finished.answer,
            Some(Ok(json!({ "summary": "ok", "findings": [] })))
        );
        assert_eq!(
            finished.usage,
            [Usage {
                model: "gpt-5.4".into(),
                input_tokens: 100,
                cached_input_tokens: 40,
                output_tokens: 9,
                usd: None,
            }]
        );
    }

    #[test]
    fn a_failed_turn_says_why() {
        let mut codex = Codex::default();
        codex.event(r#"{"type":"turn.failed","error":{"message":"401 Unauthorized"}}"#);
        assert_eq!(
            codex.finish(success()).answer,
            Some(Err("codex failed: 401 Unauthorized".into()))
        );

        let mut codex = Codex::default();
        codex.event(
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"not json"}}"#,
        );
        assert!(matches!(
            codex.finish(success()).answer,
            Some(Err(why)) if why.contains("other than JSON")
        ));
    }

    fn success() -> ExitStatus {
        std::process::Command::new("true").status().unwrap()
    }
}
