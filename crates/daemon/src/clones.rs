//! The daemon's own clones: one blobless bare clone per repo under
//! `repos/<owner>/<name>.git`. It never touches the developer's checkouts.
//! Git work on one repo runs one operation at a time.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use slopwatch_protocol::RepoName;
use tokio::process::Command;

use crate::github::{GitRemote, PIPELINE_PATH};

pub struct Clones {
    root: PathBuf,
    locks: Mutex<HashMap<RepoName, Arc<tokio::sync::Mutex<()>>>>,
}

/// The Pipeline file as it stands at the tip of a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineAt {
    /// The commit the file was read from.
    pub sha: String,
    /// `None` when the branch has no Pipeline file.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitError(pub String);

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitError {}

impl Clones {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn path(&self, repo: &RepoName) -> PathBuf {
        self.root
            .join(&repo.owner)
            .join(format!("{}.git", repo.name))
    }

    /// Fetches `branch` and reads the Pipeline file at its tip, cloning the
    /// repo first if the daemon has no clone of it yet.
    pub async fn pipeline_at(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        branch: &str,
    ) -> Result<PipelineAt, GitError> {
        if branch.starts_with('-') || branch.contains("..") || branch.contains(':') {
            return Err(GitError(format!("`{branch}` isn't a branch name")));
        }
        let lock = self
            .locks
            .lock()
            .expect("no panics while holding the clone locks")
            .entry(repo.clone())
            .or_default()
            .clone();
        let _turn = lock.lock().await;

        let path = self.path(repo);
        if !path.join("HEAD").exists() {
            self.clone_bare(&path, remote).await?;
        }
        let local = format!("refs/slopwatch/base/{branch}");
        git(
            &path,
            remote,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--filter=blob:none",
                &remote.url,
                &format!("+refs/heads/{branch}:{local}"),
            ],
        )
        .await?;
        let sha = git(&path, remote, &["rev-parse", "--verify", &local]).await?;
        let listed = git(
            &path,
            remote,
            &["ls-tree", "--name-only", &sha, "--", PIPELINE_PATH],
        )
        .await?;
        let text = if listed.is_empty() {
            None
        } else {
            // A blobless clone fetches the blob from the remote here.
            Some(
                git(
                    &path,
                    remote,
                    &["cat-file", "blob", &format!("{sha}:{PIPELINE_PATH}")],
                )
                .await?,
            )
        };
        Ok(PipelineAt { sha, text })
    }

    async fn clone_bare(&self, path: &Path, remote: &GitRemote) -> Result<(), GitError> {
        // A clone that died halfway leaves a directory git won't clone into.
        let _ = tokio::fs::remove_dir_all(path).await;
        let parent = path.parent().expect("clone paths have a parent");
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| GitError(format!("can't create {}: {error}", parent.display())))?;
        let target = path.to_str().expect("clone paths are UTF-8");
        git(
            parent,
            remote,
            &[
                "clone",
                "--quiet",
                "--bare",
                "--filter=blob:none",
                "--no-tags",
                &remote.url,
                target,
            ],
        )
        .await
        .map(drop)
    }
}

/// Runs git in `dir` and returns its stdout, trimmed of the final newline
/// for one-line answers. Text output keeps everything else as is.
async fn git(dir: &Path, remote: &GitRemote, args: &[&str]) -> Result<String, GitError> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs(remote.env.iter().map(|(k, v)| (k, v)))
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| GitError(format!("can't run git: {error}")))?;
    if !output.status.success() {
        return Err(GitError(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut text = String::from_utf8(output.stdout)
        .map_err(|_| GitError(format!("git {} wrote non-UTF-8", args[0])))?;
    if args[0] != "cat-file" {
        text.truncate(text.trim_end().len());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::GitHub;
    use crate::github::fake::{CI_PIPELINE, FakeGitHub};

    fn repo() -> RepoName {
        RepoName::new("o", "r")
    }

    #[tokio::test]
    async fn reads_the_pipeline_at_the_tip_of_a_branch_from_a_blobless_clone() {
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        let sha = github.add_pipeline(&repo(), "main");
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path());

        let read = clones.pipeline_at(&repo(), &remote, "main").await.unwrap();

        assert_eq!(read.sha, sha);
        assert_eq!(read.text.as_deref(), Some(CI_PIPELINE));
        let path = clones.path(&repo());
        assert!(path.join("HEAD").exists(), "a bare clone");
        let config = std::fs::read_to_string(path.join("config")).unwrap();
        assert!(
            config.contains("partialclonefilter = blob:none"),
            "{config}"
        );
    }

    #[tokio::test]
    async fn a_second_read_fetches_what_changed_since() {
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        github.add_pipeline(&repo(), "main");
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path());
        clones.pipeline_at(&repo(), &remote, "main").await.unwrap();

        let changed = "version: 1\nsteps:\n  checks: { uses: ci }\ngate: [checks]\n";
        let sha = github.set_pipeline(&repo(), "main", changed);
        let read = clones.pipeline_at(&repo(), &remote, "main").await.unwrap();

        assert_eq!(read.sha, sha);
        assert_eq!(read.text.as_deref(), Some(changed));
    }

    #[tokio::test]
    async fn a_branch_without_a_pipeline_reads_as_none() {
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path());

        let read = clones.pipeline_at(&repo(), &remote, "main").await.unwrap();

        assert_eq!(read.sha, github.branch_sha(&repo(), "main"));
        assert_eq!(read.text, None);
    }

    #[tokio::test]
    async fn a_missing_branch_is_an_error() {
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path());

        assert!(clones.pipeline_at(&repo(), &remote, "nope").await.is_err());
        assert!(
            clones
                .pipeline_at(&repo(), &remote, "--upload-pack=x")
                .await
                .is_err()
        );
    }
}
