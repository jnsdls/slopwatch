//! The built-in `claude` Plugin: Claude Code as a reviewer, through
//! `claude -p` with stream-json output and a JSON Schema for the answer.
//! [`review`](super::review) has the `with:` keys.
//!
//! Repo-controlled config stays off unless the Step sets `repo_config`. On
//! an API key that's `--bare`. On a subscription it's `--safe-mode`,
//! because `--bare` never reads the stored login: it skips hooks, MCP
//! servers, plugins and CLAUDE.md the same way and keeps the login. Both
//! add `--strict-mcp-config`, and the reviewer only gets the read-only
//! tools.
//!
//! On a subscription the CLI reads its login from the Keychain under
//! `$USER`, which the daemon passes every Step. A login kept outside
//! `~/.claude` comes through the Plugin's config directory setting in the
//! daemon, handed to the CLI as `CLAUDE_CONFIG_DIR`.
//!
//! Claude reports its own list-price cost per model, which becomes the
//! Step's usage. The `system/init` line says which credential it used, and
//! the Step log keeps it with any `rate_limit_event`.

use std::path::Path;
use std::process::{Command, ExitStatus};

use serde_json::Value;
use slopwatch_core::Workspace;
use slopwatch_protocol::step::{
    CONFIG_DIR_ENV, Manifest, PR_DIFF, STEP_DIALECT, SecretSpec, Start, Usage,
};

use super::review::{self, Agent, Auth, Config, Event, Finished};

/// The Secret an `auth: api_key` Step runs on.
pub const API_KEY: &str = "ANTHROPIC_API_KEY";

pub fn manifest() -> Manifest {
    Manifest {
        id: "claude".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        dialect: STEP_DIALECT,
        features: vec![PR_DIFF.into()],
        config_schema: review::config_schema(),
        workspace: Workspace::Read,
        effects: vec![],
        // Only `auth: api_key` needs it, so a subscription Step runs
        // without it.
        secrets: vec![SecretSpec::optional(API_KEY)],
        // Step contract: agent Steps get 30 minutes and a 5 minute stall.
        timeout: Some("30m".into()),
        stall_after: Some("5m".into()),
        concurrency: None,
    }
}

/// The tools a reviewer gets: reading, nothing that runs or writes.
const TOOLS: &str = "Read,Grep,Glob";

#[derive(Debug, Default)]
pub struct Claude {
    result: Option<Value>,
}

impl Agent for Claude {
    const NAME: &'static str = "claude";
    const API_KEY: &'static str = API_KEY;

    fn command(&mut self, config: &Config, start: &Start, _schema: &Path) -> Command {
        let mut command = Command::new(&config.cli);
        command.args([
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--permission-mode",
            "dontAsk",
            "--tools",
            TOOLS,
            "--json-schema",
            &review::schema().to_string(),
        ]);
        if !config.repo_config {
            command.arg(match config.auth {
                Auth::Subscription => "--safe-mode",
                Auth::ApiKey => "--bare",
            });
            command.arg("--strict-mcp-config");
        }
        command.env_remove(CONFIG_DIR_ENV);
        if config.auth == Auth::Subscription
            && let Some(dir) = std::env::var_os(CONFIG_DIR_ENV)
        {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
        if let Some(model) = &config.model {
            command.args(["--model", model]);
        }
        if let Some(effort) = &config.effort {
            command.args(["--effort", effort]);
        }
        if let Some(budget) = review::budget(start) {
            command.args(["--max-budget-usd", &budget]);
        }
        // The diff sits in a directory of the PR's own, outside the
        // worktree the tools may read, so a diff too long for the prompt
        // needs it named.
        if let Some(dir) = start.snapshot.diff.as_deref().and_then(Path::parent) {
            command.arg("--add-dir").arg(dir);
        }
        command
    }

    fn event(&mut self, line: &str) -> Vec<Event> {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return vec![Event::Log(line.to_owned())];
        };
        let text = |key: &str| event.get(key).and_then(Value::as_str).unwrap_or_default();
        match (text("type"), text("subtype")) {
            ("system", "init") => {
                let servers = event
                    .get("mcp_servers")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                vec![Event::Log(format!(
                    "model {}, credential {}, {servers} MCP servers, tools {}",
                    text("model"),
                    text("apiKeySource"),
                    event.get("tools").unwrap_or(&Value::Null),
                ))]
            }
            ("assistant", _) => assistant(&event),
            ("result", _) => {
                self.result = Some(event);
                vec![]
            }
            ("rate_limit_event", _) => vec![Event::Log(line.to_owned())],
            _ => vec![],
        }
    }

    fn finish(&mut self, status: ExitStatus) -> Finished {
        let Some(result) = self.result.take() else {
            return Finished {
                answer: Some(Err(format!(
                    "claude exited ({status}) before it reported a result"
                ))),
                usage: vec![],
            };
        };
        Finished {
            usage: usage(&result),
            answer: Some(answer(&result)),
        }
    }
}

/// What the assistant did in one message: tool calls go to the PR pane,
/// text to the log.
fn assistant(event: &Value) -> Vec<Event> {
    let Some(content) = event.pointer("/message/content").and_then(Value::as_array) else {
        return vec![];
    };
    content
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str)? {
            "tool_use" => {
                let name = block.get("name").and_then(Value::as_str)?;
                let input = block.get("input").unwrap_or(&Value::Null);
                let target = ["file_path", "pattern", "path"]
                    .iter()
                    .find_map(|key| input.get(key).and_then(Value::as_str));
                Some(Event::Progress(match target {
                    Some(target) => format!("{name} {target}"),
                    None => name.to_owned(),
                }))
            }
            "text" => {
                let text = block.get("text").and_then(Value::as_str)?.trim();
                (!text.is_empty()).then(|| Event::Log(text.to_owned()))
            }
            _ => None,
        })
        .collect()
}

/// The review from a `result` line, or why there isn't one.
fn answer(result: &Value) -> Result<Value, String> {
    let subtype = result
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if result.get("is_error").and_then(Value::as_bool) == Some(true) || subtype != "success" {
        let why = result
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                let errors = result.get("errors")?.as_array()?;
                Some(
                    errors
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            })
            .filter(|why| !why.is_empty())
            .unwrap_or_else(|| subtype.to_owned());
        return Err(format!("claude failed: {why}"));
    }
    match result.get("structured_output") {
        Some(answer) if answer.is_object() => Ok(answer.clone()),
        _ => Err("claude finished without the structured review it was asked for".to_owned()),
    }
}

/// What the run spent, per model. `modelUsage` counts subagents too,
/// which `usage` doesn't.
fn usage(result: &Value) -> Vec<Usage> {
    let count = |usage: &Value, key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let by_model: Vec<Usage> = result
        .get("modelUsage")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(model, usage)| {
            let cached = count(usage, "cacheReadInputTokens");
            Usage {
                model: model.clone(),
                input_tokens: count(usage, "inputTokens")
                    + cached
                    + count(usage, "cacheCreationInputTokens"),
                cached_input_tokens: cached,
                output_tokens: count(usage, "outputTokens"),
                // Tokens priced at zero would read as free. That's unknown.
                usd: usage
                    .get("costUSD")
                    .and_then(Value::as_f64)
                    .filter(|usd| *usd > 0.0 || count(usage, "outputTokens") == 0),
            }
        })
        .collect();
    if !by_model.is_empty() {
        return by_model;
    }
    match result.get("total_cost_usd").and_then(Value::as_f64) {
        Some(usd) if usd > 0.0 => vec![Usage {
            model: "claude".to_owned(),
            usd: Some(usd),
            ..Usage::default()
        }],
        _ => vec![],
    }
}

/// Serves `slopwatchd plugin claude run`.
pub fn run() -> std::io::Result<()> {
    review::run(Claude::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use slopwatch_protocol::RunId;
    use std::collections::BTreeMap;

    fn step_start(with: Value) -> (Config, Start) {
        let config = Config::parse(with.as_object().unwrap(), "claude").unwrap();
        let start = Start {
            run: RunId(1),
            step: "review".into(),
            config: with.as_object().unwrap().clone(),
            snapshot: review::tests::snapshot(),
            upstream: BTreeMap::new(),
            budget_usd: Some(2.0),
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
    fn a_subscription_step_keeps_the_login_but_loads_no_repo_config() {
        let (config, start) = step_start(json!({ "prompt": "x", "model": "opus" }));

        let args = args_of(&Claude::default().command(&config, &start, Path::new("/s.json")));

        assert!(args.contains(&"--safe-mode".to_owned()), "{args:?}");
        assert!(!args.contains(&"--bare".to_owned()));
        assert!(args.contains(&"--strict-mcp-config".to_owned()));
        assert!(args.windows(2).any(|w| w == ["--tools", TOOLS]));
        assert!(args.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(args.windows(2).any(|w| w == ["--max-budget-usd", "2.00"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["--add-dir", "/data/diffs/o/r/7"])
        );
    }

    #[test]
    fn an_api_key_step_runs_bare_and_repo_config_opts_back_in() {
        let (config, start) = step_start(json!({ "prompt": "x", "auth": "api_key" }));
        let bare = args_of(&Claude::default().command(&config, &start, Path::new("/s.json")));
        assert!(bare.contains(&"--bare".to_owned()));

        let (config, start) = step_start(json!({ "prompt": "x", "repo_config": true }));
        let open = args_of(&Claude::default().command(&config, &start, Path::new("/s.json")));
        assert!(!open.contains(&"--safe-mode".to_owned()));
        assert!(!open.contains(&"--strict-mcp-config".to_owned()));
    }

    #[test]
    fn a_result_gives_the_review_and_the_cost_per_model() {
        let mut claude = Claude::default();
        let init = r#"{"type":"system","subtype":"init","model":"claude-opus-5-5","apiKeySource":"none","mcp_servers":[],"tools":["Read"]}"#;
        let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/a.rs"}},{"type":"text","text":"Looking."}]}}"#;
        let result = json!({
            "type": "result", "subtype": "success", "is_error": false,
            "structured_output": { "summary": "ok", "findings": [] },
            "total_cost_usd": 0.31,
            "modelUsage": {
                "claude-opus-5-5": {
                    "inputTokens": 10, "outputTokens": 20, "cacheReadInputTokens": 100,
                    "cacheCreationInputTokens": 5, "costUSD": 0.3,
                },
                "claude-haiku-4-5": { "inputTokens": 1, "outputTokens": 1, "costUSD": 0.01 },
            },
        });

        assert_eq!(
            claude.event(init),
            [Event::Log(
                "model claude-opus-5-5, credential none, 0 MCP servers, tools [\"Read\"]".into()
            )]
        );
        assert_eq!(
            claude.event(read),
            [
                Event::Progress("Read src/a.rs".into()),
                Event::Log("Looking.".into())
            ]
        );
        assert!(claude.event(&result.to_string()).is_empty());
        let finished = claude.finish(success());

        assert_eq!(
            finished.answer,
            Some(Ok(json!({ "summary": "ok", "findings": [] })))
        );
        assert_eq!(finished.usage.len(), 2);
        let opus = finished
            .usage
            .iter()
            .find(|u| u.model == "claude-opus-5-5")
            .unwrap();
        assert_eq!(opus.input_tokens, 115);
        assert_eq!(opus.cached_input_tokens, 100);
        assert_eq!(opus.output_tokens, 20);
        assert_eq!(opus.usd, Some(0.3));
    }

    #[test]
    fn a_failed_run_says_why_and_still_counts_what_it_spent() {
        let mut claude = Claude::default();
        claude.event(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login","total_cost_usd":0,"modelUsage":{}}"#,
        );
        let finished = claude.finish(success());
        assert_eq!(
            finished.answer,
            Some(Err(
                "claude failed: Not logged in · Please run /login".into()
            ))
        );
        assert!(finished.usage.is_empty());

        let mut claude = Claude::default();
        claude.event(
            r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"errors":["over budget"],"total_cost_usd":2.1,"modelUsage":{}}"#,
        );
        let finished = claude.finish(success());
        assert_eq!(
            finished.answer,
            Some(Err("claude failed: over budget".into()))
        );
        assert_eq!(finished.usage[0].usd, Some(2.1));

        let mut claude = Claude::default();
        let finished = claude.finish(success());
        assert!(matches!(finished.answer, Some(Err(why)) if why.contains("before it reported")));
    }

    #[test]
    fn tokens_priced_at_zero_count_as_unknown_never_free() {
        let mut claude = Claude::default();
        claude.event(
            &json!({
                "type": "result", "subtype": "success", "is_error": false,
                "structured_output": { "summary": "", "findings": [] },
                "modelUsage": { "claude-opus-5-5": {
                    "inputTokens": 10, "outputTokens": 20, "costUSD": 0.0,
                } },
            })
            .to_string(),
        );
        assert_eq!(claude.finish(success()).usage[0].usd, None);
    }

    fn success() -> ExitStatus {
        std::process::Command::new("true").status().unwrap()
    }
}
