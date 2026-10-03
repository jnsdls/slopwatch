//! What the Secrets list shows: each Secret's name, whether it's set and
//! when, and which Plugins it's granted to. The daemon never sends a value,
//! so there is none here to show. No GPUI here, so it tests without a
//! window.

use slopwatch_protocol::{Command, InboxEntry, Scope, SecretInfo, SecretValue, is_secret_name};

#[derive(Debug, Default)]
pub struct SecretsList {
    secrets: Vec<SecretInfo>,
    loaded: bool,
    /// The Secret the value field sets, picked from the list or typed in.
    chosen: Option<String>,
}

impl SecretsList {
    pub fn listed(&mut self, secrets: Vec<SecretInfo>) {
        self.secrets = secrets;
        self.loaded = true;
    }

    pub fn secrets(&self) -> &[SecretInfo] {
        &self.secrets
    }

    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// How many Secrets a Plugin is granted that aren't set.
    pub fn unset(&self) -> usize {
        self.secrets.iter().filter(|secret| needed(secret)).count()
    }

    pub fn choose(&mut self, name: &str) {
        self.chosen = Some(name.to_owned());
    }

    pub fn chosen(&self) -> Option<&str> {
        self.chosen.as_deref()
    }

    /// The command that sets the chosen Secret, or `name` when one is
    /// typed, to `value`. `None` when there's no valid name or no value.
    pub fn set(&self, name: &str, value: &str) -> Option<Command> {
        let name = Some(name.trim())
            .filter(|name| !name.is_empty())
            .or(self.chosen.as_deref())?;
        if !is_secret_name(name) || value.trim().is_empty() {
            return None;
        }
        Some(Command::SetSecret {
            secret: name.to_owned(),
            value: SecretValue::new(value),
        })
    }
}

/// Whether a Plugin is granted `secret`, needs it, and it isn't set, so
/// Steps that require it error.
pub fn needed(secret: &SecretInfo) -> bool {
    !secret.is_set() && !secret.granted_to.is_empty() && !secret.optional
}

/// A Secret's state, as its row reads: when it was set, or that it isn't.
/// `now` is seconds since the Unix epoch.
pub fn state_line(secret: &SecretInfo, now: i64) -> String {
    let Some(at) = secret.set_at else {
        return "Not set".to_owned();
    };
    let ago = (now - at).max(0);
    let when = match ago {
        0..60 => "just now".to_owned(),
        60..3600 => plural(ago / 60, "minute"),
        3600..86_400 => plural(ago / 3600, "hour"),
        _ => plural(ago / 86_400, "day"),
    };
    format!("Set {when}")
}

fn plural(n: i64, unit: &str) -> String {
    match n {
        1 => format!("1 {unit} ago"),
        _ => format!("{n} {unit}s ago"),
    }
}

/// Which Plugins a Secret goes to, as one line.
pub fn granted_line(secret: &SecretInfo) -> String {
    match secret.granted_to.as_slice() {
        [] => "No Plugin is granted it".to_owned(),
        plugins => format!("Granted to {}", plugins.join(", ")),
    }
}

/// The Secret an Inbox entry asks the developer to set, if it does.
pub fn missing_secret(entry: &InboxEntry) -> Option<&str> {
    match &entry.scope {
        Scope::Cause {
            cause: slopwatch_protocol::Cause::MissingSecret { name },
        } => Some(name),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::{Cause, EntryId};

    fn info(name: &str, set_at: Option<i64>, granted: &[&str]) -> SecretInfo {
        SecretInfo {
            name: name.into(),
            set_at,
            granted_to: granted.iter().map(|&p| p.to_owned()).collect(),
            optional: false,
        }
    }

    #[test]
    fn a_row_reads_when_it_was_set_or_that_it_isnt() {
        assert_eq!(state_line(&info("A", None, &[]), 100), "Not set");
        assert_eq!(state_line(&info("A", Some(100), &[]), 130), "Set just now");
        assert_eq!(state_line(&info("A", Some(0), &[]), 60), "Set 1 minute ago");
        assert_eq!(
            state_line(&info("A", Some(0), &[]), 7200),
            "Set 2 hours ago"
        );
        assert_eq!(
            state_line(&info("A", Some(0), &[]), 3 * 86_400),
            "Set 3 days ago"
        );
        assert_eq!(
            granted_line(&info("A", None, &["claude", "jev"])),
            "Granted to claude, jev"
        );
        assert_eq!(
            granted_line(&info("A", None, &[])),
            "No Plugin is granted it"
        );
    }

    #[test]
    fn unset_counts_only_secrets_a_plugin_needs() {
        let mut list = SecretsList::default();
        list.listed(vec![
            info("A", None, &["jev"]),
            info("B", Some(1), &["jev"]),
            info("C", None, &[]),
            SecretInfo {
                optional: true,
                ..info("ANTHROPIC_API_KEY", None, &["claude"])
            },
        ]);
        assert_eq!(list.unset(), 1, "an optional Secret isn't missing");
    }

    #[test]
    fn setting_takes_a_typed_name_or_the_chosen_one_and_needs_a_value() {
        let mut list = SecretsList::default();
        assert_eq!(list.set("", "value-123"), None, "no name");
        list.choose("JEV_API_KEY");

        assert_eq!(
            list.set("", "value-123"),
            Some(Command::SetSecret {
                secret: "JEV_API_KEY".into(),
                value: SecretValue::new("value-123"),
            })
        );
        assert_eq!(
            list.set(" OTHER ", "value-123"),
            Some(Command::SetSecret {
                secret: "OTHER".into(),
                value: SecretValue::new("value-123"),
            })
        );
        assert_eq!(list.set("lower", "value-123"), None, "not an env var name");
        assert_eq!(list.set("", "   "), None, "no value");
    }

    #[test]
    fn a_missing_secret_entry_names_the_secret() {
        let entry = InboxEntry {
            id: EntryId(1),
            scope: Scope::Cause {
                cause: Cause::MissingSecret {
                    name: "JEV_API_KEY".into(),
                },
            },
            title: "Secret `JEV_API_KEY` isn't set".into(),
            reasons: vec![],
            prs: vec![],
            raised_at: 0,
            closed: None,
        };
        assert_eq!(missing_secret(&entry), Some("JEV_API_KEY"));
        let pr = InboxEntry {
            scope: Scope::Pr,
            ..entry
        };
        assert_eq!(missing_secret(&pr), None);
    }
}
