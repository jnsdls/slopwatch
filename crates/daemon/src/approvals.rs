//! Approvals: what a Plugin may have, by the Plugin's name (ADR 0012).
//!
//! An Approval covers the workspace, the Effects and the Secrets the
//! Plugin's manifest asked for when it was approved. It is the Secret
//! grant: a Step gets a Secret only when its Plugin's Approval names it.
//! Built-in Plugins ship approved through the same record, written again
//! from their manifests each time the daemon starts. The developer
//! approves a third-party Plugin from the Plugins list, and the Approval
//! keeps through rebuilds until a manifest asks for more.

use slopwatch_protocol::step::Manifest;
use slopwatch_protocol::{Actor, Cause};

pub use slopwatch_protocol::Grant;

/// How a Step errors when its Plugin has no Approval, or its manifest
/// asks for more than the Approval covers. A shared Inbox entry per
/// Plugin holds its PR back.
const PLUGIN_UNAPPROVED: &str = "error(plugin unapproved)";

/// Why a Plugin can't run: it has no Approval, or its manifest asks for
/// more than its Approval covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unapproved {
    pub plugin: String,
    /// What the manifest asks for beyond its Approval, one phrase each.
    /// `None` when it was never approved.
    pub more: Option<Vec<String>>,
}

impl Unapproved {
    /// Whether `plugin` may run under `approval` with what its manifest
    /// `asks` for. `None` when it may.
    pub fn check(plugin: &str, asks: &Grant, approval: Option<&Approval>) -> Option<Self> {
        let more = match approval {
            None => None,
            Some(approval) => {
                let more = approval.grant.beyond(asks);
                if more.is_empty() {
                    return None;
                }
                Some(more)
            }
        };
        Some(Self {
            plugin: plugin.to_owned(),
            more,
        })
    }

    /// What's wrong, after "Plugin `<name>`".
    fn what(&self) -> String {
        match &self.more {
            None => "hasn't been approved".to_owned(),
            Some(more) => format!(
                "now asks for {}, which its Approval doesn't cover",
                more.join(", ")
            ),
        }
    }

    /// The error its Step settles with.
    pub fn reason(&self) -> String {
        format!(
            "{PLUGIN_UNAPPROVED}: Plugin `{}` {}",
            self.plugin,
            self.what()
        )
    }

    /// The line its Inbox entry shows.
    pub fn summary(&self) -> String {
        format!("Plugin `{}` {}", self.plugin, self.what())
    }
}

/// Whether a Step's error came from its Plugin `plugin` lacking an
/// Approval.
pub fn unapproved_plugin(reason: &str, plugin: &str) -> bool {
    reason.starts_with(&format!("{PLUGIN_UNAPPROVED}: Plugin `{plugin}`"))
}

/// Whether a Step's error is one an unapproved Plugin's entry explains.
pub fn held_by_approval(reason: &str) -> bool {
    reason.starts_with(PLUGIN_UNAPPROVED)
}

/// The shared cause an unapproved Plugin holds PRs back on.
pub fn cause(plugin: &str) -> Cause {
    Cause::UnapprovedPlugin {
        plugin: plugin.to_owned(),
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

    /// `actor`'s Approval of everything `manifest` asks for.
    pub fn granting(manifest: &Manifest, actor: Actor, now: i64) -> Self {
        Self {
            actor: Some(actor),
            ..Self::builtin(manifest, now)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use slopwatch_core::Workspace;
    use slopwatch_protocol::step::{EffectKind, STEP_DIALECT, SecretSpec};

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
    fn a_plugin_runs_only_under_an_approval_that_covers_what_it_asks() {
        let asks = Grant::of(&manifest());
        let mut approval = Approval::builtin(&manifest(), 10);

        assert_eq!(Unapproved::check("jev", &asks, Some(&approval)), None);
        let none = Unapproved::check("jev", &asks, None).unwrap().reason();
        assert!(unapproved_plugin(&none, "jev"));
        assert!(!unapproved_plugin(&none, "je"));
        assert!(held_by_approval(&none));

        approval.grant.secrets = vec!["JEV_API_KEY".into()];
        approval.grant.effects.clear();
        assert_eq!(
            Unapproved::check("jev", &asks, Some(&approval))
                .unwrap()
                .reason(),
            "error(plugin unapproved): Plugin `jev` now asks for the `comment` Effect, \
             Secret `EXTRA`, which its Approval doesn't cover"
        );
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
