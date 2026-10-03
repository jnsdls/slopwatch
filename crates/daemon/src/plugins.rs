//! The Plugins the daemon can run, and where it finds them.
//!
//! Built-in Plugins run from the daemon's own executable, as
//! `slopwatchd plugin <name> describe` and `slopwatchd plugin <name> run`,
//! so they ship and update with the app and speak the Step contract like
//! any third-party Plugin. Only `ci` is built so far. Third-party Plugins
//! and their Approval come with their own ticket (ADR 0012).

pub mod ci;

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use slopwatch_core::{PluginInfo, Resolver};
use slopwatch_protocol::step::Manifest;

use crate::Library;

/// What a Run's Pipeline can name: the installed Plugins, and Library
/// Steps through `lib/`, read live.
pub struct Plugins {
    /// The executable built-in Plugins run from.
    builtin_exe: PathBuf,
    /// SHA-256 of `builtin_exe`, and the file it was taken from.
    builtin_hash: Mutex<Option<(FileStamp, String)>>,
    library: Arc<Library>,
}

/// Tells one file from another at a path, or the same file rewritten.
type FileStamp = (u64, u64, u64, Option<SystemTime>);

fn file_stamp(path: &Path) -> std::io::Result<FileStamp> {
    let meta = std::fs::metadata(path)?;
    Ok((meta.dev(), meta.ino(), meta.len(), meta.modified().ok()))
}

impl Plugins {
    pub fn new(builtin_exe: impl Into<PathBuf>, library: Arc<Library>) -> Self {
        Self {
            builtin_exe: builtin_exe.into(),
            builtin_hash: Mutex::new(None),
            library,
        }
    }

    pub fn manifest(&self, plugin: &str) -> Option<Manifest> {
        builtin_manifest(plugin)
    }

    /// The version an Outcome is reused under: the manifest version plus
    /// a hash of the executable as it is now (ADR 0012). The hash is taken
    /// again whenever the file changed. `None` when the Plugin isn't
    /// installed or its executable can't be read, and then nothing is
    /// reused.
    pub fn version(&self, plugin: &str) -> Option<String> {
        let manifest = builtin_manifest(plugin)?;
        let hash = self.builtin_hash()?;
        Some(format!("{}+{hash}", manifest.version))
    }

    fn builtin_hash(&self) -> Option<String> {
        let path = &self.builtin_exe;
        let stamp = file_stamp(path)
            .inspect_err(|error| eprintln!("slopwatchd: can't hash {}: {error}", path.display()))
            .ok()?;
        let mut cached = self
            .builtin_hash
            .lock()
            .expect("no panics while holding the hash");
        if let Some((at, hash)) = cached.as_ref()
            && *at == stamp
        {
            return Some(hash.clone());
        }
        let hash = file_hash(path)?;
        *cached = Some((stamp, hash.clone()));
        Some(hash)
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

fn file_hash(path: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        let mut buffer = vec![0; 1 << 16];
        loop {
            match file.read(&mut buffer)? {
                0 => return Ok(()),
                n => hasher.update(&buffer[..n]),
            }
        }
    });
    if let Err(error) = read {
        eprintln!("slopwatchd: can't hash {}: {error}", path.display());
        return None;
    }
    Some(
        hasher
            .finalize()
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            }),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_follows_the_executable_and_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("slopwatchd");
        std::fs::write(&exe, "build one").unwrap();
        let library = Arc::new(Library::open(dir.path().join("steps")).unwrap());
        let plugins = Plugins::new(&exe, library);

        let first = plugins.version("ci").unwrap();
        assert!(first.starts_with(&format!("{}+", ci::manifest().version)));
        assert_eq!(plugins.version("ci").unwrap(), first);
        assert_eq!(plugins.version("nobody"), None);

        // An install swaps in a new file.
        let next = dir.path().join("next");
        std::fs::write(&next, "build two").unwrap();
        std::fs::rename(&next, &exe).unwrap();
        assert_ne!(plugins.version("ci").unwrap(), first);

        std::fs::remove_file(&exe).unwrap();
        assert_eq!(plugins.version("ci"), None);
    }
}
