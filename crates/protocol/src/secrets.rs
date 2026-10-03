//! Secrets on the wire. A client sets a Secret with
//! [`Command::SetSecret`](crate::Command::SetSecret) and never gets a value
//! back: [`SecretInfo`] carries names and dates only.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A Secret's value, on its one way in. Its `Debug` prints no value, so a
/// command logged by accident leaks nothing.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value itself, for the Keychain and a Step's env only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(***)")
    }
}

/// One Secret as the Secrets list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretInfo {
    /// The env var name a manifest asks for.
    pub name: String,
    /// When it was last set, in seconds since the Unix epoch. `None` while
    /// it isn't set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_at: Option<i64>,
    /// The Plugins whose Approval covers it, sorted.
    #[serde(default)]
    pub granted_to: Vec<String>,
    /// Every Plugin granted it runs without it, as a review Step on a
    /// subscription login does. An unset optional Secret isn't missing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl SecretInfo {
    pub fn is_set(&self) -> bool {
        self.set_at.is_some()
    }
}

/// Whether `name` can name a Secret: an env var name in upper case,
/// letters, digits and `_`, not starting with a digit, and not one the
/// daemon sets for every Step (`PATH`, `HOME`, `USER`, `LOGNAME`, `SLOPWATCH_*`).
pub fn is_secret_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let shaped = (first.is_ascii_uppercase() || first == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    shaped
        && !matches!(name, "PATH" | "HOME" | "USER" | "LOGNAME")
        && !name.starts_with("SLOPWATCH_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_never_shows_in_debug_output() {
        let value = SecretValue::new("sk-very-secret");
        assert_eq!(format!("{value:?}"), "SecretValue(***)");
        assert_eq!(serde_json::to_string(&value).unwrap(), "\"sk-very-secret\"");
    }

    #[test]
    fn secret_names_are_env_var_names_the_daemon_doesnt_own() {
        for good in ["ANTHROPIC_API_KEY", "_X", "JEV_KEY2"] {
            assert!(is_secret_name(good), "{good}");
        }
        for bad in [
            "",
            "lower",
            "2KEY",
            "WITH-DASH",
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "SLOPWATCH_RUN",
            "A B",
        ] {
            assert!(!is_secret_name(bad), "{bad}");
        }
    }
}
