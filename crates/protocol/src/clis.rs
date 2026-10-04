//! The CLIs the daemon runs, and the developer's settings for each: which
//! executable, and for the agent CLIs, extra `PATH` dirs and a config
//! directory. They live in the daemon, never in a repo, since a path that
//! exists on one machine means nothing on another.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A CLI the daemon depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cli {
    /// Claude Code, for the `claude` Plugin and `fix` with `agent: claude`.
    Claude,
    /// Codex, for the `codex` Plugin and `fix` with `agent: codex`.
    Codex,
    /// GitHub's CLI, whose login is the daemon's GitHub credential.
    Gh,
    /// Git, for the daemon's clones and worktrees.
    Git,
}

impl Cli {
    /// Every CLI, in the order the Settings screen lists them.
    pub const ALL: [Cli; 4] = [Cli::Claude, Cli::Codex, Cli::Gh, Cli::Git];

    /// The CLI's usual name, which is also its executable until the
    /// developer sets another.
    pub fn name(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
            Cli::Gh => "gh",
            Cli::Git => "git",
        }
    }

    /// An agent CLI a Step runs. Only these take `PATH` dirs and a config
    /// directory.
    pub fn is_agent(self) -> bool {
        matches!(self, Cli::Claude | Cli::Codex)
    }
}

impl fmt::Display for Cli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The developer's settings for one CLI in the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliSettings {
    /// A name on `PATH` or an absolute path. `None` runs the CLI's usual
    /// name, found on `PATH` or where Homebrew puts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Absolute directories put in front of `PATH` for the CLI, and for
    /// the Steps that run it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    /// An absolute config directory the CLI logs in from on a
    /// subscription: `CLAUDE_CONFIG_DIR` for Claude, `CODEX_HOME` for
    /// Codex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

impl CliSettings {
    /// Why these settings for `cli` can't be kept, if they can't.
    pub fn problem(&self, cli: Cli) -> Option<String> {
        if let Some(executable) = &self.executable {
            if executable.trim().is_empty() {
                return Some("The executable can't be blank".to_owned());
            }
            if executable.contains('/') && !executable.starts_with('/') {
                return Some(format!(
                    "`{executable}` isn't a name on PATH or an absolute path"
                ));
            }
        }
        if !cli.is_agent() && (!self.path.is_empty() || self.config_dir.is_some()) {
            return Some(format!("{cli} takes no PATH dirs or config directory"));
        }
        if let Some(dir) = self.path.iter().find(|dir| !dir.starts_with('/')) {
            return Some(format!("`{dir}` isn't an absolute path"));
        }
        if let Some(dir) = self.path.iter().find(|dir| dir.contains(':')) {
            return Some(format!("`{dir}` holds a `:`, which `PATH` can't carry"));
        }
        if let Some(dir) = self
            .config_dir
            .as_deref()
            .filter(|dir| !dir.starts_with('/'))
        {
            return Some(format!(
                "The config directory `{dir}` isn't an absolute path"
            ));
        }
        None
    }
}

/// What the daemon found when it tried a CLI's executable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliStatus {
    /// The file the executable resolved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
    /// The first line of what `--version` printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Whether the CLI says it's logged in, for the agent CLIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<Login>,
    /// Why the daemon can't run it, such as no such file on `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// What an agent CLI says about its login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    pub logged_in: bool,
    /// How, in the CLI's words, such as `claude.ai (max)` or `ChatGPT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One CLI as the Settings screen shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliListing {
    pub cli: Cli,
    pub settings: CliSettings,
    pub status: CliStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_take_a_name_or_an_absolute_path_and_dirs_only_for_agents() {
        let ok = CliSettings {
            executable: Some("mclaude".into()),
            path: vec!["/opt/tools/bin".into()],
            config_dir: Some("/Users/me/.claude-work".into()),
        };
        assert_eq!(ok.problem(Cli::Claude), None);
        let absolute = CliSettings {
            executable: Some("/opt/homebrew/bin/gh".into()),
            ..CliSettings::default()
        };
        assert_eq!(absolute.problem(Cli::Gh), None);

        let bad = [
            (Cli::Claude, Some("bin/claude"), vec![], None),
            (Cli::Claude, Some(" "), vec![], None),
            (Cli::Claude, None, vec!["bin"], None),
            (Cli::Codex, None, vec!["/a:/b"], None),
            (Cli::Codex, None, vec![], Some("~/.codex")),
            (Cli::Gh, None, vec!["/opt/bin"], None),
            (Cli::Git, None, vec![], Some("/tmp")),
        ];
        for (cli, executable, path, config_dir) in bad {
            let settings = CliSettings {
                executable: executable.map(str::to_owned),
                path: path.into_iter().map(str::to_owned).collect(),
                config_dir: config_dir.map(str::to_owned),
            };
            assert!(settings.problem(cli).is_some(), "{cli}: {settings:?}");
        }
    }
}
