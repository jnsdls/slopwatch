//! Where the daemon's GitHub credential comes from. This is the only module
//! that reads it, so a GitHub App installation token can replace the
//! developer's `gh` token here alone (GitHub identity and push ownership,
//! #15).

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::process::Command;

use crate::github::GitHubError;

#[async_trait]
pub trait Credentials: Send + Sync {
    /// A token for GitHub's API.
    async fn token(&self) -> Result<String, GitHubError>;

    /// Drops a token GitHub rejected, so the next call reads a fresh one.
    fn forget(&self);
}

/// The developer's own `gh` login, read with `gh auth token`.
#[derive(Default)]
pub struct GhToken {
    cached: Mutex<Option<String>>,
}

/// Where Homebrew puts `gh`. A launchd agent's `PATH` has neither.
const GH_FALLBACKS: [&str; 2] = ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"];

impl GhToken {
    fn cached(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.cached
            .lock()
            .expect("no panics while holding the token")
    }
}

#[async_trait]
impl Credentials for GhToken {
    async fn token(&self) -> Result<String, GitHubError> {
        if let Some(token) = self.cached().clone() {
            return Ok(token);
        }
        let candidates = std::iter::once("gh").chain(
            GH_FALLBACKS
                .into_iter()
                .filter(|path| Path::new(path).exists()),
        );
        let mut last_error = "gh isn't installed".to_owned();
        for gh in candidates {
            let output = match Command::new(gh).args(["auth", "token"]).output().await {
                Ok(output) => output,
                Err(error) => {
                    last_error = format!("can't run {gh}: {error}");
                    continue;
                }
            };
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
            *self.cached() = Some(token.clone());
            return Ok(token);
        }
        Err(GitHubError::Auth(last_error))
    }

    fn forget(&self) {
        *self.cached() = None;
    }
}
