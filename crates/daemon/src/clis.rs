//! How the daemon finds and runs the CLIs it depends on: `claude`,
//! `codex`, `gh` and `git`. The developer's settings say which executable
//! each one is, and for the agent CLIs, extra `PATH` dirs and a config
//! directory. They live in the daemon's store, never in a repo.
//!
//! An executable is a name or an absolute path. A name is looked up on the
//! `PATH` Steps get, after the CLI's own dirs. A CLI left at its usual
//! name is also looked for where Homebrew puts it, since launchd's `PATH`
//! has neither place. A name the developer sets replaces that fallback.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;
use slopwatch_protocol::{Cli, CliListing, CliSettings, CliStatus, Login};
use tokio::process::Command;

/// Where Homebrew puts CLIs, on Apple silicon and on Intel.
pub const HOMEBREW_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// How long `--version` or a login check may take before the CLI counts
/// as hung. Claude Code starts Node, which takes a moment on a cold cache.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The developer's settings for each CLI, shared by everything in the
/// daemon that runs one.
pub struct Clis {
    settings: Mutex<HashMap<Cli, CliSettings>>,
    /// Where a CLI left at its usual name is looked for after `PATH`.
    fallbacks: Vec<PathBuf>,
}

impl Default for Clis {
    fn default() -> Self {
        Self::new(HashMap::new())
    }
}

impl Clis {
    /// The settings the store kept.
    pub fn new(settings: HashMap<Cli, CliSettings>) -> Self {
        Self {
            settings: Mutex::new(settings),
            fallbacks: HOMEBREW_DIRS.iter().map(PathBuf::from).collect(),
        }
    }

    /// Looks for a CLI at its usual name in `dirs` after `PATH`, in place
    /// of Homebrew's. Tests use it.
    pub fn with_fallbacks(mut self, dirs: Vec<PathBuf>) -> Self {
        self.fallbacks = dirs;
        self
    }

    fn state(&self) -> MutexGuard<'_, HashMap<Cli, CliSettings>> {
        self.settings
            .lock()
            .expect("no panics while holding the CLI settings")
    }

    pub fn settings(&self, cli: Cli) -> CliSettings {
        self.state().get(&cli).cloned().unwrap_or_default()
    }

    /// Replaces the settings for `cli`. The store keeps them; this is what
    /// the next run of the CLI reads.
    pub fn set(&self, cli: Cli, settings: CliSettings) {
        let mut state = self.state();
        if settings == CliSettings::default() {
            state.remove(&cli);
        } else {
            state.insert(cli, settings);
        }
    }

    /// The `PATH` the CLI runs with: its own dirs in front of `base`.
    pub fn path(&self, cli: Cli, base: &str) -> String {
        self.settings(cli)
            .path
            .iter()
            .map(String::as_str)
            .chain((!base.is_empty()).then_some(base))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// The config directory the developer set for the CLI, if any.
    pub fn config_dir(&self, cli: Cli) -> Option<String> {
        self.settings(cli).config_dir
    }

    /// The file the CLI's executable resolves to on `base`, or why there's
    /// none.
    pub fn resolve(&self, cli: Cli, base: &str) -> Result<PathBuf, String> {
        let settings = self.settings(cli);
        let (executable, fallbacks) = match &settings.executable {
            Some(executable) => (executable.as_str(), &[][..]),
            None => (cli.name(), &self.fallbacks[..]),
        };
        resolve(executable, &self.path(cli, base), fallbacks)
    }

    /// What to run for the CLI: the file it resolves to, or else its
    /// executable as set, so running it fails saying what's missing.
    pub fn program(&self, cli: Cli, base: &str) -> String {
        match self.resolve(cli, base) {
            Ok(file) => file.display().to_string(),
            Err(_) => self
                .settings(cli)
                .executable
                .unwrap_or_else(|| cli.name().to_owned()),
        }
    }

    /// Every CLI with its settings and what trying it found, checked side
    /// by side.
    pub async fn listings(&self, base: &str) -> Vec<CliListing> {
        let statuses = tokio::join!(
            self.check(Cli::Claude, base),
            self.check(Cli::Codex, base),
            self.check(Cli::Gh, base),
            self.check(Cli::Git, base),
        );
        let (claude, codex, gh, git) = statuses;
        Cli::ALL
            .into_iter()
            .zip([claude, codex, gh, git])
            .map(|(cli, status)| CliListing {
                cli,
                settings: self.settings(cli),
                status,
            })
            .collect()
    }

    /// Resolves the CLI, asks it for `--version`, and for an agent CLI,
    /// whether it's logged in.
    pub async fn check(&self, cli: Cli, base: &str) -> CliStatus {
        let file = match self.resolve(cli, base) {
            Ok(file) => file,
            Err(problem) => {
                return CliStatus {
                    problem: Some(problem),
                    ..CliStatus::default()
                };
            }
        };
        let env = self.env(cli, base);
        let mut status = CliStatus {
            resolved: Some(file.display().to_string()),
            ..CliStatus::default()
        };
        match output(&file, &["--version"], &env).await {
            Ok(out) if out.success => status.version = first_line(&out.stdout, &out.stderr),
            Ok(out) => {
                let said = first_line(&out.stderr, &out.stdout);
                status.problem = Some(format!(
                    "`--version` failed: {}",
                    said.as_deref().unwrap_or("it printed nothing")
                ));
            }
            Err(problem) => status.problem = Some(problem),
        }
        if status.problem.is_none() {
            status.login = match cli {
                Cli::Claude => output(&file, &["auth", "status"], &env)
                    .await
                    .ok()
                    .and_then(|out| claude_login(&out.stdout)),
                Cli::Codex => output(&file, &["login", "status"], &env)
                    .await
                    .ok()
                    .and_then(|out| codex_login(&out)),
                Cli::Gh | Cli::Git => None,
            };
        }
        status
    }

    /// The env a check runs the CLI with: what a Step gets, so the answer
    /// is the one a Step would see, with the config directory where the
    /// CLI reads it.
    fn env(&self, cli: Cli, base: &str) -> Vec<(String, String)> {
        let mut env = vec![("PATH".to_owned(), self.path(cli, base))];
        for name in ["HOME", "USER", "LOGNAME"] {
            env.extend(
                std::env::var(name)
                    .ok()
                    .map(|value| (name.to_owned(), value)),
            );
        }
        let dir_var = match cli {
            Cli::Claude => Some("CLAUDE_CONFIG_DIR"),
            Cli::Codex => Some("CODEX_HOME"),
            Cli::Gh | Cli::Git => None,
        };
        if let (Some(var), Some(dir)) = (dir_var, self.config_dir(cli)) {
            env.push((var.to_owned(), dir));
        }
        env
    }
}

/// The file `executable` names: itself when it's an absolute path, or
/// else the first executable file by that name in `path`'s dirs, then in
/// `fallbacks`.
pub fn resolve(executable: &str, path: &str, fallbacks: &[PathBuf]) -> Result<PathBuf, String> {
    if executable.starts_with('/') {
        let file = Path::new(executable);
        return match runnable(file) {
            Ok(()) => Ok(file.to_owned()),
            Err(why) => Err(format!("`{executable}` {why}")),
        };
    }
    let dirs = path
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .chain(fallbacks.iter().cloned());
    for dir in dirs {
        let file = dir.join(executable);
        if runnable(&file).is_ok() {
            return Ok(file);
        }
    }
    let also = if fallbacks.is_empty() {
        String::new()
    } else {
        let dirs: Vec<String> = fallbacks
            .iter()
            .map(|dir| dir.display().to_string())
            .collect();
        format!(" or in {}", dirs.join(", "))
    };
    Err(format!("`{executable}` isn't on PATH{also}"))
}

/// Whether `file` is an executable file, or why not.
fn runnable(file: &Path) -> Result<(), &'static str> {
    let meta = std::fs::metadata(file).map_err(|_| "doesn't exist")?;
    if !meta.is_file() {
        return Err("isn't a file");
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err("isn't executable");
    }
    Ok(())
}

/// What a CLI printed and whether it succeeded.
struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Runs `file args` with only `env`, and gives up after [`CHECK_TIMEOUT`].
/// What goes wrong is said without the file, which the status names.
async fn output(file: &Path, args: &[&str], env: &[(String, String)]) -> Result<Output, String> {
    let mut command = Command::new(file);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let run = command.output();
    let output = match tokio::time::timeout(CHECK_TIMEOUT, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(format!("can't run it: {error}")),
        Err(_) => {
            return Err(format!(
                "`{}` didn't answer within {} s",
                args.join(" "),
                CHECK_TIMEOUT.as_secs()
            ));
        }
    };
    Ok(Output {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The first non-blank line of `text`, or of `or` when there's none.
fn first_line(text: &str, or: &str) -> Option<String> {
    text.lines()
        .chain(or.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The login `claude auth status` reports as JSON. `None` from a Claude
/// Code too old to say.
fn claude_login(stdout: &str) -> Option<Login> {
    let status: Value = serde_json::from_str(stdout.trim()).ok()?;
    let logged_in = status.get("loggedIn")?.as_bool()?;
    let text = |key: &str| status.get(key).and_then(Value::as_str);
    let detail = match (text("authMethod"), text("subscriptionType")) {
        (Some(method), Some(plan)) => Some(format!("{method} ({plan})")),
        (Some(method), None) => Some(method.to_owned()),
        (None, _) => None,
    };
    Some(Login {
        logged_in,
        detail: detail.filter(|_| logged_in),
    })
}

/// The login `codex login status` reports in words, on either stream.
/// `None` when it says neither, as from a Codex too old to have the
/// command.
fn codex_login(out: &Output) -> Option<Login> {
    let line = first_line(&out.stdout, &out.stderr)?;
    if out.success && line.to_lowercase().starts_with("logged in") {
        // "Logged in using ChatGPT": what follows "using" is how.
        let how = line
            .split_once(" using ")
            .map(|(_, how)| how.trim().to_owned());
        return Some(Login {
            logged_in: true,
            detail: how.filter(|how| !how.is_empty()),
        });
    }
    line.to_lowercase()
        .contains("not logged in")
        .then_some(Login {
            logged_in: false,
            detail: None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let file = dir.join(name);
        std::fs::write(&file, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        file
    }

    #[test]
    fn a_name_resolves_on_path_then_in_the_fallbacks_and_an_absolute_path_as_is() {
        let path = tempfile::tempdir().unwrap();
        let brew = tempfile::tempdir().unwrap();
        let on_path = script(path.path(), "claude", "true");
        let in_brew = script(brew.path(), "gh", "true");
        std::fs::write(path.path().join("plain"), "").unwrap();
        let base = path.path().to_str().unwrap();
        let fallbacks = vec![brew.path().to_owned()];

        assert_eq!(resolve("claude", base, &fallbacks), Ok(on_path.clone()));
        assert_eq!(resolve("gh", base, &fallbacks), Ok(in_brew));
        assert_eq!(
            resolve(on_path.to_str().unwrap(), "", &[]),
            Ok(on_path.clone())
        );
        let missing = resolve("gh", base, &[]).unwrap_err();
        assert_eq!(missing, "`gh` isn't on PATH");
        assert!(
            resolve("nope", base, &fallbacks)
                .unwrap_err()
                .contains(&format!("or in {}", brew.path().display()))
        );
        assert!(resolve("plain", base, &[]).is_err(), "not executable");
        let plain = path.path().join("plain");
        assert!(
            resolve(plain.to_str().unwrap(), "", &[])
                .unwrap_err()
                .ends_with("isn't executable")
        );
    }

    #[test]
    fn a_set_executable_replaces_the_usual_name_and_its_fallbacks() {
        let path = tempfile::tempdir().unwrap();
        let brew = tempfile::tempdir().unwrap();
        script(brew.path(), "gh", "true");
        let mine = script(path.path(), "mygh", "true");
        let base = path.path().to_str().unwrap();
        let clis = Clis::default().with_fallbacks(vec![brew.path().to_owned()]);
        assert_eq!(clis.resolve(Cli::Gh, base), Ok(brew.path().join("gh")));

        clis.set(
            Cli::Gh,
            CliSettings {
                executable: Some("mygh".into()),
                ..CliSettings::default()
            },
        );
        assert_eq!(clis.resolve(Cli::Gh, base), Ok(mine));

        clis.set(
            Cli::Gh,
            CliSettings {
                executable: Some("gh".into()),
                ..CliSettings::default()
            },
        );
        assert!(
            clis.resolve(Cli::Gh, base).is_err(),
            "a name set by hand isn't looked for in Homebrew's dirs"
        );
        assert_eq!(clis.program(Cli::Gh, base), "gh");
    }

    #[test]
    fn an_agent_cli_runs_with_its_own_dirs_in_front() {
        let clis = Clis::default();
        clis.set(
            Cli::Claude,
            CliSettings {
                path: vec!["/opt/a".into(), "/opt/b".into()],
                ..CliSettings::default()
            },
        );
        assert_eq!(clis.path(Cli::Claude, "/usr/bin"), "/opt/a:/opt/b:/usr/bin");
        assert_eq!(clis.path(Cli::Codex, "/usr/bin"), "/usr/bin");
    }

    #[tokio::test]
    async fn a_check_reports_the_file_the_version_and_the_login() {
        let dir = tempfile::tempdir().unwrap();
        script(
            dir.path(),
            "mclaude",
            r#"case "$1" in
--version) echo "2.1.0 (Claude Code)" ;;
auth) printf '{"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"max","dir":"%s"}' "$CLAUDE_CONFIG_DIR" ;;
esac"#,
        );
        script(
            dir.path(),
            "codex",
            r#"case "$1" in
--version) echo "codex-cli 0.159.0" ;;
login) echo "Not logged in" >&2; exit 1 ;;
esac"#,
        );
        script(dir.path(), "broken", r#"echo "boom" >&2; exit 3"#);
        let clis = Clis::default().with_fallbacks(vec![]);
        clis.set(
            Cli::Claude,
            CliSettings {
                executable: Some("mclaude".into()),
                config_dir: Some("/Users/me/.claude-work".into()),
                ..CliSettings::default()
            },
        );
        clis.set(
            Cli::Git,
            CliSettings {
                executable: Some("broken".into()),
                ..CliSettings::default()
            },
        );
        let base = dir.path().to_str().unwrap();

        let claude = clis.check(Cli::Claude, base).await;
        assert_eq!(
            claude.resolved,
            Some(dir.path().join("mclaude").display().to_string())
        );
        assert_eq!(claude.version.as_deref(), Some("2.1.0 (Claude Code)"));
        assert_eq!(
            claude.login,
            Some(Login {
                logged_in: true,
                detail: Some("claude.ai (max)".into()),
            })
        );
        assert_eq!(claude.problem, None);

        let codex = clis.check(Cli::Codex, base).await;
        assert_eq!(codex.version.as_deref(), Some("codex-cli 0.159.0"));
        assert_eq!(
            codex.login,
            Some(Login {
                logged_in: false,
                detail: None,
            })
        );

        let gh = clis.check(Cli::Gh, base).await;
        assert_eq!(gh.problem.as_deref(), Some("`gh` isn't on PATH"));
        assert_eq!(gh.resolved, None);

        let git = clis.check(Cli::Git, base).await;
        let problem = git.problem.unwrap();
        assert_eq!(problem, "`--version` failed: boom");
    }

    #[test]
    fn logins_read_from_each_clis_own_words() {
        assert_eq!(
            claude_login(r#"{"loggedIn": false}"#),
            Some(Login {
                logged_in: false,
                detail: None,
            })
        );
        assert_eq!(claude_login("error: unknown command 'auth'"), None);
        let out = |success, stdout: &str, stderr: &str| Output {
            success,
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        assert_eq!(
            codex_login(&out(true, "Logged in using ChatGPT\n", "")),
            Some(Login {
                logged_in: true,
                detail: Some("ChatGPT".into()),
            })
        );
        assert_eq!(
            codex_login(&out(false, "", "Not logged in\n")),
            Some(Login {
                logged_in: false,
                detail: None,
            })
        );
        assert_eq!(codex_login(&out(false, "", "error: unrecognized")), None);
    }
}
