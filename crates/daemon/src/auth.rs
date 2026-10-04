//! Where the daemon's GitHub credential comes from. This is the only module
//! that reads it, so a GitHub App installation token can replace the
//! developer's `gh` token here alone (GitHub identity and push ownership,
//! #15).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use slopwatch_protocol::Cli;
use tokio::process::Command;

use crate::clis::Clis;
use crate::github::GitHubError;

#[async_trait]
pub trait Credentials: Send + Sync {
    /// A token for GitHub's API.
    async fn token(&self) -> Result<String, GitHubError>;

    /// Drops a token GitHub rejected, so the next call reads a fresh one.
    fn forget(&self);
}

/// The developer's own `gh` login, read with `gh auth token` from the `gh`
/// the CLI settings name.
pub struct GhToken {
    clis: Arc<Clis>,
    /// The `PATH` a `gh` named without a path is looked up on.
    path: String,
    /// The token, and the `gh` that printed it. Setting another `gh` reads
    /// a fresh one.
    cached: Mutex<Option<(String, String)>>,
}

impl GhToken {
    pub fn new(clis: Arc<Clis>, path: impl Into<String>) -> Self {
        Self {
            clis,
            path: path.into(),
            cached: Mutex::new(None),
        }
    }

    fn cached(&self) -> std::sync::MutexGuard<'_, Option<(String, String)>> {
        self.cached
            .lock()
            .expect("no panics while holding the token")
    }
}

impl Default for GhToken {
    /// `gh` at its usual name, on the daemon's own `PATH` or where Homebrew
    /// puts it.
    fn default() -> Self {
        Self::new(
            Arc::new(Clis::default()),
            std::env::var("PATH").unwrap_or_default(),
        )
    }
}

#[async_trait]
impl Credentials for GhToken {
    async fn token(&self) -> Result<String, GitHubError> {
        let gh = self
            .clis
            .resolve(Cli::Gh, &self.path)
            .map_err(GitHubError::Auth)?
            .display()
            .to_string();
        if let Some((token, from)) = self.cached().clone()
            && from == gh
        {
            return Ok(token);
        }
        let output = Command::new(&gh)
            .args(["auth", "token"])
            .output()
            .await
            .map_err(|error| GitHubError::Auth(format!("can't run {gh}: {error}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GitHubError::Auth(format!(
                "`gh auth token` failed: {}",
                stderr.trim()
            )));
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if token.is_empty() {
            return Err(GitHubError::Auth("`gh auth token` printed nothing".into()));
        }
        *self.cached() = Some((token.clone(), gh));
        Ok(token)
    }

    fn forget(&self) {
        *self.cached() = None;
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use slopwatch_protocol::CliSettings;

    use super::*;

    fn fake_gh(dir: &Path, name: &str, token: &str) -> PathBuf {
        let file = dir.join(name);
        std::fs::write(
            &file,
            format!("#!/bin/sh\n[ \"$1 $2\" = 'auth token' ] && echo {token}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        file
    }

    fn gh_set_to(clis: &Clis, executable: &str) {
        clis.set(
            Cli::Gh,
            CliSettings {
                executable: Some(executable.into()),
                ..CliSettings::default()
            },
        );
    }

    #[tokio::test]
    async fn the_token_comes_from_the_gh_the_settings_name() {
        let path = tempfile::tempdir().unwrap();
        let brew = tempfile::tempdir().unwrap();
        fake_gh(brew.path(), "gh", "from-homebrew");
        fake_gh(path.path(), "work-gh", "from-work-gh");
        let elsewhere = tempfile::tempdir().unwrap();
        let absolute = fake_gh(elsewhere.path(), "gh", "from-absolute");
        let clis = Arc::new(Clis::default().with_fallbacks(vec![brew.path().to_owned()]));
        let token = GhToken::new(Arc::clone(&clis), path.path().to_str().unwrap());

        // Unset, and with no `gh` on PATH, Homebrew's is the default.
        assert_eq!(token.token().await.unwrap(), "from-homebrew");

        gh_set_to(&clis, "work-gh");
        assert_eq!(token.token().await.unwrap(), "from-work-gh");

        gh_set_to(&clis, absolute.to_str().unwrap());
        assert_eq!(token.token().await.unwrap(), "from-absolute");

        // A name set by hand replaces the Homebrew fallback.
        gh_set_to(&clis, "gh");
        let Err(GitHubError::Auth(problem)) = token.token().await else {
            panic!("no gh on PATH");
        };
        assert_eq!(problem, "`gh` isn't on PATH");
    }
}
