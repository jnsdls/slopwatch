//! What the Plugins list shows: each Plugin, what its manifest asks for,
//! what its Approval covers, and the developer's settings for it, with the
//! commands that approve it and save its settings (ADR 0012). No GPUI here,
//! so it tests without a window.

use slopwatch_protocol::{
    Cause, Command, Grant, InboxEntry, PluginListing, PluginSettings, Scope, workspace_name,
};

#[derive(Debug, Default)]
pub struct PluginsList {
    plugins: Vec<PluginListing>,
    loaded: bool,
    /// The Plugin whose detail shows.
    chosen: Option<String>,
}

impl PluginsList {
    pub fn listed(&mut self, plugins: Vec<PluginListing>) {
        self.plugins = plugins;
        self.loaded = true;
    }

    pub fn plugins(&self) -> &[PluginListing] {
        &self.plugins
    }

    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// How many Plugins wait for the developer's Approval.
    pub fn waiting(&self) -> usize {
        self.plugins
            .iter()
            .filter(|plugin| plugin.needs_approval())
            .count()
    }

    pub fn choose(&mut self, name: &str) {
        self.chosen = Some(name.to_owned());
    }

    /// The chosen Plugin. A built-in comes before a file that took its
    /// name, so the name picks the built-in.
    pub fn chosen(&self) -> Option<&PluginListing> {
        let name = self.chosen.as_deref()?;
        self.plugins.iter().find(|plugin| plugin.name == name)
    }
}

/// The command that approves `plugin` for what it asks for as listed, if
/// it needs approving.
pub fn approve(plugin: &PluginListing) -> Option<Command> {
    if !plugin.needs_approval() {
        return None;
    }
    Some(Command::ApprovePlugin {
        plugin: plugin.name.clone(),
        grant: plugin.asks.clone()?,
    })
}

/// What a Grant holds, one line each.
pub fn grant_lines(grant: &Grant) -> Vec<String> {
    let list = |items: Vec<String>| {
        if items.is_empty() {
            "none".to_owned()
        } else {
            items.join(", ")
        }
    };
    vec![
        format!("Workspace: {}", workspace_name(grant.workspace)),
        format!(
            "Effects: {}",
            list(grant.effects.iter().map(ToString::to_string).collect())
        ),
        format!("Secrets: {}", list(grant.secrets.clone())),
    ]
}

/// What the manifest asks for now that its Approval doesn't cover, one
/// phrase each. Empty for a Plugin never approved, whose asks are all new.
pub fn asks_more(plugin: &PluginListing) -> Vec<String> {
    match (&plugin.approved, &plugin.asks) {
        (Some(approved), Some(asks)) => approved.beyond(asks),
        _ => Vec::new(),
    }
}

/// Where a Plugin stands, as its row reads.
pub fn state_line(plugin: &PluginListing) -> String {
    if let Some(problem) = &plugin.problem {
        return format!("Didn't load: {problem}");
    }
    if plugin.builtin {
        return "Built in, approved".to_owned();
    }
    let more = asks_more(plugin);
    if !more.is_empty() {
        return format!("Paused: now asks for {}", more.join(", "));
    }
    if plugin.needs_approval() {
        return "Needs approval".to_owned();
    }
    "Approved".to_owned()
}

/// The command that saves `plugin`'s settings from what the fields hold:
/// `PATH` dirs separated by `:`, and a cap that's blank for none. `Err`
/// says what's wrong with them.
pub fn save_settings(plugin: &str, path: &str, cap: &str) -> Result<Command, String> {
    let cap = match cap.trim() {
        "" => None,
        cap => Some(
            cap.parse::<u32>()
                .map_err(|_| format!("`{cap}` isn't a whole number"))?,
        ),
    };
    let settings = PluginSettings {
        path: path
            .split(':')
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
            .map(str::to_owned)
            .collect(),
        cap,
    };
    if let Some(problem) = settings.problem() {
        return Err(problem);
    }
    Ok(Command::SetPluginSettings {
        plugin: plugin.to_owned(),
        settings,
    })
}

/// The settings as the fields show them.
pub fn settings_fields(settings: &PluginSettings) -> (String, String) {
    (
        settings.path.join(":"),
        settings.cap.map(|cap| cap.to_string()).unwrap_or_default(),
    )
}

/// The Plugin an Inbox entry asks the developer to approve, if it does.
pub fn unapproved_plugin(entry: &InboxEntry) -> Option<&str> {
    match &entry.scope {
        Scope::Cause {
            cause: Cause::UnapprovedPlugin { plugin },
        } => Some(plugin),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_core::Workspace;
    use slopwatch_protocol::EntryId;
    use slopwatch_protocol::step::EffectKind;

    fn grant(workspace: Workspace, effects: &[EffectKind], secrets: &[&str]) -> Grant {
        Grant {
            workspace,
            effects: effects.to_vec(),
            secrets: secrets.iter().map(|&s| s.to_owned()).collect(),
        }
    }

    fn listing(name: &str, asks: Option<Grant>, approved: Option<Grant>) -> PluginListing {
        PluginListing {
            name: name.into(),
            builtin: false,
            path: Some(format!("/plugins/{name}")),
            version: asks.as_ref().map(|_| "1".to_owned()),
            asks,
            approved,
            approved_at: None,
            problem: None,
            settings: PluginSettings::default(),
        }
    }

    #[test]
    fn a_row_reads_where_the_plugin_stands() {
        let asks = grant(Workspace::Read, &[EffectKind::Comment], &[]);
        let new = listing("lint", Some(asks.clone()), None);
        assert_eq!(state_line(&new), "Needs approval");
        let approved = listing("lint", Some(asks.clone()), Some(asks.clone()));
        assert_eq!(state_line(&approved), "Approved");
        let more = listing(
            "lint",
            Some(grant(Workspace::Write, &[EffectKind::Comment], &["KEY"])),
            Some(asks),
        );
        assert_eq!(
            state_line(&more),
            "Paused: now asks for the `write` workspace, Secret `KEY`"
        );
        let broken = PluginListing {
            problem: Some("`merge` is reserved".into()),
            ..listing("merge", None, None)
        };
        assert_eq!(state_line(&broken), "Didn't load: `merge` is reserved");
        let builtin = PluginListing {
            builtin: true,
            ..approved.clone()
        };
        assert_eq!(state_line(&builtin), "Built in, approved");
    }

    #[test]
    fn approving_sends_what_the_list_showed_and_only_when_it_needs_it() {
        let asks = grant(Workspace::None, &[], &["KEY"]);
        let new = listing("lint", Some(asks.clone()), None);
        assert_eq!(
            approve(&new),
            Some(Command::ApprovePlugin {
                plugin: "lint".into(),
                grant: asks.clone(),
            })
        );
        assert_eq!(
            approve(&listing("lint", Some(asks.clone()), Some(asks))),
            None
        );
        assert_eq!(approve(&listing("broken", None, None)), None);
    }

    #[test]
    fn waiting_counts_plugins_that_need_approval() {
        let asks = grant(Workspace::None, &[], &[]);
        let mut list = PluginsList::default();
        list.listed(vec![
            listing("a", Some(asks.clone()), None),
            listing("b", Some(asks.clone()), Some(asks)),
            listing("c", None, None),
        ]);
        assert_eq!(list.waiting(), 1);
        list.choose("b");
        assert_eq!(list.chosen().unwrap().name, "b");
    }

    #[test]
    fn a_grant_reads_as_three_lines() {
        assert_eq!(
            grant_lines(&grant(
                Workspace::Write,
                &[EffectKind::Comment, EffectKind::Label],
                &[]
            )),
            vec![
                "Workspace: write",
                "Effects: comment, label",
                "Secrets: none"
            ]
        );
    }

    #[test]
    fn settings_fields_parse_into_a_command_or_say_why_not() {
        assert_eq!(
            save_settings("lint", " /opt/a/bin : /opt/b ", "2"),
            Ok(Command::SetPluginSettings {
                plugin: "lint".into(),
                settings: PluginSettings {
                    path: vec!["/opt/a/bin".into(), "/opt/b".into()],
                    cap: Some(2),
                },
            })
        );
        assert_eq!(
            save_settings("lint", "", ""),
            Ok(Command::SetPluginSettings {
                plugin: "lint".into(),
                settings: PluginSettings::default(),
            })
        );
        assert!(save_settings("lint", "bin", "").is_err());
        assert!(save_settings("lint", "", "two").is_err());
        assert!(save_settings("lint", "", "0").is_err());
        let settings = PluginSettings {
            path: vec!["/a".into(), "/b".into()],
            cap: Some(3),
        };
        assert_eq!(
            settings_fields(&settings),
            ("/a:/b".to_owned(), "3".to_owned())
        );
    }

    #[test]
    fn an_unapproved_plugin_entry_names_the_plugin() {
        let entry = InboxEntry {
            id: EntryId(1),
            scope: Scope::Cause {
                cause: Cause::UnapprovedPlugin {
                    plugin: "lint".into(),
                },
            },
            title: "Plugin `lint` needs approval".into(),
            reasons: vec![],
            prs: vec![],
            raised_at: 0,
            closed: None,
        };
        assert_eq!(unapproved_plugin(&entry), Some("lint"));
        let pr = InboxEntry {
            scope: Scope::Pr,
            ..entry
        };
        assert_eq!(unapproved_plugin(&pr), None);
    }
}
