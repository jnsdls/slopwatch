// Each test binary uses a different part of this module.
#![allow(dead_code)]

use std::collections::HashMap;

use slopwatch_core::{LoadError, Pipeline, PluginInfo, Resolver, Workspace, load};

/// A Library and Plugin set held in memory. It knows the built-in Plugins
/// plus whatever a test adds.
pub struct TestResolver {
    pub library: HashMap<String, String>,
    pub plugins: HashMap<String, PluginInfo>,
}

impl Default for TestResolver {
    fn default() -> Self {
        let builtin = |workspace| PluginInfo {
            workspace,
            builtin: true,
        };
        let plugins = [
            ("jev", builtin(Workspace::None)),
            ("ci", builtin(Workspace::None)),
            ("claude", builtin(Workspace::Read)),
            ("codex", builtin(Workspace::Read)),
            ("fix", builtin(Workspace::Write)),
            ("human", builtin(Workspace::None)),
            ("merge", builtin(Workspace::None)),
        ]
        .into_iter()
        .map(|(name, info)| (name.to_owned(), info))
        .collect();
        TestResolver {
            library: HashMap::new(),
            plugins,
        }
    }
}

impl TestResolver {
    pub fn with_library(mut self, name: &str, text: &str) -> Self {
        self.library.insert(name.to_owned(), text.to_owned());
        self
    }

    pub fn with_third_party(mut self, name: &str, workspace: Workspace) -> Self {
        self.plugins.insert(
            name.to_owned(),
            PluginInfo {
                workspace,
                builtin: false,
            },
        );
        self
    }
}

impl Resolver for TestResolver {
    fn library_step(&self, name: &str) -> Option<String> {
        self.library.get(name).cloned()
    }

    fn plugin(&self, name: &str) -> Option<PluginInfo> {
        self.plugins.get(name).copied()
    }
}

pub fn load_ok(text: &str) -> Pipeline {
    load(text, &TestResolver::default()).unwrap_or_else(|errors| panic!("{errors:#?}"))
}

/// Loads a Pipeline that must fail and returns its error messages.
pub fn load_err_with(text: &str, resolver: &TestResolver) -> Vec<String> {
    match load(text, resolver) {
        Ok(_) => panic!("expected the Pipeline to fail to load"),
        Err(errors) => errors.iter().map(LoadError::to_string).collect(),
    }
}

pub fn load_err(text: &str) -> Vec<String> {
    load_err_with(text, &TestResolver::default())
}
