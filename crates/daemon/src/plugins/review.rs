//! What the `claude` and `codex` Plugins share: they run an agent CLI
//! headless as a reviewer in a worktree of the PR's head, with tools that
//! only read. They ask it for Findings in a fixed JSON shape, and fail when
//! a Finding reaches the Step's `fail_on` severity. The `fix` Plugin runs
//! the same CLIs the same way, as a fixer that may edit ([`Job::Fix`]).
//!
//! A Step's `with:` keys:
//!
//! - `prompt` (required): what to look for. The Plugin wraps it with the
//!   PR's title, description, files and diff.
//! - `model`: the CLI's model name or alias. The CLI's default otherwise.
//! - `effort`: the reasoning effort, passed through to the CLI.
//! - `auth`: `subscription` (the default) runs on the CLI's own login.
//!   `api_key` runs on the Plugin's API key Secret instead.
//! - `fail_on`: `error` (the default), `warning` or `info`. The Step fails
//!   when any Finding is at least this severe.
//! - `repo_config`: `true` lets the CLI load the repo's hooks and MCP
//!   servers. Off by default, because the PR's branch controls them.
//! - `cli`: the executable, as an absolute path or a name on `PATH`, in
//!   place of the one the daemon's CLI settings name. The Pipeline is
//!   committed, so a path here only exists on the machine that wrote it.
//!
//! The CLI's environment is the Step's: `PATH`, `HOME` and the Secrets the
//! Plugin is granted. On a subscription, the API key Secret is taken out,
//! so a set key never switches billing silently.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Map, Value, json};
use slopwatch_core::Verdict;
use slopwatch_protocol::step::{
    CLI_ENV, Finding, FromStep, Outcome, Outputs, PrSnapshot, Severity, Start, ToStep, Usage,
};

/// The most diff a prompt carries inline. A longer one stays in its file,
/// and the prompt tells the reviewer where.
const INLINE_DIFF_BYTES: u64 = 200 * 1024;

/// The most changed paths a prompt lists.
const LISTED_FILES: usize = 300;

/// How a Step pays for the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// The CLI's own stored login, such as a Claude or ChatGPT plan.
    Subscription,
    /// The Plugin's API key Secret.
    ApiKey,
}

/// What a session asks the agent to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Read the PR and report Findings. Tools only read.
    Review,
    /// Fix the problems behind the Gate's failing terms by editing the
    /// worktree, which the daemon commits ([`super::fix`]).
    Fix,
}

/// A review or fix Step's `with:`.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub job: Job,
    /// What to look for.
    pub prompt: String,
    /// The CLI's model, or its default.
    pub model: Option<String>,
    /// The reasoning effort, or the CLI's default.
    pub effort: Option<String>,
    pub auth: Auth,
    /// The least severe Finding that fails the Step.
    pub fail_on: Severity,
    /// Let the CLI load the repo's hooks and MCP servers.
    pub repo_config: bool,
    /// The executable to run.
    pub cli: String,
}

/// Every key a review Step's `with:` takes.
const KEYS: &[&str] = &[
    "prompt",
    "model",
    "effort",
    "auth",
    "fail_on",
    "repo_config",
    "cli",
];

impl Config {
    /// Reads a Step's `with:`, or says what's wrong with it. `cli` is the
    /// executable when the Step names none.
    pub fn parse(with: &Map<String, Value>, cli: &str) -> Result<Config, String> {
        if let Some(key) = with.keys().find(|key| !KEYS.contains(&key.as_str())) {
            return Err(format!(
                "`with:` has an unknown key `{key}`. It takes {}.",
                KEYS.iter()
                    .map(|key| format!("`{key}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let text = |key: &str| -> Result<Option<String>, String> {
            match with.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
                Some(_) => Err(format!("`{key}` must be a non-empty string")),
            }
        };
        let prompt = text("prompt")?.ok_or("`with:` needs a `prompt` saying what to review")?;
        let auth = match text("auth")?.as_deref() {
            None | Some("subscription") => Auth::Subscription,
            Some("api_key") => Auth::ApiKey,
            Some(other) => {
                return Err(format!(
                    "`auth` is `subscription` or `api_key`, not `{other}`"
                ));
            }
        };
        let fail_on = match text("fail_on")?.as_deref() {
            None => Severity::Error,
            // A Library seeded before this build keeps the first presets'
            // scale, since seeding never rewrites a preset it wrote.
            Some("critical" | "high") => Severity::Error,
            Some("medium") => Severity::Warning,
            Some("low") => Severity::Info,
            Some(other) => severity(other).ok_or_else(|| {
                format!("`fail_on` is `error`, `warning` or `info`, not `{other}`")
            })?,
        };
        let repo_config = match with.get("repo_config") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(on)) => *on,
            Some(_) => return Err("`repo_config` must be true or false".to_owned()),
        };
        let cli = text("cli")?.unwrap_or_else(|| cli.to_owned());
        if cli.contains('/') && !Path::new(&cli).is_absolute() {
            return Err(format!(
                "`cli` must be an absolute path or a name on PATH, not `{cli}`"
            ));
        }
        Ok(Config {
            job: Job::Review,
            prompt,
            model: text("model")?,
            effort: text("effort")?,
            auth,
            fail_on,
            repo_config,
            cli,
        })
    }
}

/// The JSON Schema for a review Step's `with:`, for the manifest.
pub fn config_schema() -> Value {
    let text = json!({ "type": "string", "minLength": 1 });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["prompt"],
        "properties": {
            "prompt": text,
            "model": text,
            "effort": text,
            "auth": { "enum": ["subscription", "api_key"], "default": "subscription" },
            "fail_on": { "enum": ["error", "warning", "info"], "default": "error" },
            "repo_config": { "type": "boolean", "default": false },
            "cli": text,
        },
    })
}

/// The JSON Schema both CLIs hold the reviewer's answer to. OpenAI's
/// strict structured output wants every property required and no others,
/// so optional fields are nullable instead.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "findings"],
        "properties": {
            "summary": {
                "type": "string",
                "description": "One or two sentences on the PR as a whole.",
            },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["severity", "file", "line", "message"],
                    "properties": {
                        "severity": { "type": "string", "enum": ["error", "warning", "info"] },
                        "file": { "type": ["string", "null"] },
                        "line": { "type": ["integer", "null"] },
                        "message": { "type": "string" },
                    },
                },
            },
        },
    })
}

/// What the reviewer is told: the Step's prompt, wrapped with the PR.
pub fn prompt(config: &Config, snapshot: &PrSnapshot, diff: Option<&Diff>) -> String {
    let mut text = format!(
        "You are reviewing pull request #{} in {}: \"{}\".\n\n",
        snapshot.number, snapshot.repo, snapshot.title
    );
    if !snapshot.body.trim().is_empty() {
        text.push_str(&format!(
            "Its description:\n<description>\n{}\n</description>\n\n",
            snapshot.body.trim()
        ));
    }
    text.push_str(
        "The working directory is a checkout of the PR's head commit. Read files there for \
         context, and follow the repo's AGENTS.md or CLAUDE.md if it has one. Don't change any \
         file.\n\n",
    );
    match diff {
        Some(Diff::Inline(diff)) => text.push_str(&format!(
            "The PR's diff against {}:\n```diff\n{diff}\n```\n\n",
            snapshot.base
        )),
        Some(Diff::TooLong { path, bytes, files }) => {
            let listed = files.len().min(LISTED_FILES);
            text.push_str(&format!(
                "The PR changes {} files against {}:\n",
                files.len(),
                snapshot.base
            ));
            for file in &files[..listed] {
                text.push_str(&format!("- {file}\n"));
            }
            if listed < files.len() {
                text.push_str(&format!("- and {} more\n", files.len() - listed));
            }
            text.push_str(&format!(
                "\nIts diff is {bytes} bytes, too long to include here. Read it from {}.\n\n",
                path.display()
            ));
        }
        None => {}
    }
    text.push_str(&format!("What to review:\n{}\n\n", config.prompt.trim()));
    text.push_str(
        "Report only problems the PR introduces or the lines it touches. Give each one a \
         severity: `error` when the change is wrong or unsafe to merge, `warning` when it should \
         change before merging, `info` for anything smaller. Give the file path relative to the \
         repo root and the line in the PR's head where the problem is, or null when it isn't \
         about one place. Answer with the JSON object the output schema describes: a short \
         summary and the findings, an empty list if you found none.",
    );
    text
}

/// The PR's diff as the prompt carries it.
#[derive(Debug, Clone, PartialEq)]
pub enum Diff {
    Inline(String),
    /// Too long to include: where it is, its size and the paths it
    /// changes, so the reviewer knows where to look.
    TooLong {
        path: PathBuf,
        bytes: u64,
        files: Vec<String>,
    },
}

impl Diff {
    /// Reads the diff the daemon wrote for the Run, `None` if there's none.
    pub fn read(snapshot: &PrSnapshot) -> Option<Diff> {
        let path = snapshot.diff.as_ref()?;
        let text = std::fs::read_to_string(path).ok()?;
        let bytes = text.len() as u64;
        if bytes <= INLINE_DIFF_BYTES {
            return Some(Diff::Inline(text));
        }
        let files = text
            .lines()
            .filter_map(|line| line.strip_prefix("diff --git a/"))
            .filter_map(|paths| paths.split_once(" b/").map(|(_, to)| to.to_owned()))
            .collect();
        Some(Diff::TooLong {
            path: path.clone(),
            bytes,
            files,
        })
    }
}

/// Turns the reviewer's answer into the Step's Outcome.
pub fn outcome(answer: &Value, fail_on: Severity) -> Result<Outcome, String> {
    let summary = answer
        .get("summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let findings = answer
        .get("findings")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("the review has no findings list: {answer}"))?
        .iter()
        .map(finding)
        .collect::<Result<Vec<_>, _>>()?;
    let failing = findings
        .iter()
        .filter(|finding| finding.severity >= fail_on)
        .count();
    let verdict = if failing > 0 {
        Verdict::Fail
    } else {
        Verdict::Pass
    };
    let mut note = match findings.len() {
        0 => "No findings".to_owned(),
        1 => "1 finding".to_owned(),
        n => format!("{n} findings"),
    };
    if failing > 0 {
        note.push_str(&format!(", {failing} at {fail_on} or above"));
    }
    if !summary.is_empty() {
        note.push_str(&format!(". {summary}"));
    }
    Ok(Outcome {
        verdict,
        outputs: Outputs {
            findings,
            note: Some(note),
            ..Outputs::default()
        },
    })
}

/// A severity by its wire name.
fn severity(name: &str) -> Option<Severity> {
    serde_json::from_value(Value::String(name.to_owned())).ok()
}

/// One Finding in the reviewer's answer.
fn finding(value: &Value) -> Result<Finding, String> {
    let severity = value
        .get("severity")
        .and_then(Value::as_str)
        .and_then(severity)
        .ok_or_else(|| format!("a finding has no known severity: {value}"))?;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .ok_or_else(|| format!("a finding has no message: {value}"))?;
    Ok(Finding {
        severity,
        message: message.trim().to_owned(),
        file: value
            .get("file")
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
            .map(str::to_owned),
        line: value
            .get("line")
            .and_then(Value::as_u64)
            .and_then(|line| u32::try_from(line).ok())
            .filter(|line| *line > 0),
    })
}

/// What a review session needs from one CLI.
pub trait Agent {
    /// The Plugin's name, for log lines.
    const NAME: &'static str;
    /// The Secret an `api_key` Step runs on.
    const API_KEY: &'static str;

    /// The command that runs the review. The prompt goes to its stdin.
    /// `schema` is a file holding [`schema`].
    fn command(&mut self, config: &Config, start: &Start, schema: &Path) -> Command;

    /// Reads one line of the CLI's output.
    fn event(&mut self, line: &str) -> Vec<Event>;

    /// What the CLI answered, once it has exited with `status`.
    fn finish(&mut self, status: std::process::ExitStatus) -> Finished;
}

/// What a line of the CLI's output says.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A line for the PR pane, such as the file it reads.
    Progress(String),
    /// A line for the Step log.
    Log(String),
}

/// How the CLI's run ended.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Finished {
    /// The structured review, or why there's none.
    pub answer: Option<Result<Value, String>>,
    /// What the run spent, by model.
    pub usage: Vec<Usage>,
}

/// Runs one review session over the process's stdio: reads `start`, runs
/// the CLI, and reports its usage and the Outcome. A `cancel`, or the
/// daemon hanging up, stops the CLI.
pub fn run<A: Agent>(agent: A) -> std::io::Result<()> {
    let mut input = BufReader::new(std::io::stdin());
    let mut output = std::io::stdout().lock();
    let Some(start) = read_start(&mut input, A::NAME)? else {
        return Ok(());
    };
    let config = match Config::parse(&start.config, &default_cli(A::NAME)) {
        Ok(config) => config,
        Err(reason) => return send(&mut output, &FromStep::Error { reason }),
    };
    session(agent, &config, &start, input, &mut output)
}

/// The executable a Step runs when its `with:` names none: the one the
/// daemon's CLI settings name, handed over in [`CLI_ENV`], or else `name`.
pub fn default_cli(name: &str) -> String {
    std::env::var(CLI_ENV)
        .ok()
        .filter(|cli| !cli.is_empty())
        .unwrap_or_else(|| name.to_owned())
}

/// Reads the daemon's `start`, the first line of a session. `None` when
/// the daemon hung up or sent something else first.
pub fn read_start(input: &mut impl BufRead, name: &str) -> std::io::Result<Option<Start>> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    match serde_json::from_str::<ToStep>(&line) {
        Ok(ToStep::Start(start)) => Ok(Some(start)),
        Ok(_) | Err(_) => {
            eprintln!("{name}: the daemon didn't start with `start`");
            Ok(None)
        }
    }
}

/// Runs the CLI for `config`'s job once `start` is in, and reports its
/// usage and the Outcome on `output`. `input` is the rest of the daemon's
/// side, watched for `cancel`.
pub fn session<A: Agent>(
    mut agent: A,
    config: &Config,
    start: &Start,
    input: impl BufRead + Send + 'static,
    output: &mut impl Write,
) -> std::io::Result<()> {
    if config.auth == Auth::ApiKey && std::env::var_os(A::API_KEY).is_none() {
        let reason = format!(
            "`auth: api_key` runs on the Secret {}, which isn't set. Set it under Secrets, or \
             use `auth: subscription`.",
            A::API_KEY
        );
        return send(output, &FromStep::Error { reason });
    }

    let mut schema_file = tempfile::Builder::new()
        .prefix("slopwatch-schema")
        .suffix(".json")
        .tempfile()?;
    serde_json::to_writer(&mut schema_file, &schema_for(config.job))?;
    schema_file.flush()?;
    let mut command = agent.command(config, start, schema_file.path());
    if config.auth == Auth::Subscription {
        command.env_remove(A::API_KEY);
    }
    let prompt = match config.job {
        Job::Review => prompt(
            config,
            &start.snapshot,
            Diff::read(&start.snapshot).as_ref(),
        ),
        Job::Fix => super::fix::prompt(config, start),
    };
    // The arguments only: the env can hold an API key.
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy())
        .collect();
    eprintln!("{}: running {} {}", A::NAME, config.cli, args.join(" "));
    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let reason = format!("can't run `{}`: {error}", config.cli);
            return send(output, &FromStep::Error { reason });
        }
    };
    // The prompt goes in on a thread of its own, so a CLI that writes
    // before it has read all of a long one can't block on its stdout.
    let mut stdin = child.stdin.take().expect("stdin is piped");
    std::thread::spawn(move || {
        // A CLI that dies early closes its stdin, and its exit says why.
        let _ = stdin.write_all(prompt.as_bytes());
    });
    let exited = Arc::new(AtomicBool::new(false));
    let cancelled = watch_for_cancel(input, &child, Arc::clone(&exited));
    let stdout = child.stdout.take().expect("stdout is piped");
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        for event in agent.event(&line) {
            match event {
                Event::Progress(message) => {
                    eprintln!("{}: {message}", A::NAME);
                    send(
                        output,
                        &FromStep::Progress {
                            message: Some(message),
                        },
                    )?;
                }
                Event::Log(message) => eprintln!("{}: {message}", A::NAME),
            }
        }
    }
    // Its stdout closed, so it's exiting. Once reaped, its pid may belong
    // to someone else, and a late cancel mustn't signal that.
    exited.store(true, Ordering::Release);
    let status = child.wait()?;
    let finished = agent.finish(status);
    for usage in finished.usage {
        send(output, &FromStep::Usage(usage))?;
    }
    if cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let judged = |answer: &Value| match config.job {
        Job::Review => outcome(answer, config.fail_on),
        Job::Fix => super::fix::outcome(answer),
    };
    let message = match finished.answer {
        Some(Ok(answer)) => match judged(&answer) {
            Ok(outcome) => FromStep::Outcome(outcome),
            Err(reason) => FromStep::Error {
                reason: format!("{} answered in the wrong shape: {reason}", A::NAME),
            },
        },
        Some(Err(reason)) => FromStep::Error { reason },
        None => FromStep::Error {
            reason: format!("{} exited ({status}) without an answer", A::NAME),
        },
    };
    send(output, &message)
}

/// The JSON Schema the agent's answer to `job` holds to.
pub fn schema_for(job: Job) -> Value {
    match job {
        Job::Review => schema(),
        Job::Fix => super::fix::schema(),
    }
}

/// Stops the CLI when the daemon sends `cancel` or hangs up, unless
/// `exited` says it's already on its way out. Returns the flag that says
/// the Step was cancelled.
fn watch_for_cancel(
    mut input: impl BufRead + Send + 'static,
    child: &Child,
    exited: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let pid = child.id() as i32;
    let flag = Arc::clone(&cancelled);
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            let stop = match input.read_line(&mut line) {
                Ok(0) | Err(_) => true,
                Ok(_) => matches!(serde_json::from_str(&line), Ok(ToStep::Cancel)),
            };
            if stop {
                flag.store(true, Ordering::Release);
                if exited.load(Ordering::Acquire) {
                    return;
                }
                // SIGINT ends a Claude turn cleanly. The daemon's SIGTERM
                // and SIGKILL to the group follow if it doesn't stop.
                // SAFETY: kill only sends a signal to the CLI it spawned.
                unsafe { libc::kill(pid, libc::SIGINT) };
                return;
            }
        }
    });
    cancelled
}

/// Writes one protocol message to the daemon.
pub fn send(output: &mut impl Write, message: &FromStep) -> std::io::Result<()> {
    let line = serde_json::to_string(message)?;
    writeln!(output, "{line}")?;
    output.flush()
}

/// The `budget_usd` a Step got, written the way a CLI flag wants it.
pub fn budget(start: &Start) -> Option<String> {
    start
        .budget_usd
        .filter(|usd| *usd > 0.0)
        .map(|usd| format!("{usd:.2}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use slopwatch_protocol::RepoName;
    use slopwatch_protocol::step::Checks;

    fn with(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    pub(crate) fn snapshot() -> PrSnapshot {
        PrSnapshot {
            repo: RepoName::new("o", "r"),
            number: 7,
            title: "Add the thing".into(),
            body: "It adds the thing.".into(),
            url: String::new(),
            author: "me".into(),
            head_sha: "abc".into(),
            base: "main".into(),
            draft: false,
            labels: vec![],
            checks: Checks::default(),
            merge: None,
            diff: Some("/data/diffs/o/r/7/def-abc.diff".into()),
            linked_issues: vec![],
            stacked_on: None,
        }
    }

    #[test]
    fn a_config_takes_defaults_and_refuses_what_it_doesnt_know() {
        let config = Config::parse(&with(json!({ "prompt": "Find bugs." })), "claude").unwrap();
        assert_eq!(config.auth, Auth::Subscription);
        assert_eq!(config.fail_on, Severity::Error);
        assert!(!config.repo_config);
        assert_eq!(config.cli, "claude");

        let config = Config::parse(
            &with(json!({
                "prompt": "Find bugs.", "auth": "api_key", "fail_on": "warning",
                "model": "opus", "effort": "high", "repo_config": true, "cli": "/opt/claude",
            })),
            "claude",
        )
        .unwrap();
        assert_eq!(config.auth, Auth::ApiKey);
        assert_eq!(config.fail_on, Severity::Warning);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert!(config.repo_config);
        assert_eq!(config.cli, "/opt/claude");

        let refused = |value: Value| Config::parse(&with(value), "claude").unwrap_err();
        assert!(refused(json!({})).contains("needs a `prompt`"));
        assert!(refused(json!({ "prompt": "x", "fial_on": "error" })).contains("`fial_on`"));
        assert!(refused(json!({ "prompt": "x", "auth": "oauth" })).contains("`oauth`"));
        assert!(refused(json!({ "prompt": "x", "cli": "./claude" })).contains("absolute"));
        // The first presets' scale still reads.
        let high = Config::parse(&with(json!({ "prompt": "x", "fail_on": "high" })), "codex");
        assert_eq!(high.unwrap().fail_on, Severity::Error);
    }

    #[test]
    fn the_prompt_carries_the_pr_and_points_at_a_diff_too_long_to_include() {
        let config = Config::parse(&with(json!({ "prompt": "Find bugs." })), "claude").unwrap();

        let long = prompt(
            &config,
            &snapshot(),
            Some(&Diff::TooLong {
                path: "/data/diffs/o/r/7/def-abc.diff".into(),
                bytes: 300_000,
                files: vec!["src/thing.rs".into()],
            }),
        );
        assert!(long.contains("pull request #7 in o/r: \"Add the thing\""));
        assert!(long.contains("It adds the thing."));
        assert!(long.contains("- src/thing.rs"));
        assert!(long.contains("Read it from /data/diffs/o/r/7/def-abc.diff"));
        assert!(long.contains("Find bugs."));

        let short = prompt(
            &config,
            &snapshot(),
            Some(&Diff::Inline("+fn thing() {}".into())),
        );
        assert!(short.contains("```diff\n+fn thing() {}\n```"));
    }

    #[test]
    fn a_long_diff_stays_in_its_file_with_the_paths_it_changes_listed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pr.diff");
        let mut text = String::from("diff --git a/src/a.rs b/src/a.rs\n+short\n");
        std::fs::write(&path, &text).unwrap();
        let mut pr = snapshot();
        pr.diff = Some(path.clone());
        assert_eq!(Diff::read(&pr), Some(Diff::Inline(text.clone())));

        text.push_str("diff --git a/old.rs b/new.rs\n");
        text.push_str(&"+filler\n".repeat(30_000));
        std::fs::write(&path, &text).unwrap();
        assert_eq!(
            Diff::read(&pr),
            Some(Diff::TooLong {
                path,
                bytes: text.len() as u64,
                files: vec!["src/a.rs".into(), "new.rs".into()],
            })
        );
        pr.diff = None;
        assert_eq!(Diff::read(&pr), None);
    }

    #[test]
    fn a_finding_at_fail_on_or_above_fails_the_step() {
        let answer = json!({
            "summary": "Mostly fine.",
            "findings": [
                { "severity": "warning", "file": "src/thing.rs", "line": 4, "message": "Unwrap can panic." },
                { "severity": "info", "file": null, "line": null, "message": "Consider a test." },
            ],
        });

        let lenient = outcome(&answer, Severity::Error).unwrap();
        assert_eq!(lenient.verdict, Verdict::Pass);
        assert_eq!(
            lenient.outputs.note.as_deref(),
            Some("2 findings. Mostly fine.")
        );
        assert_eq!(
            lenient.outputs.findings[0],
            Finding {
                severity: Severity::Warning,
                message: "Unwrap can panic.".into(),
                file: Some("src/thing.rs".into()),
                line: Some(4),
            }
        );
        assert_eq!(lenient.outputs.findings[1].file, None);

        let strict = outcome(&answer, Severity::Warning).unwrap();
        assert_eq!(strict.verdict, Verdict::Fail);
        assert_eq!(
            strict.outputs.note.as_deref(),
            Some("2 findings, 1 at warning or above. Mostly fine.")
        );

        let clean = outcome(&json!({ "summary": "", "findings": [] }), Severity::Info).unwrap();
        assert_eq!(clean.verdict, Verdict::Pass);
        assert_eq!(clean.outputs.note.as_deref(), Some("No findings"));

        assert!(outcome(&json!({ "summary": "x" }), Severity::Error).is_err());
        assert!(
            outcome(
                &json!({ "findings": [{ "severity": "huge", "message": "x" }] }),
                Severity::Error
            )
            .is_err()
        );
    }
}
