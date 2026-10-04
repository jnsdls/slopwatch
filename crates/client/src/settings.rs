//! What the daemon settings screen shows: the daily Budget and what Steps
//! spent today, and the CLIs the daemon runs, with the commands that save
//! them. No GPUI here, so it tests without a window.

use slopwatch_protocol::{Cents, Cli, CliListing, CliSettings, CliStatus, Command, DaemonSettings};

#[derive(Debug, Default)]
pub struct SettingsModel {
    /// `None` until the daemon answers.
    settings: Option<DaemonSettings>,
    spent_today: Cents,
    /// Empty until the daemon answers.
    clis: Vec<CliListing>,
}

impl SettingsModel {
    pub fn listed(&mut self, settings: DaemonSettings, spent_today: Cents) {
        self.settings = Some(settings);
        self.spent_today = spent_today;
    }

    pub fn clis_listed(&mut self, clis: Vec<CliListing>) {
        self.clis = clis;
    }

    pub fn cli(&self, cli: Cli) -> Option<&CliListing> {
        self.clis.iter().find(|listing| listing.cli == cli)
    }

    pub fn loaded(&self) -> bool {
        self.settings.is_some()
    }

    /// The daily Budget as the field shows it: dollars, blank when off.
    pub fn daily_field(&self) -> String {
        match self.settings.and_then(|settings| settings.daily_budget) {
            Some(daily) => daily.to_string().trim_start_matches('$').to_owned(),
            None => String::new(),
        }
    }

    /// What Steps spent today, against the daily Budget if it's on.
    pub fn spent_line(&self) -> String {
        match self.settings.and_then(|settings| settings.daily_budget) {
            Some(daily) => format!("Spent today: {} of {daily}", self.spent_today),
            None => format!(
                "Spent today: {}. The daily Budget is off.",
                self.spent_today
            ),
        }
    }
}

/// The command that saves `field`, the daily Budget in dollars. Blank
/// turns it off.
pub fn save_daily(field: &str) -> Result<Command, String> {
    let field = field.trim().trim_start_matches('$');
    let daily_budget = if field.is_empty() {
        None
    } else {
        let usd: f64 = field
            .parse()
            .ok()
            .filter(|usd: &f64| usd.is_finite() && *usd > 0.0)
            .ok_or_else(|| {
                format!("Write the daily Budget in dollars, such as 25, or leave it blank to turn it off. \"{field}\" isn't one.")
            })?;
        Some(Cents::from_usd(usd))
    };
    Ok(Command::SetSettings {
        settings: DaemonSettings { daily_budget },
    })
}

/// A CLI's settings as its fields hold them: the executable, blank for
/// its usual name, `PATH` dirs separated by `:`, and a config directory
/// that's blank for none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliFields {
    pub executable: String,
    pub path: String,
    pub config_dir: String,
}

/// The settings as the fields show them.
pub fn cli_fields(settings: &CliSettings) -> CliFields {
    CliFields {
        executable: settings.executable.clone().unwrap_or_default(),
        path: settings.path.join(":"),
        config_dir: settings.config_dir.clone().unwrap_or_default(),
    }
}

/// The command that saves `cli`'s settings from what the fields hold.
/// `Err` says what's wrong with them.
pub fn save_cli(cli: Cli, fields: &CliFields) -> Result<Command, String> {
    let text = |field: &str| Some(field.trim().to_owned()).filter(|text| !text.is_empty());
    let settings = CliSettings {
        executable: text(&fields.executable),
        path: fields
            .path
            .split(':')
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
            .map(str::to_owned)
            .collect(),
        config_dir: text(&fields.config_dir),
    };
    if let Some(problem) = settings.problem(cli) {
        return Err(problem);
    }
    Ok(Command::SetCliSettings { cli, settings })
}

/// What the daemon found when it tried the CLI, as its row reads: the file
/// and the version, or why it can't run it.
pub fn status_line(status: &CliStatus) -> Result<String, String> {
    if let Some(problem) = &status.problem {
        return Err(match &status.resolved {
            Some(file) => format!("{file}: {problem}"),
            None => problem.clone(),
        });
    }
    let file = status.resolved.as_deref().unwrap_or("?");
    Ok(match &status.version {
        Some(version) => format!("{file} · {version}"),
        None => file.to_owned(),
    })
}

/// What the CLI says about its login, if it says, and whether that's
/// logged in.
pub fn login_line(status: &CliStatus) -> Option<(String, bool)> {
    let login = status.login.as_ref()?;
    Some(match (&login.detail, login.logged_in) {
        (Some(detail), true) => (format!("Logged in with {detail}"), true),
        (None, true) => ("Logged in".to_owned(), true),
        (_, false) => ("Not logged in".to_owned(), false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::Login;

    #[test]
    fn the_field_shows_dollars_and_blank_turns_the_budget_off() {
        let mut model = SettingsModel::default();
        assert!(!model.loaded());
        model.listed(DaemonSettings::default(), Cents(120));

        assert_eq!(model.daily_field(), "25");
        assert_eq!(model.spent_line(), "Spent today: $1.20 of $25");
        assert_eq!(
            save_daily(" $12.50 "),
            Ok(Command::SetSettings {
                settings: DaemonSettings {
                    daily_budget: Some(Cents(1250)),
                },
            })
        );
        assert_eq!(
            save_daily(""),
            Ok(Command::SetSettings {
                settings: DaemonSettings { daily_budget: None },
            })
        );
        assert!(save_daily("0").is_err());
        assert!(save_daily("lots").is_err());

        model.listed(DaemonSettings { daily_budget: None }, Cents(0));
        assert_eq!(model.daily_field(), "");
        assert_eq!(
            model.spent_line(),
            "Spent today: $0. The daily Budget is off."
        );
    }

    #[test]
    fn cli_fields_save_trimmed_and_blank_means_the_usual_name() {
        let fields = |executable: &str, path: &str, config_dir: &str| CliFields {
            executable: executable.into(),
            path: path.into(),
            config_dir: config_dir.into(),
        };
        assert_eq!(
            save_cli(
                Cli::Claude,
                &fields(" mclaude ", " /opt/a : /opt/b ", " /Users/me/.claude-work ")
            ),
            Ok(Command::SetCliSettings {
                cli: Cli::Claude,
                settings: CliSettings {
                    executable: Some("mclaude".into()),
                    path: vec!["/opt/a".into(), "/opt/b".into()],
                    config_dir: Some("/Users/me/.claude-work".into()),
                },
            })
        );
        assert_eq!(
            save_cli(Cli::Gh, &fields("", "", "")),
            Ok(Command::SetCliSettings {
                cli: Cli::Gh,
                settings: CliSettings::default(),
            })
        );
        assert!(save_cli(Cli::Gh, &fields("bin/gh", "", "")).is_err());
        assert!(save_cli(Cli::Codex, &fields("", "", "~/.codex")).is_err());
        let settings = CliSettings {
            executable: Some("/opt/homebrew/bin/gh".into()),
            path: vec!["/a".into(), "/b".into()],
            config_dir: Some("/c".into()),
        };
        assert_eq!(
            cli_fields(&settings),
            fields("/opt/homebrew/bin/gh", "/a:/b", "/c")
        );
    }

    #[test]
    fn a_cli_row_reads_its_file_and_version_or_why_it_cant_run() {
        let found = CliStatus {
            resolved: Some("/Users/me/.local/bin/mclaude".into()),
            version: Some("2.1.0 (Claude Code)".into()),
            login: Some(Login {
                logged_in: true,
                detail: Some("claude.ai (max)".into()),
            }),
            problem: None,
        };
        assert_eq!(
            status_line(&found),
            Ok("/Users/me/.local/bin/mclaude · 2.1.0 (Claude Code)".into())
        );
        assert_eq!(
            login_line(&found),
            Some(("Logged in with claude.ai (max)".into(), true))
        );
        let missing = CliStatus {
            problem: Some("`mclaude` isn't on PATH".into()),
            ..CliStatus::default()
        };
        assert_eq!(status_line(&missing), Err("`mclaude` isn't on PATH".into()));
        assert_eq!(login_line(&missing), None);
        let failing = CliStatus {
            resolved: Some("/bin/gh".into()),
            problem: Some("`--version` failed: boom".into()),
            ..CliStatus::default()
        };
        assert_eq!(
            status_line(&failing),
            Err("/bin/gh: `--version` failed: boom".into())
        );
        let out = CliStatus {
            login: Some(Login {
                logged_in: false,
                detail: None,
            }),
            ..found
        };
        assert_eq!(login_line(&out), Some(("Not logged in".into(), false)));
    }
}
