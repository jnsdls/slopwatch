//! Plugins on the wire: what the Plugins list shows, and the Approval the
//! developer gives a third-party Plugin (ADR 0012).

use serde::{Deserialize, Serialize};
use slopwatch_core::Workspace;

use crate::step::{EffectKind, Manifest};

/// What a Plugin asks for, or what its Approval lets it have: a
/// workspace, Effects and Secrets by name.
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

    /// Whether it lets a Step have the Secret `name`.
    pub fn covers_secret(&self, name: &str) -> bool {
        self.secrets.iter().any(|granted| granted == name)
    }

    /// What `asks` wants that this Grant doesn't cover, one phrase each,
    /// such as "the `write` workspace" or "Secret `KEY`". Empty when it
    /// covers all of it.
    pub fn beyond(&self, asks: &Grant) -> Vec<String> {
        let mut more = Vec::new();
        if rank(asks.workspace) > rank(self.workspace) {
            more.push(format!(
                "the `{}` workspace",
                workspace_name(asks.workspace)
            ));
        }
        for effect in &asks.effects {
            if !self.effects.contains(effect) {
                more.push(format!("the `{effect}` Effect"));
            }
        }
        for secret in &asks.secrets {
            if !self.covers_secret(secret) {
                more.push(format!("Secret `{secret}`"));
            }
        }
        more
    }

    /// Whether it covers everything `asks` wants.
    pub fn covers(&self, asks: &Grant) -> bool {
        self.beyond(asks).is_empty()
    }
}

/// `none` < `read` < `write`: a Grant of one workspace covers the ones
/// below it.
fn rank(workspace: Workspace) -> u8 {
    match workspace {
        Workspace::None => 0,
        Workspace::Read => 1,
        Workspace::Write => 2,
    }
}

/// A workspace as a manifest writes it.
pub fn workspace_name(workspace: Workspace) -> &'static str {
    match workspace {
        Workspace::None => "none",
        Workspace::Read => "read",
        Workspace::Write => "write",
    }
}

/// One Plugin as the Plugins list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginListing {
    /// The word a Pipeline writes in `uses:`.
    pub name: String,
    /// Ships with the app and comes approved.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin: bool,
    /// Where the daemon found a third-party Plugin, on its own machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The manifest's version. `None` when it didn't load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// What the manifest asks for. `None` when it didn't load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asks: Option<Grant>,
    /// What its Approval covers, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<Grant>,
    /// When it was approved, in seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_at: Option<i64>,
    /// Why it didn't load, such as a `describe` that hung or a reserved
    /// name. A Plugin with a problem can't run or be approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// The developer's settings for it in the daemon.
    #[serde(default)]
    pub settings: PluginSettings,
}

impl PluginListing {
    /// It loaded, and its Approval doesn't cover what it asks for now, so
    /// its Steps error until the developer approves it.
    pub fn needs_approval(&self) -> bool {
        match (&self.asks, &self.approved) {
            (Some(_), None) => true,
            (Some(asks), Some(approved)) => !approved.covers(asks),
            (None, _) => false,
        }
    }
}

/// The developer's per-Plugin settings in the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSettings {
    /// Absolute directories put in front of `PATH` for the Plugin's
    /// `describe` and its Steps, so it finds the tools it runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    /// At most this many of the Plugin's Steps run at once, in place of
    /// the manifest's `concurrency`. At least 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
    /// An absolute config directory for the CLI the Plugin runs, which its
    /// Steps get as `SLOPWATCH_CONFIG_DIR`. The `claude` Plugin runs Claude
    /// Code on a subscription with it as `CLAUDE_CONFIG_DIR`, for a login
    /// kept outside `~/.claude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

impl PluginSettings {
    /// Why these settings can't be kept, if they can't.
    pub fn problem(&self) -> Option<String> {
        if let Some(dir) = self.path.iter().find(|dir| !dir.starts_with('/')) {
            return Some(format!("`{dir}` isn't an absolute path"));
        }
        if let Some(dir) = self.path.iter().find(|dir| dir.contains(':')) {
            return Some(format!("`{dir}` holds a `:`, which `PATH` can't carry"));
        }
        if self.cap == Some(0) {
            return Some("A cap of 0 would hold the Plugin's Steps forever".to_owned());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(workspace: Workspace, effects: &[EffectKind], secrets: &[&str]) -> Grant {
        Grant {
            workspace,
            effects: effects.to_vec(),
            secrets: secrets.iter().map(|&s| s.to_owned()).collect(),
        }
    }

    #[test]
    fn a_grant_covers_less_or_the_same_and_names_whatever_is_more() {
        let approved = grant(Workspace::Read, &[EffectKind::Comment], &["KEY"]);

        assert!(approved.covers(&grant(Workspace::None, &[], &[])));
        assert!(approved.covers(&approved.clone()));
        assert_eq!(
            approved.beyond(&grant(
                Workspace::Write,
                &[EffectKind::Comment, EffectKind::Merge],
                &["KEY", "OTHER"],
            )),
            vec![
                "the `write` workspace".to_owned(),
                "the `merge` Effect".to_owned(),
                "Secret `OTHER`".to_owned(),
            ]
        );
    }

    #[test]
    fn a_listing_needs_approval_only_when_it_loaded_and_asks_for_more() {
        let asks = grant(Workspace::Read, &[], &["KEY"]);
        let mut listing = PluginListing {
            name: "lint".into(),
            builtin: false,
            path: None,
            version: Some("1".into()),
            asks: Some(asks.clone()),
            approved: None,
            approved_at: None,
            problem: None,
            settings: PluginSettings::default(),
        };
        assert!(listing.needs_approval());
        listing.approved = Some(asks);
        assert!(!listing.needs_approval());
        listing.asks = Some(grant(Workspace::Write, &[], &["KEY"]));
        assert!(listing.needs_approval());
        listing.asks = None;
        listing.problem = Some("hung".into());
        assert!(!listing.needs_approval());
    }

    #[test]
    fn settings_take_absolute_dirs_and_a_cap_of_at_least_one() {
        let ok = PluginSettings {
            path: vec!["/opt/tools/bin".into()],
            cap: Some(2),
            config_dir: Some("/Users/me/.claude-work".into()),
        };
        assert_eq!(ok.problem(), None);
        for bad in [
            PluginSettings {
                path: vec!["bin".into()],
                cap: None,
                config_dir: None,
            },
            PluginSettings {
                path: vec!["/a:/b".into()],
                cap: None,
                config_dir: None,
            },
            PluginSettings {
                path: vec![],
                cap: Some(0),
                ..PluginSettings::default()
            },
            PluginSettings {
                config_dir: Some("~/.claude".into()),
                ..PluginSettings::default()
            },
        ] {
            assert!(bad.problem().is_some(), "{bad:?}");
        }
    }
}
