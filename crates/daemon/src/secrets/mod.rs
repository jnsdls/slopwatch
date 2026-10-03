//! Secrets: named credentials the daemon keeps for Steps and never shows
//! back.
//!
//! Values live in the Keychain ([`keychain`]), and the store records only
//! which names are set and when. A client sets a value with a write-only
//! command; nothing the daemon sends a client carries one. A Step gets a
//! Secret as an env var when its Plugin's Approval covers it, and the
//! daemon masks every value a Step got out of whatever that Step writes
//! ([`mask`]).
//!
//! Values are cached in memory once read or set, so a Step spawn doesn't
//! wait on the Keychain. Rotating goes through [`Secrets::set`], which
//! replaces the cached value too, so the next spawn gets the new one.

pub mod keychain;
pub mod mask;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use slopwatch_protocol::{SecretInfo, SecretValue, is_secret_name};

use crate::approvals::Approval;
use crate::store::{Store, StoreError};
pub use keychain::{Keychain, KeychainError, MemoryKeychain, SecurityCli};
pub use mask::{MASKED, Mask, StreamMask};

/// The shortest value the daemon takes. A shorter one would mask common
/// words out of every log, and no API key is that short.
pub const MIN_VALUE_CHARS: usize = 8;

pub struct Secrets {
    keychain: Arc<dyn Keychain>,
    store: Store,
    cache: Mutex<HashMap<String, SecretValue>>,
}

/// Why a Secret command was refused.
#[derive(Debug)]
pub enum SecretError {
    BadName(String),
    /// The value can't be a Secret. The message never quotes it.
    BadValue(String),
    NotFound(String),
    Keychain(KeychainError),
    Store(StoreError),
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretError::BadName(name) => write!(
                f,
                "`{name}` can't name a Secret; use an env var name in upper case, such as \
                 `JEV_API_KEY`, that isn't `PATH`, `HOME` or `SLOPWATCH_*`"
            ),
            SecretError::BadValue(why) => f.write_str(why),
            SecretError::NotFound(name) => write!(f, "No Secret `{name}` is set"),
            SecretError::Keychain(error) => error.fmt(f),
            SecretError::Store(error) => write!(f, "Database error: {error}"),
        }
    }
}

impl From<StoreError> for SecretError {
    fn from(error: StoreError) -> Self {
        SecretError::Store(error)
    }
}

impl Secrets {
    pub fn new(keychain: Arc<dyn Keychain>, store: Store) -> Self {
        Self {
            keychain,
            store,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<String, SecretValue>> {
        self.cache
            .lock()
            .expect("no panics while holding the Secret cache")
    }

    /// Every Secret that's set or that an Approval covers, by name.
    pub fn list(&self, approvals: &[Approval]) -> Result<Vec<SecretInfo>, StoreError> {
        let mut secrets: BTreeMap<String, SecretInfo> = self
            .store
            .secrets()?
            .into_iter()
            .map(|(name, set_at)| {
                let info = SecretInfo {
                    name: name.clone(),
                    set_at: Some(set_at),
                    granted_to: Vec::new(),
                };
                (name, info)
            })
            .collect();
        for approval in approvals {
            for name in &approval.grant.secrets {
                let info = secrets.entry(name.clone()).or_insert_with(|| SecretInfo {
                    name: name.clone(),
                    set_at: None,
                    granted_to: Vec::new(),
                });
                if !info.granted_to.contains(&approval.plugin) {
                    info.granted_to.push(approval.plugin.clone());
                }
            }
        }
        Ok(secrets
            .into_values()
            .map(|mut info| {
                info.granted_to.sort();
                info
            })
            .collect())
    }

    /// Sets or rotates `name`. Surrounding whitespace, as a paste often
    /// brings, is dropped. Blocks on the Keychain.
    pub fn set(&self, name: &str, value: SecretValue, now: i64) -> Result<(), SecretError> {
        if !is_secret_name(name) {
            return Err(SecretError::BadName(name.to_owned()));
        }
        let value = check_value(name, value)?;
        self.keychain
            .set(name, &value)
            .map_err(SecretError::Keychain)?;
        self.cache().insert(name.to_owned(), value);
        self.store.put_secret(name, now)?;
        Ok(())
    }

    /// Removes `name`. Blocks on the Keychain.
    pub fn delete(&self, name: &str) -> Result<(), SecretError> {
        if self.store.secret_set_at(name)?.is_none() {
            return Err(SecretError::NotFound(name.to_owned()));
        }
        self.keychain.delete(name).map_err(SecretError::Keychain)?;
        self.cache().remove(name);
        self.store.remove_secret(name)?;
        Ok(())
    }

    pub fn is_set(&self, name: &str) -> Result<bool, StoreError> {
        Ok(self.store.secret_set_at(name)?.is_some())
    }

    /// The value of `name`, if it's set. A value not cached yet is read
    /// from the Keychain, which blocks.
    pub fn value(&self, name: &str) -> Result<Option<SecretValue>, KeychainError> {
        if let Some(value) = self.cache().get(name) {
            return Ok(Some(value.clone()));
        }
        let set = self
            .store
            .secret_set_at(name)
            .map_err(|error| KeychainError(format!("Database error: {error}")))?;
        if set.is_none() {
            return Ok(None);
        }
        let value = self.keychain.get(name)?;
        if let Some(value) = &value {
            self.cache().insert(name.to_owned(), value.clone());
        }
        Ok(value)
    }

    /// Reads every set value into the cache, so the first Steps after a
    /// start don't wait on the Keychain. Blocks.
    pub fn warm(&self) {
        let Ok(names) = self.store.secrets() else {
            return;
        };
        for (name, _) in names {
            if let Err(error) = self.value(&name) {
                eprintln!("slopwatchd: {error}");
            }
        }
    }
}

fn check_value(name: &str, value: SecretValue) -> Result<SecretValue, SecretError> {
    let trimmed = value.expose().trim();
    if trimmed.chars().count() < MIN_VALUE_CHARS {
        return Err(SecretError::BadValue(format!(
            "The value for `{name}` is shorter than {MIN_VALUE_CHARS} characters, so it isn't \
             set"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(SecretError::BadValue(format!(
            "The value for `{name}` holds a line break or another control character, so it \
             isn't set"
        )));
    }
    Ok(SecretValue::new(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::Grant;
    use slopwatch_core::Workspace;

    const VALUE: &str = "sk-live-0123456789";

    fn secrets() -> (Secrets, Arc<MemoryKeychain>) {
        let keychain = Arc::new(MemoryKeychain::default());
        let secrets = Secrets::new(
            Arc::clone(&keychain) as Arc<dyn Keychain>,
            Store::in_memory(),
        );
        (secrets, keychain)
    }

    fn approval(plugin: &str, secrets: &[&str]) -> Approval {
        Approval {
            plugin: plugin.into(),
            grant: Grant {
                workspace: Workspace::None,
                effects: vec![],
                secrets: secrets.iter().map(|&s| s.to_owned()).collect(),
            },
            actor: None,
            approved_at: 0,
        }
    }

    #[test]
    fn the_list_shows_names_dates_and_grants_never_values() {
        let (secrets, _) = secrets();
        secrets
            .set("JEV_API_KEY", SecretValue::new(VALUE), 100)
            .unwrap();
        secrets.set("UNUSED", SecretValue::new(VALUE), 50).unwrap();

        let listed = secrets
            .list(&[
                approval("jev", &["JEV_API_KEY"]),
                approval("claude", &["ANTHROPIC_API_KEY"]),
                approval("other", &["JEV_API_KEY"]),
            ])
            .unwrap();

        assert_eq!(
            listed,
            vec![
                SecretInfo {
                    name: "ANTHROPIC_API_KEY".into(),
                    set_at: None,
                    granted_to: vec!["claude".into()],
                },
                SecretInfo {
                    name: "JEV_API_KEY".into(),
                    set_at: Some(100),
                    granted_to: vec!["jev".into(), "other".into()],
                },
                SecretInfo {
                    name: "UNUSED".into(),
                    set_at: Some(50),
                    granted_to: vec![],
                },
            ]
        );
        assert!(!serde_json::to_string(&listed).unwrap().contains(VALUE));
    }

    #[test]
    fn a_set_value_is_trimmed_cached_and_rotated() {
        let (secrets, keychain) = secrets();
        secrets
            .set("KEY", SecretValue::new(format!("  {VALUE}\n")), 1)
            .unwrap();

        assert_eq!(secrets.value("KEY").unwrap(), Some(SecretValue::new(VALUE)));
        assert_eq!(keychain.get("KEY").unwrap(), Some(SecretValue::new(VALUE)));
        // Served from the cache.
        assert_eq!(keychain.reads("KEY"), 1);

        secrets
            .set("KEY", SecretValue::new("rotated-value"), 2)
            .unwrap();
        assert_eq!(
            secrets.value("KEY").unwrap(),
            Some(SecretValue::new("rotated-value"))
        );
    }

    #[test]
    fn a_new_daemon_reads_a_set_value_from_the_keychain_once() {
        let keychain = Arc::new(MemoryKeychain::default());
        let store = Store::in_memory();
        Secrets::new(Arc::clone(&keychain) as Arc<dyn Keychain>, store.clone())
            .set("KEY", SecretValue::new(VALUE), 1)
            .unwrap();
        let reads = keychain.reads("KEY");

        let restarted = Secrets::new(Arc::clone(&keychain) as Arc<dyn Keychain>, store);
        restarted.warm();
        assert_eq!(
            restarted.value("KEY").unwrap(),
            Some(SecretValue::new(VALUE))
        );
        assert_eq!(keychain.reads("KEY"), reads + 1);
        // An unset name never reaches the Keychain.
        assert_eq!(restarted.value("OTHER").unwrap(), None);
        assert_eq!(keychain.reads("OTHER"), 0);
    }

    #[test]
    fn bad_names_and_values_are_refused_without_quoting_the_value() {
        let (secrets, keychain) = secrets();

        assert!(matches!(
            secrets.set("lower", SecretValue::new(VALUE), 1),
            Err(SecretError::BadName(_))
        ));
        for value in ["abc12", "   ", "line1-abcdef\nline2-abcdef"] {
            let error = secrets.set("KEY", SecretValue::new(value), 1).unwrap_err();
            assert!(matches!(error, SecretError::BadValue(_)));
            if !value.trim().is_empty() {
                assert!(!error.to_string().contains(value.trim()));
            }
        }
        assert_eq!(keychain.get("KEY").unwrap(), None);
        assert!(!secrets.is_set("KEY").unwrap());
    }

    #[test]
    fn deleting_forgets_the_value_everywhere() {
        let (secrets, keychain) = secrets();
        secrets.set("KEY", SecretValue::new(VALUE), 1).unwrap();

        secrets.delete("KEY").unwrap();

        assert_eq!(secrets.value("KEY").unwrap(), None);
        assert_eq!(keychain.get("KEY").unwrap(), None);
        assert!(matches!(
            secrets.delete("KEY"),
            Err(SecretError::NotFound(_))
        ));
    }
}
