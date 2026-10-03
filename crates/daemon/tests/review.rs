//! The `claude` and `codex` review Steps end to end: the built-in Plugins
//! run as real processes against the fake GitHub, in a worktree of the
//! PR's head, and drive fake CLIs. Each fake records how it was run and
//! prints the output the test gives it.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use slopwatch_core::{EndReason, Resolver, Verdict, load};
use slopwatch_daemon::github::GitHub;
use slopwatch_daemon::github::fake::FakeGitHub;
use slopwatch_daemon::plugins::Plugins;
use slopwatch_daemon::plugins::review::Config;
use slopwatch_daemon::secrets::MemoryKeychain;
use slopwatch_daemon::store::Store;
use slopwatch_daemon::{Daemon, Library, Retention, Runs, RunsConfig, Watching};
use slopwatch_protocol::step::{Finding, Outputs, Severity, Usage};
use slopwatch_protocol::{PluginSettings, RepoName, RunEvent, RunId, RunView, SecretValue};

const WAIT: Duration = Duration::from_secs(30);

/// A fake CLI: records its arguments, working directory, the files there,
/// the API keys it got and its prompt in `<control>/<name>.*`, then prints
/// `<control>/<name>.reply`.
const FAKE_CLI: &str = r#"#!/bin/sh
out="$(dirname "$0")/$(basename "$0")"
printf '%s\n' "$@" > "$out.args"
pwd > "$out.cwd"
ls > "$out.ls"
printf '%s' "${ANTHROPIC_API_KEY-unset}" > "$out.anthropic"
printf '%s' "${CODEX_API_KEY-unset}" > "$out.codex"
printf '%s' "${CLAUDE_CONFIG_DIR-unset}" > "$out.config"
printf '%s' "${USER-unset}" > "$out.user"
cat > "$out.prompt"
cat "$out.reply"
"#;

const KEY: &str = "sk-test-72-0123456789abcdef";

fn repo() -> RepoName {
    RepoName::new("jnsdls", "app")
}

/// My PR 1 in `jnsdls/app`, labelled, with a one-Step Pipeline on main
/// that runs `plugin` through the fake CLI with `with`.
struct World {
    _reaper: support::Reaper,
    github: Arc<FakeGitHub>,
    data: tempfile::TempDir,
    control: tempfile::TempDir,
}

impl World {
    fn new(plugin: &str, with: serde_json::Value) -> Self {
        let control = tempfile::tempdir().unwrap();
        let cli = control.path().join(plugin);
        std::fs::write(&cli, FAKE_CLI).unwrap();
        std::fs::set_permissions(&cli, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let mut with = with;
        with["cli"] = json!(cli.to_str().unwrap());
        with["prompt"] = json!("Find bugs.");
        // Budgets well past what the fake CLIs report, so the reviews
        // aren't stopped.
        let pipeline = format!(
            "version: 1\nbudget_usd: 20\nsteps:\n  review: {{ uses: {plugin}, budget_usd: 20, with: \
             {with} }}\ngate: [review]\n"
        );
        let github = Arc::new(FakeGitHub::new("me"));
        github.add_repo(&repo());
        github.set_pipeline(&repo(), "main", &pipeline);
        github.open_pr(&repo(), 1, "me", "Add the thing");
        github.label_on_github(&repo(), 1, true);
        Self {
            _reaper: support::Reaper::new(control.path()),
            github,
            data: tempfile::tempdir().unwrap(),
            control,
        }
    }

    fn reply(&self, plugin: &str, lines: &[serde_json::Value]) {
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(self.recorded_path(plugin, "reply"), text).unwrap();
    }

    fn recorded_path(&self, plugin: &str, what: &str) -> PathBuf {
        self.control.path().join(format!("{plugin}.{what}"))
    }

    fn recorded(&self, plugin: &str, what: &str) -> String {
        std::fs::read_to_string(self.recorded_path(plugin, what)).unwrap()
    }

    fn store(&self) -> Store {
        Store::open(&self.data.path().join("state.db")).unwrap()
    }

    /// Runs a daemon until the PR's first Run ends, setting the Secrets
    /// `secrets` first, and returns the Run.
    fn first_run(&self, secrets: &[(&str, &str)]) -> RunId {
        self.first_run_with(secrets, PluginSettings::default())
    }

    /// [`World::first_run`], with the `claude` Plugin's settings set to
    /// `settings`.
    fn first_run_with(&self, secrets: &[(&str, &str)], settings: PluginSettings) -> RunId {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = self.store();
            let github: Arc<dyn GitHub> = Arc::clone(&self.github) as Arc<dyn GitHub>;
            let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&github)).unwrap());
            let library = Arc::new(Library::open(self.data.path().join("steps")).unwrap());
            let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library));
            let runs = Runs::start(
                store,
                github,
                Arc::clone(&watching),
                RunsConfig {
                    data_dir: self.data.path().to_owned(),
                    plugins,
                    login_path: None,
                    retention: Retention::default(),
                    keychain: Arc::new(MemoryKeychain::default()),
                },
            )
            .unwrap();
            runs.set_plugin_settings("claude", settings).await.unwrap();
            for (name, value) in secrets {
                runs.set_secret((*name).to_owned(), SecretValue::new(*value))
                    .await
                    .unwrap();
            }
            watching.add_repo(repo()).await.unwrap();
            let daemon = Daemon::with_build_id("test", watching, library).with_runs(runs);
            until(&daemon, "the first Run to end", || {
                self.runs()
                    .first()
                    .is_some_and(|run| self.ended(*run).is_some())
            })
            .await;
        });
        self.runs()[0]
    }

    fn runs(&self) -> Vec<RunId> {
        self.store()
            .run_summaries(&repo(), 1, 20)
            .unwrap()
            .iter()
            .map(|run| run.id)
            .collect()
    }

    fn events(&self, run: RunId) -> Vec<RunEvent> {
        self.store()
            .events_after(run, 0)
            .unwrap()
            .into_iter()
            .map(|stored| serde_json::from_str(&stored.event).unwrap())
            .collect()
    }

    fn view(&self, run: RunId) -> RunView {
        let mut view = RunView::default();
        for (seq, event) in (1..).zip(self.events(run)) {
            view.apply(seq, event);
        }
        view
    }

    fn ended(&self, run: RunId) -> Option<EndReason> {
        self.events(run).iter().find_map(|event| match event {
            RunEvent::Ended { reason, .. } => Some(*reason),
            _ => None,
        })
    }

    fn settled(&self, run: RunId) -> (Verdict, Option<String>, Outputs) {
        self.events(run)
            .into_iter()
            .rev()
            .find_map(|event| match event {
                RunEvent::StepSettled {
                    verdict,
                    reason,
                    outputs,
                    ..
                } => Some((verdict, reason, outputs)),
                _ => None,
            })
            .expect("the Step settled")
    }

    fn usage(&self, run: RunId) -> Vec<Usage> {
        self.events(run)
            .into_iter()
            .filter_map(|event| match event {
                RunEvent::StepUsage { usage, .. } => Some(usage),
                _ => None,
            })
            .collect()
    }
}

/// Polls until `done` holds.
async fn until(daemon: &Daemon, what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let _ = daemon.poll().await;
        if done() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// Every Step log file under `dir`, joined.
fn logs(dir: &Path) -> String {
    let mut text = String::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            text.push_str(&logs(&path));
        } else if let Ok(read) = std::fs::read_to_string(&path) {
            text.push_str(&read);
        }
    }
    text
}

/// What `claude -p --output-format stream-json` prints for a review that
/// found `findings` and cost `usd`.
fn claude_reply(findings: serde_json::Value, usd: f64) -> Vec<serde_json::Value> {
    vec![
        json!({
            "type": "system", "subtype": "init", "model": "claude-opus-5-5",
            "apiKeySource": "none", "mcp_servers": [], "tools": ["Glob", "Grep", "Read"],
        }),
        json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "tool_use", "name": "Read", "input": { "file_path": "change-1.txt" } },
            ] },
        }),
        json!({
            "type": "result", "subtype": "success", "is_error": false,
            "structured_output": { "summary": "One problem.", "findings": findings },
            "total_cost_usd": usd,
            "modelUsage": { "claude-opus-5-5": {
                "inputTokens": 1000, "outputTokens": 200, "cacheReadInputTokens": 5000,
                "cacheCreationInputTokens": 0, "costUSD": usd,
            } },
        }),
    ]
}

#[test]
fn a_claude_review_reports_findings_and_cost_from_a_worktree_of_the_head() {
    let world = World::new("claude", json!({}));
    world.reply(
        "claude",
        &claude_reply(
            json!([{ "severity": "error", "file": "change-1.txt", "line": 1, "message": "It breaks." }]),
            0.12,
        ),
    );

    // A set API key stays out of a subscription Step.
    let run = world.first_run(&[("ANTHROPIC_API_KEY", KEY)]);

    assert_eq!(world.ended(run), Some(EndReason::NotShippable));
    let (verdict, reason, outputs) = world.settled(run);
    assert_eq!((verdict, reason), (Verdict::Fail, None));
    assert_eq!(
        outputs.findings,
        [Finding {
            severity: Severity::Error,
            message: "It breaks.".into(),
            file: Some("change-1.txt".into()),
            line: Some(1),
        }]
    );
    assert_eq!(
        outputs.note.as_deref(),
        Some("1 finding, 1 at error or above. One problem.")
    );
    let view = world.view(run);
    assert_eq!(
        view.step("review").unwrap().cost.unwrap().to_string(),
        "$0.12"
    );
    assert_eq!(world.usage(run)[0].cached_input_tokens, 5000);

    let args = world.recorded("claude", "args");
    let args: Vec<&str> = args.lines().collect();
    for flag in ["-p", "--safe-mode", "--strict-mcp-config", "--json-schema"] {
        assert!(args.contains(&flag), "{flag} in {args:?}");
    }
    assert!(!args.contains(&"--bare"));
    assert_eq!(world.recorded("claude", "anthropic"), "unset");
    assert_eq!(
        world.recorded("claude", "config"),
        "unset",
        "no setting, the CLI's default"
    );
    assert_eq!(
        world.recorded("claude", "user"),
        std::env::var("USER").unwrap(),
        "the CLI finds its Keychain login under $USER"
    );
    // The working directory was a checkout of the PR's head, gone now.
    let cwd = world.recorded("claude", "cwd");
    assert!(cwd.contains("worktrees"), "{cwd}");
    assert!(
        !Path::new(cwd.trim()).exists(),
        "the worktree is cleaned up"
    );
    let files = world.recorded("claude", "ls");
    assert!(files.contains("change-1.txt"), "{files}");
    let prompt = world.recorded("claude", "prompt");
    assert!(prompt.contains("Add the thing"), "{prompt}");
    assert!(prompt.contains("+++ b/change-1.txt"), "the diff: {prompt}");
    assert!(prompt.contains("Find bugs."));
}

/// Settings that point Claude Steps at a config directory of their own.
fn claude_dir() -> PluginSettings {
    PluginSettings {
        config_dir: Some("/Users/me/.claude-work".into()),
        ..PluginSettings::default()
    }
}

#[test]
fn a_subscription_claude_step_logs_in_from_the_config_dir_in_its_plugin_settings() {
    let world = World::new("claude", json!({}));
    world.reply("claude", &claude_reply(json!([]), 0.05));

    let run = world.first_run_with(&[], claude_dir());

    assert_eq!(world.ended(run), Some(EndReason::Shippable));
    assert_eq!(world.recorded("claude", "config"), "/Users/me/.claude-work");
}

#[test]
fn an_api_key_claude_step_runs_bare_on_the_secret() {
    let world = World::new("claude", json!({ "auth": "api_key", "fail_on": "warning" }));
    world.reply("claude", &claude_reply(json!([]), 0.05));

    let run = world.first_run_with(&[("ANTHROPIC_API_KEY", KEY)], claude_dir());

    assert_eq!(world.ended(run), Some(EndReason::Shippable));
    assert_eq!(world.recorded("claude", "anthropic"), KEY);
    assert_eq!(
        world.recorded("claude", "config"),
        "unset",
        "an API key needs no login"
    );
    let args = world.recorded("claude", "args");
    assert!(args.lines().any(|arg| arg == "--bare"), "{args}");
    assert!(!args.lines().any(|arg| arg == "--safe-mode"));
}

#[test]
fn an_api_key_step_without_its_secret_errors_saying_which() {
    let world = World::new("claude", json!({ "auth": "api_key" }));
    world.reply("claude", &claude_reply(json!([]), 0.05));

    let run = world.first_run(&[]);

    let (verdict, reason, _) = world.settled(run);
    assert_eq!(verdict, Verdict::Error);
    let reason = reason.unwrap();
    assert!(reason.starts_with("error(claude): "), "{reason}");
    assert!(reason.contains("ANTHROPIC_API_KEY"), "{reason}");
    assert!(
        !world.recorded_path("claude", "args").exists(),
        "the CLI never ran"
    );
}

#[test]
fn a_cli_that_isnt_logged_in_errors_with_its_own_words() {
    let world = World::new("claude", json!({}));
    world.reply(
        "claude",
        &[json!({
            "type": "result", "subtype": "success", "is_error": true,
            "result": "Not logged in · Please run /login", "total_cost_usd": 0, "modelUsage": {},
        })],
    );

    let run = world.first_run(&[]);

    let (verdict, reason, _) = world.settled(run);
    assert_eq!(verdict, Verdict::Error);
    assert_eq!(
        reason.as_deref(),
        Some("error(claude): claude failed: Not logged in · Please run /login")
    );
    assert_eq!(world.view(run).cost(), None, "it spent nothing");
}

#[test]
fn a_codex_review_is_priced_from_its_tokens_and_unknown_without_a_model() {
    let reply = |findings: serde_json::Value| {
        vec![
            json!({ "type": "thread.started", "thread_id": "t1" }),
            json!({ "type": "item.completed", "item": {
                "id": "i0", "type": "agent_message",
                "text": json!({ "summary": "Fine.", "findings": findings }).to_string(),
            } }),
            json!({ "type": "turn.completed", "usage": {
                "input_tokens": 2_000_000, "cached_input_tokens": 1_000_000,
                "output_tokens": 1_000_000, "reasoning_output_tokens": 10,
            } }),
        ]
    };
    let world = World::new("codex", json!({ "model": "gpt-5.4", "auth": "api_key" }));
    world.reply(
        "codex",
        &reply(json!([{ "severity": "info", "file": null, "line": null, "message": "Nit." }])),
    );

    let run = world.first_run(&[("OPENAI_API_KEY", KEY)]);

    assert_eq!(world.ended(run), Some(EndReason::Shippable));
    assert_eq!(world.settled(run).2.findings[0].message, "Nit.");
    // 1M fresh input at $2.50, 1M cached at $0.25, 1M output at $15.
    assert_eq!(
        world.view(run).cost().unwrap().to_string(),
        "$17.75",
        "{:?}",
        world.usage(run)
    );
    assert_eq!(world.recorded("codex", "codex"), KEY);
    let log = logs(&world.data.path().join("logs"));
    assert!(log.contains("codex: running"), "{log}");
    assert!(
        !log.contains("CODEX_API_KEY"),
        "the command's env stays out: {log}"
    );
    let args = world.recorded("codex", "args");
    for flag in [
        "exec",
        "--ignore-user-config",
        "--ignore-rules",
        "hooks",
        "plugins",
    ] {
        assert!(args.lines().any(|arg| arg == flag), "{flag} in {args}");
    }

    let world = World::new("codex", json!({}));
    world.reply("codex", &reply(json!([])));
    let run = world.first_run(&[("OPENAI_API_KEY", KEY)]);
    assert_eq!(world.view(run).cost().unwrap().to_string(), "+?");
    assert_eq!(world.recorded("codex", "codex"), "unset");
}

#[tokio::test]
async fn the_review_api_keys_list_as_optional_so_an_unset_one_isnt_missing() {
    let world = World::new("claude", json!({}));
    let store = world.store();
    let github: Arc<dyn GitHub> = Arc::clone(&world.github) as Arc<dyn GitHub>;
    let watching = Arc::new(Watching::new(store.clone(), Arc::clone(&github)).unwrap());
    let library = Arc::new(Library::open(world.data.path().join("steps")).unwrap());
    let runs = Runs::start(
        store,
        github,
        watching,
        RunsConfig {
            data_dir: world.data.path().to_owned(),
            plugins: Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), library),
            login_path: None,
            retention: Retention::default(),
            keychain: Arc::new(MemoryKeychain::default()),
        },
    )
    .unwrap();

    let listed = runs.list_secrets().unwrap();

    for (name, plugin) in [("ANTHROPIC_API_KEY", "claude"), ("OPENAI_API_KEY", "codex")] {
        let info = listed.iter().find(|info| info.name == name).unwrap();
        assert_eq!(info.granted_to, [plugin]);
        assert!(info.optional, "{name}");
    }
}

#[test]
fn the_shipped_review_presets_read_as_review_configs() {
    let home = tempfile::tempdir().unwrap();
    let library = Arc::new(Library::open(home.path().join("steps")).unwrap());
    let plugins = Plugins::new(env!("CARGO_BIN_EXE_slopwatchd"), Arc::clone(&library));
    let pipeline = load(
        "version: 1\nsteps:\n  claude: { uses: lib/claude-review }\n  codex: { uses: lib/codex-review }\ngate: [claude]\n",
        &plugins as &dyn Resolver,
    )
    .unwrap();

    for (step, cli) in [("claude", "claude"), ("codex", "codex")] {
        let config = Config::parse(&pipeline.step(step).unwrap().config, cli).unwrap();
        assert_eq!(config.fail_on, Severity::Error);
        assert!(config.model.is_some(), "{step} names a model to price");
    }
}
