use std::path::{Path, PathBuf};

/// Overrides the data dir, for tests and for running a daemon by hand next
/// to the installed ones.
pub const DATA_DIR_ENV: &str = "SLOPWATCH_DATA_DIR";

/// Which install a binary belongs to. A dev build and a release build run
/// side by side with their own bundle ID, launchd label, data dir and socket
/// (ADR 0009), because Background Task Management keys its records on bundle
/// ID and label.
///
/// `SLOPWATCH_FLAVOR=release` at build time makes a release build. Anything
/// else, including a plain `cargo build`, makes a dev build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Release,
    Dev,
}

impl Flavor {
    /// The flavor this binary was built as.
    pub const CURRENT: Flavor = match env!("SLOPWATCH_FLAVOR").as_bytes() {
        b"release" => Flavor::Release,
        _ => Flavor::Dev,
    };

    pub fn bundle_id(self) -> &'static str {
        match self {
            Flavor::Release => "com.jnsdls.slopwatch",
            Flavor::Dev => "com.jnsdls.slopwatch.dev",
        }
    }

    /// The name the app shows, so a dev window is told apart from a release
    /// one.
    pub fn app_name(self) -> &'static str {
        match self {
            Flavor::Release => "slopwatch",
            Flavor::Dev => "slopwatch dev",
        }
    }

    /// The daemon's launchd label. Its plist in the bundle's
    /// `Contents/Library/LaunchAgents` is named `<label>.plist`.
    pub fn agent_label(self) -> &'static str {
        match self {
            Flavor::Release => "com.jnsdls.slopwatch.daemon",
            Flavor::Dev => "com.jnsdls.slopwatch.dev.daemon",
        }
    }

    /// `~/Library/Application Support/slopwatch` for release builds and
    /// `slopwatch-dev` next to it for dev builds, unless
    /// `SLOPWATCH_DATA_DIR` names another directory.
    pub fn data_dir(self) -> PathBuf {
        if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|dir| !dir.is_empty()) {
            return PathBuf::from(dir);
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let name = match self {
            Flavor::Release => "slopwatch",
            Flavor::Dev => "slopwatch-dev",
        };
        home.join("Library/Application Support").join(name)
    }
}

/// Where the daemon that owns `data_dir` listens.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_and_release_share_no_identity() {
        assert_ne!(Flavor::Dev.bundle_id(), Flavor::Release.bundle_id());
        assert_ne!(Flavor::Dev.agent_label(), Flavor::Release.agent_label());
        if std::env::var_os(DATA_DIR_ENV).is_none() {
            assert_ne!(Flavor::Dev.data_dir(), Flavor::Release.data_dir());
        }
    }
}
