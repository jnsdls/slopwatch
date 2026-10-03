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

use slopwatch_protocol::step::Manifest;
use slopwatch_protocol::{SecretInfo, SecretValue, is_secret_name};

use crate::approvals::Approval;
use crate::store::{Store, StoreError};
pub use keychain::{Keychain, KeychainError, MemoryKeychain, SecurityCli};
pub use mask::{MASKED, Mask, StreamMask};

/// The shortest value the daemon takes. A shorter one would mask common
/// words out of every log, and no API key is that short.
pub const MIN_VALUE_CHARS: usize = 8;

/// How a Step whose required Secret isn't set errors. A shared Inbox entry
/// per Secret holds its PR back.
const SECRET_MISSING: &str = "error(secret missing)";
/// How a Step errors when its Plugin's Approval doesn't cover a Secret
/// its manifest requires, or the manifest names one no Secret can have.
const SECRET_UNGRANTED: &str = "error(secret ungranted)";
/// How a Step errors when the Keychain wouldn't give up a Secret's value.
const SECRET_UNREADABLE: &str = "error(secret unreadable)";

/// Whether a Step's error is one a shared cause's entry explains.
pub fn held_by_cause(reason: &str) -> bool {
    reason.starts_with(SECRET_MISSING)
}

/// Whether a Step's error came from the missing Secret `name`.
pub fn missing_secret(reason: &str, name: &str) -> bool {
    reason.starts_with(SECRET_MISSING) && reason.contains(&format!("`{name}`"))
}

/// Why a Step can't have the Secrets it requires.
#[derive(Debug)]
pub struct Denied {
    /// The Step's error.
    pub reason: String,
    /// Required Secrets that aren't set, each a shared cause.
    pub unset: Vec<String>,
}

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
    /// The daemon can't take the command, such as one without Runs.
    Refused(String),
    Keychain(KeychainError),
    /// The daemon failed on its side.
    Internal(String),
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
            SecretError::Refused(why) | SecretError::Internal(why) => f.write_str(why),
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
        let Some(value) = self.keychain.get(name)? else {
            return Err(KeychainError(format!(
                "`{name}` is set, but its Keychain item is gone. Set it again."
            )));
        };
        // A set that landed while this read waited on the Keychain wins.
        Ok(Some(
            self.cache().entry(name.to_owned()).or_insert(value).clone(),
        ))
    }

    /// The Secrets a Step of `plugin` gets as env vars: those its manifest
    /// names that its Approval covers and that are set. Fails when a
    /// required one can't be had. May block on the Keychain.
    pub fn for_step(
        &self,
        plugin: &str,
        manifest: Option<&Manifest>,
    ) -> Result<Vec<(String, SecretValue)>, Denied> {
        let wanted = manifest.map_or(&[][..], |manifest| &manifest.secrets[..]);
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let approval = self
            .store
            .approvals()
            .map_err(|error| Denied {
                reason: format!("{SECRET_UNREADABLE}: can't read Approvals: {error}"),
                unset: Vec::new(),
            })?
            .into_iter()
            .find(|approval| approval.plugin == plugin);
        let (mut handed, mut unset, mut ungranted, mut unreadable) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for spec in wanted {
            let granted = is_secret_name(&spec.name)
                && approval
                    .as_ref()
                    .is_some_and(|approval| approval.grant.covers_secret(&spec.name));
            if !granted {
                if !spec.optional {
                    ungranted.push(spec.name.clone());
                }
                continue;
            }
            match self.value(&spec.name) {
                Ok(Some(value)) => handed.push((spec.name.clone(), value)),
                Ok(None) if spec.optional => {}
                Ok(None) => unset.push(spec.name.clone()),
                Err(error) if spec.optional => eprintln!("slopwatchd: {error}"),
                Err(error) => unreadable.push(error.to_string()),
            }
        }
        let names = |names: &[String]| {
            names
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !ungranted.is_empty() {
            return Err(Denied {
                reason: format!(
                    "{SECRET_UNGRANTED}: Plugin `{plugin}` has no Approval for {}",
                    names(&ungranted)
                ),
                unset: Vec::new(),
            });
        }
        if !unreadable.is_empty() {
            return Err(Denied {
                reason: format!("{SECRET_UNREADABLE}: {}", unreadable.join("; ")),
                unset: Vec::new(),
            });
        }
        if !unset.is_empty() {
            let verb = if unset.len() == 1 { "isn't" } else { "aren't" };
            return Err(Denied {
                reason: format!("{SECRET_MISSING}: {} {verb} set", names(&unset)),
                unset,
            });
        }
        Ok(handed)
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
    // `security -w` prints anything but printable ASCII back as hex.
    if !trimmed.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        return Err(SecretError::BadValue(format!(
            "The value for `{name}` holds a line break, a control character or a character \
             outside ASCII, so it isn't set"
        )));
    }
    Ok(SecretValue::new(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals::Grant;
    use slopwatch_core::Workspace;
    use slopwatch_protocol::step::SecretSpec;

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
        for value in [
            "abc12",
            "   ",
            "line1-abcdef\nline2-abcdef",
            "not-ascii-ü-value",
        ] {
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

    fn manifest(secrets: Vec<SecretSpec>) -> Manifest {
        Manifest {
            id: "jev".into(),
            version: "1".into(),
            dialect: slopwatch_protocol::step::STEP_DIALECT,
            features: vec![],
            config_schema: serde_json::Value::Null,
            workspace: Workspace::None,
            effects: vec![],
            secrets,
            timeout: None,
            stall_after: None,
            concurrency: None,
        }
    }

    #[test]
    fn a_step_gets_the_set_secrets_its_approval_covers_and_no_others() {
        let (secrets, _) = secrets();
        secrets
            .store
            .put_approval(&approval("jev", &["KEY", "EXTRA"]))
            .unwrap();
        secrets.set("KEY", SecretValue::new(VALUE), 1).unwrap();
        secrets.set("OTHER", SecretValue::new(VALUE), 1).unwrap();
        let wants = manifest(vec![
            SecretSpec::required("KEY"),
            SecretSpec::optional("EXTRA"),
            SecretSpec::optional("OTHER"),
        ]);

        let handed = secrets.for_step("jev", Some(&wants)).unwrap();
        assert_eq!(handed, vec![("KEY".to_owned(), SecretValue::new(VALUE))]);

        let denied = secrets
            .for_step(
                "jev",
                Some(&manifest(vec![SecretSpec::required("MISSING")])),
            )
            .unwrap_err();
        assert_eq!(
            denied.reason,
            "error(secret ungranted): Plugin `jev` has no Approval for `MISSING`"
        );
        assert!(denied.unset.is_empty());

        secrets
            .store
            .put_approval(&approval("jev", &["A", "B", "path"]))
            .unwrap();
        let denied = secrets
            .for_step(
                "jev",
                Some(&manifest(vec![
                    SecretSpec::required("A"),
                    SecretSpec::required("B"),
                ])),
            )
            .unwrap_err();
        assert_eq!(denied.reason, "error(secret missing): `A`, `B` aren't set");
        assert_eq!(denied.unset, ["A", "B"]);
        assert!(held_by_cause(&denied.reason));
        assert!(missing_secret(&denied.reason, "B"));
        assert!(!missing_secret(&denied.reason, "C"));

        let denied = secrets
            .for_step("jev", Some(&manifest(vec![SecretSpec::required("path")])))
            .unwrap_err();
        assert!(
            denied.reason.starts_with("error(secret ungranted)"),
            "a name no Secret can have"
        );
    }

    #[test]
    fn a_set_secret_whose_keychain_item_is_gone_is_an_error_not_a_missing_secret() {
        let keychain = Arc::new(MemoryKeychain::default());
        let store = Store::in_memory();
        Secrets::new(Arc::clone(&keychain) as Arc<dyn Keychain>, store.clone())
            .set("KEY", SecretValue::new(VALUE), 1)
            .unwrap();
        keychain.delete("KEY").unwrap();
        store.put_approval(&approval("jev", &["KEY"])).unwrap();
        let restarted = Secrets::new(Arc::clone(&keychain) as Arc<dyn Keychain>, store);

        let error = restarted.value("KEY").unwrap_err();
        assert!(error.to_string().contains("Set it again"), "{error}");
        let denied = restarted
            .for_step("jev", Some(&manifest(vec![SecretSpec::required("KEY")])))
            .unwrap_err();
        assert!(
            denied.reason.starts_with("error(secret unreadable)"),
            "{}",
            denied.reason
        );
        assert!(
            denied.unset.is_empty(),
            "no shared entry that setting can't clear"
        );
    }
}
