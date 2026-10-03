//! The Plugins the daemon can run, and where it finds them.
//!
//! Built-in Plugins run from the daemon's own executable, as
//! `slopwatchd plugin <name> describe` and `slopwatchd plugin <name> run`,
//! so they ship and update with the app and speak the Step contract like
//! any third-party Plugin. Only `ci` is built so far. Third-party Plugins
//! and their Approval come with their own ticket (ADR 0012).

pub mod ci;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use slopwatch_core::{PluginInfo, Resolver};
use slopwatch_protocol::step::Manifest;

use crate::Library;

/// What a Run's Pipeline can name: the installed Plugins, and Library
/// Steps through `lib/`, read live.
pub struct Plugins {
    /// The executable built-in Plugins run from.
    builtin_exe: PathBuf,
    library: Arc<Library>,
}

impl Plugins {
    pub fn new(builtin_exe: impl Into<PathBuf>, library: Arc<Library>) -> Self {
        Self {
            builtin_exe: builtin_exe.into(),
            library,
        }
    }

    pub fn manifest(&self, plugin: &str) -> Option<Manifest> {
        builtin_manifest(plugin)
    }

    /// The program and arguments that start a session with `plugin`.
    pub fn command(&self, plugin: &str) -> Option<(PathBuf, Vec<String>)> {
        builtin_manifest(plugin)?;
        Some((
            self.builtin_exe.clone(),
            vec!["plugin".into(), plugin.into(), "run".into()],
        ))
    }
}

fn builtin_manifest(plugin: &str) -> Option<Manifest> {
    match plugin {
        "ci" => Some(ci::manifest()),
        _ => None,
    }
}

impl Resolver for Plugins {
    fn library_step(&self, name: &str) -> Option<String> {
        self.library.step(name)
    }

    fn plugin(&self, name: &str) -> Option<PluginInfo> {
        self.manifest(name).map(|manifest| PluginInfo {
            workspace: manifest.workspace,
            builtin: true,
        })
    }
}

/// Serves `slopwatchd plugin <name> describe|run`. `args` are the ones
/// after `plugin`.
pub fn main(args: &[String]) -> ExitCode {
    let (Some(name), Some(verb)) = (args.first(), args.get(1)) else {
        eprintln!("usage: slopwatchd plugin <name> describe|run");
        return ExitCode::from(2);
    };
    let Some(manifest) = builtin_manifest(name) else {
        eprintln!("slopwatchd: no built-in Plugin named `{name}`");
        return ExitCode::from(2);
    };
    let result = match verb.as_str() {
        "describe" => {
            let text = serde_json::to_string(&manifest).expect("manifests always serialize");
            writeln!(std::io::stdout(), "{text}")
        }
        "run" => match name.as_str() {
            "ci" => ci::run(std::io::stdin().lock(), std::io::stdout().lock()),
            _ => unreachable!("every built-in manifest has a session"),
        },
        _ => {
            eprintln!("usage: slopwatchd plugin <name> describe|run");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("slopwatchd plugin {name}: {error}");
            ExitCode::FAILURE
        }
    }
}
