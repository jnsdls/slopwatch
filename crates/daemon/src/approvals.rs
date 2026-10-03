//! Approvals: what a Plugin may have, by the Plugin's name (ADR 0012).
//!
//! An Approval covers the workspace, the Effects and the Secrets the
//! Plugin's manifest asked for when it was approved. It is the Secret
//! grant: a Step gets a Secret only when its Plugin's Approval names it.
//! Built-in Plugins ship approved through the same record, written again
//! from their manifests each time the daemon starts. Approving a
//! third-party Plugin comes with its own ticket.

use serde::{Deserialize, Serialize};
use slopwatch_core::Workspace;
use slopwatch_protocol::Actor;
use slopwatch_protocol::step::{EffectKind, Manifest};

/// What an Approval lets a Plugin have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub workspace: Workspace,
    #[serde(default)]
    pub effects: Vec<EffectKind>,
    /// Secret names.
    #[serde(default)]
    pub secrets: Vec<String>,
}

impl Grant {
    /// Everything `manifest` asks for.
    pub fn of(manifest: &Manifest) -> Self {
        Self {
            workspace: manifest.workspace,
            effects: manifest.effects.clone(),
            secrets: manifest
                .secrets
                .iter()
                .map(|spec| spec.name.clone())
                .collect(),
        }
    }

    pub fn covers_secret(&self, name: &str) -> bool {
        self.secrets.iter().any(|granted| granted == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub plugin: String,
    pub grant: Grant,
    /// Who approved it. `None` for a built-in Plugin, which ships approved.
    pub actor: Option<Actor>,
    /// Seconds since the Unix epoch.
    pub approved_at: i64,
}

impl Approval {
    /// The Approval a built-in Plugin ships with: everything its manifest
    /// asks for.
    pub fn builtin(manifest: &Manifest, now: i64) -> Self {
        Self {
            plugin: manifest.id.clone(),
            grant: Grant::of(manifest),
            actor: None,
            approved_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use slopwatch_protocol::step::{STEP_DIALECT, SecretSpec};

    fn manifest() -> Manifest {
        Manifest {
            id: "jev".into(),
            version: "1".into(),
            dialect: STEP_DIALECT,
            features: vec![],
            config_schema: serde_json::Value::Null,
            workspace: Workspace::None,
            effects: vec![EffectKind::Comment],
            secrets: vec![
                SecretSpec::required("JEV_API_KEY"),
                SecretSpec::optional("EXTRA"),
            ],
            timeout: None,
            stall_after: None,
            concurrency: None,
        }
    }

    #[test]
    fn a_builtin_approval_covers_every_secret_its_manifest_names() {
        let approval = Approval::builtin(&manifest(), 10);

        assert!(approval.grant.covers_secret("JEV_API_KEY"));
        assert!(approval.grant.covers_secret("EXTRA"));
        assert!(!approval.grant.covers_secret("OTHER"));
        assert_eq!(approval.actor, None);
    }

    #[test]
    fn approvals_round_trip_through_the_store_and_a_new_one_replaces_the_old() {
        let store = Store::in_memory();
        store
            .put_approval(&Approval::builtin(&manifest(), 10))
            .unwrap();
        let mut changed = Approval::builtin(&manifest(), 20);
        changed.grant.secrets = vec!["JEV_API_KEY".into()];
        changed.actor = Some(Actor::Developer { via: "gui".into() });
        store.put_approval(&changed).unwrap();

        assert_eq!(store.approvals().unwrap(), vec![changed]);
    }
}
