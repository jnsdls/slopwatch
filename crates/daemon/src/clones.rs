//! The daemon's own clones: one blobless bare clone per repo under
//! `repos/<owner>/<name>.git`. It never touches the developer's checkouts.
//! Git work on one repo runs one operation at a time.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use slopwatch_protocol::{Cli, RepoName};
use tokio::process::Command;

use crate::clis::Clis;
use crate::github::{GitRemote, PIPELINE_PATH};

pub struct Clones {
    root: PathBuf,
    locks: Mutex<HashMap<RepoName, Arc<tokio::sync::Mutex<()>>>>,
    /// The CLI settings that say which git to run, and the `PATH` it's
    /// looked up on. `None` runs `git` from the daemon's own `PATH`.
    clis: Option<(Arc<Clis>, String)>,
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
            clis: None,
        }
    }

    /// Runs the git the CLI settings name, looked up on `path`.
    pub fn with_git(mut self, clis: Arc<Clis>, path: impl Into<String>) -> Self {
        self.clis = Some((clis, path.into()));
        self
    }

    /// The git to run.
    fn program(&self) -> String {
        match &self.clis {
            Some((clis, path)) => clis.program(Cli::Git, path),
            None => "git".to_owned(),
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
        let _turn = self.turn(repo).await;
        let path = self.cloned(repo, remote).await?;
        let sha = self.fetch_branch(&path, remote, branch).await?;
        let listed = self
            .git(
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
                self.git(
                    &path,
                    remote,
                    &["cat-file", "blob", &format!("{sha}:{PIPELINE_PATH}")],
                )
                .await?,
            )
        };
        Ok(PipelineAt { sha, text })
    }

    /// The repo's default branch, the Pipeline file at its tip, and that
    /// file's blob SHA.
    pub async fn default_pipeline(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
    ) -> Result<(String, PipelineAt, Option<String>), GitError> {
        let path = {
            let _turn = self.turn(repo).await;
            self.cloned(repo, remote).await?
        };
        let head = self
            .git(
                &path,
                remote,
                &["ls-remote", "--symref", &remote.url, "HEAD"],
            )
            .await?;
        let branch = head
            .lines()
            .find_map(|line| {
                line.strip_prefix("ref: refs/heads/")?
                    .strip_suffix("\tHEAD")
            })
            .ok_or_else(|| GitError(format!("{repo} has no default branch")))?
            .to_owned();
        let at = self.pipeline_at(repo, remote, &branch).await?;
        let blob = match at.text {
            Some(_) => {
                let file = format!("{}:{PIPELINE_PATH}", at.sha);
                Some(self.git(&path, remote, &["rev-parse", &file]).await?)
            }
            None => None,
        };
        Ok((branch, at, blob))
    }

    /// The paths PR `number` changes at `head_sha`, against where it
    /// branched from `base_sha`, the way GitHub lists a PR's files. A
    /// rename lists both paths. Fetches the PR's head first; listing names
    /// needs only trees, so no blob comes down.
    pub async fn changed_files(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        number: u64,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<Vec<String>, GitError> {
        for sha in [base_sha, head_sha] {
            if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(GitError(format!("`{sha}` isn't a commit SHA")));
            }
        }
        let _turn = self.turn(repo).await;
        let path = self.cloned(repo, remote).await?;
        self.git(
            &path,
            remote,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--filter=blob:none",
                &remote.url,
                &format!("+refs/pull/{number}/head:refs/slopwatch/pull/{number}"),
            ],
        )
        .await?;
        let listed = self
            .git(
                &path,
                remote,
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    "-z",
                    &format!("{base_sha}...{head_sha}"),
                    "--",
                ],
            )
            .await?;
        Ok(listed
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// PR `number`'s unified diff at `head_sha` against where it branched
    /// from `base_sha`, the diff GitHub shows for it. The blobless clone
    /// fetches the blobs it needs from the remote here. Bytes that aren't
    /// UTF-8 come back replaced.
    pub async fn diff(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        number: u64,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<String, GitError> {
        for sha in [base_sha, head_sha] {
            if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(GitError(format!("`{sha}` isn't a commit SHA")));
            }
        }
        let _turn = self.turn(repo).await;
        let path = self.cloned(repo, remote).await?;
        self.git(
            &path,
            remote,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--filter=blob:none",
                &remote.url,
                &format!("+refs/pull/{number}/head:refs/slopwatch/pull/{number}"),
            ],
        )
        .await?;
        let output = self
            .git_output(
                &path,
                remote,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    &format!("{base_sha}...{head_sha}"),
                    "--",
                ],
            )
            .await?;
        Ok(String::from_utf8_lossy(&output).into_owned())
    }

    /// Fetches `branch` and returns the commit at its tip, so a stacked
    /// PR's files can be listed against its parent's head.
    pub async fn branch_tip(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        branch: &str,
    ) -> Result<String, GitError> {
        let _turn = self.turn(repo).await;
        let path = self.cloned(repo, remote).await?;
        self.fetch_branch(&path, remote, branch).await
    }

    /// The commit at the tip of `branch` on the remote, or `None` if the
    /// remote has no such branch. Fetches nothing.
    pub async fn tip(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        branch: &str,
    ) -> Result<Option<String>, GitError> {
        let path = {
            let _turn = self.turn(repo).await;
            self.cloned(repo, remote).await?
        };
        let listed = self
            .git(
                &path,
                remote,
                &["ls-remote", &remote.url, &format!("refs/heads/{branch}")],
            )
            .await?;
        Ok(listed.split_whitespace().next().map(str::to_owned))
    }

    /// Whether the Pipeline file at `base_sha` differs from the one at the
    /// commit where `head_sha` branched from it. Both commits must have
    /// been fetched, as [`Clones::pipeline_at`] does.
    pub async fn pipeline_moved_since_fork(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        head_sha: &str,
        base_sha: &str,
    ) -> Result<bool, GitError> {
        let _turn = self.turn(repo).await;
        let path = self.cloned(repo, remote).await?;
        let fork = self
            .git(&path, remote, &["merge-base", head_sha, base_sha])
            .await?;
        let blob = async |commit: &str| {
            let listed = self
                .git(&path, remote, &["ls-tree", commit, "--", PIPELINE_PATH])
                .await?;
            Ok::<_, GitError>(listed.split_whitespace().nth(2).map(str::to_owned))
        };
        Ok(blob(&fork).await? != blob(base_sha).await?)
    }

    /// The parents of `sha`, a commit the clone has fetched.
    pub async fn parents(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        sha: &str,
    ) -> Result<Vec<String>, GitError> {
        let _turn = self.turn(repo).await;
        let path = self.cloned(repo, remote).await?;
        let listed = self
            .git(&path, remote, &["log", "-1", "--format=%P", sha])
            .await?;
        Ok(listed.split_whitespace().map(str::to_owned).collect())
    }

    /// Checks out PR `number` at `head_sha` into `dir` as a detached
    /// worktree of the repo's clone, for a Step that declares a workspace.
    /// Whatever was at `dir` goes first, and so does the bookkeeping of
    /// worktrees whose directories are gone. The checkout fetches the
    /// blobs it needs from the remote.
    pub async fn add_worktree(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        number: u64,
        head_sha: &str,
        dir: &Path,
    ) -> Result<(), GitError> {
        if head_sha.is_empty() || !head_sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(GitError(format!("`{head_sha}` isn't a commit SHA")));
        }
        let _turn = self.turn(repo).await;
        let clone = self.cloned(repo, remote).await?;
        let commit = format!("{head_sha}^{{commit}}");
        if self
            .git(&clone, remote, &["cat-file", "-e", &commit])
            .await
            .is_err()
        {
            self.git(
                &clone,
                remote,
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--filter=blob:none",
                    &remote.url,
                    &format!("+refs/pull/{number}/head:refs/slopwatch/pull/{number}"),
                ],
            )
            .await?;
        }
        self.git(&clone, remote, &["worktree", "prune"]).await?;
        let _ = tokio::fs::remove_dir_all(dir).await;
        let target = dir
            .to_str()
            .ok_or_else(|| GitError(format!("{} isn't UTF-8", dir.display())))?;
        self.git(
            &clone,
            remote,
            &[
                "worktree", "add", "--quiet", "--detach", "--force", target, head_sha,
            ],
        )
        .await
        .map(drop)
    }

    /// What the files in `dir`, a worktree of PR `number` checked out at
    /// `head`, change against `head`, as a tree in the clone and the paths
    /// that differ. Files `.gitignore` names are left out. The tree and the
    /// changed files' blobs go into the clone, so a commit can be made
    /// from them once the worktree is gone. `index` is a scratch file
    /// outside the worktree, removed afterwards, so whatever the Step did
    /// to the worktree's own index or `HEAD` doesn't count.
    pub async fn worktree_changes(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        dir: &Path,
        head: &str,
        index: &Path,
    ) -> Result<WorktreeChanges, GitError> {
        let _turn = self.turn(repo).await;
        let clone = self.cloned(repo, remote).await?;
        // The clone is named outright, never found through the worktree's
        // `.git` file. The Step could have pointed that at a repo whose
        // config runs its code with the remote's credential in reach.
        let env = [
            ("GIT_DIR", utf8(&clone)?),
            ("GIT_WORK_TREE", utf8(dir)?),
            ("GIT_INDEX_FILE", utf8(index)?),
        ];
        let safe = |args: &[&str]| -> Vec<String> {
            [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
            ]
            .iter()
            .chain(args)
            .map(|arg| (*arg).to_owned())
            .collect()
        };
        let _ = tokio::fs::remove_file(index).await;
        let result = async {
            let run = async |args: Vec<String>| {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                self.git_output_env(dir, remote, &env, &args).await
            };
            run(safe(&["read-tree", head])).await?;
            run(safe(&["add", "--all", "--", "."])).await?;
            let tree = String::from_utf8(run(safe(&["write-tree"])).await?)
                .map_err(|_| GitError("git write-tree wrote non-UTF-8".to_owned()))?
                .trim_end()
                .to_owned();
            let diff = [
                "diff-tree",
                "-r",
                "-z",
                "--raw",
                "--no-renames",
                head,
                &tree,
            ];
            let raw = run(safe(&diff)).await?;
            Ok(WorktreeChanges {
                tree,
                changes: parse_raw_diff(&raw)?,
            })
        }
        .await;
        let _ = tokio::fs::remove_file(index).await;
        result
    }

    /// The trees of each of `commits` the clone has, in order. A commit it
    /// doesn't have is left out.
    pub async fn trees(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        commits: &[String],
    ) -> Vec<String> {
        let _turn = self.turn(repo).await;
        let Ok(path) = self.cloned(repo, remote).await else {
            return Vec::new();
        };
        let mut trees = Vec::new();
        for commit in commits {
            if let Ok(tree) = self
                .git(
                    &path,
                    remote,
                    &[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("{commit}^{{tree}}"),
                    ],
                )
                .await
            {
                trees.push(tree);
            }
        }
        trees
    }

    /// The contents of blob `oid`, which the clone has or fetches.
    pub async fn blob(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        oid: &str,
    ) -> Result<Vec<u8>, GitError> {
        let _turn = self.turn(repo).await;
        let path = self.cloned(repo, remote).await?;
        self.git_output_env(&path, remote, &[], &["cat-file", "blob", oid])
            .await
    }

    /// The parents and tree of `sha`, PR `number`'s head on GitHub,
    /// fetching the PR's head first if the clone doesn't have it.
    pub async fn commit_of(
        &self,
        repo: &RepoName,
        remote: &GitRemote,
        number: u64,
        sha: &str,
    ) -> Result<(Vec<String>, String), GitError> {
        let _turn = self.turn(repo).await;
        let path = self.cloned(repo, remote).await?;
        let commit = format!("{sha}^{{commit}}");
        if self
            .git(&path, remote, &["cat-file", "-e", &commit])
            .await
            .is_err()
        {
            self.git(
                &path,
                remote,
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    "--filter=blob:none",
                    &remote.url,
                    &format!("+refs/pull/{number}/head:refs/slopwatch/pull/{number}"),
                ],
            )
            .await?;
        }
        let listed = self
            .git(&path, remote, &["log", "-1", "--format=%P%n%T", sha])
            .await?;
        let mut lines = listed.lines();
        let parents = lines
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let tree = lines.next().unwrap_or_default().to_owned();
        Ok((parents, tree))
    }

    /// Waits for the repo's turn: one git operation per repo at a time.
    async fn turn(&self, repo: &RepoName) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .locks
            .lock()
            .expect("no panics while holding the clone locks")
            .entry(repo.clone())
            .or_default()
            .clone();
        lock.lock_owned().await
    }

    /// The repo's clone, made first if the daemon has none.
    async fn cloned(&self, repo: &RepoName, remote: &GitRemote) -> Result<PathBuf, GitError> {
        let path = self.path(repo);
        if !path.join("HEAD").exists() {
            self.clone_bare(&path, remote).await?;
        }
        Ok(path)
    }

    async fn clone_bare(&self, path: &Path, remote: &GitRemote) -> Result<(), GitError> {
        // A clone that died halfway leaves a directory git won't clone into.
        let _ = tokio::fs::remove_dir_all(path).await;
        let parent = path.parent().expect("clone paths have a parent");
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| GitError(format!("can't create {}: {error}", parent.display())))?;
        let target = path.to_str().expect("clone paths are UTF-8");
        self.git(
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

/// What a worktree changes against the commit it was checked out at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeChanges {
    /// The whole tree the worktree holds.
    pub tree: String,
    pub changes: Vec<Change>,
}

/// One path that differs, as `git diff-tree --raw` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    /// `A`dded, `D`eleted, `M`odified or `T`ype changed.
    pub status: char,
    /// The file modes before and after, such as `100644`. `000000` where
    /// the path doesn't exist.
    pub old_mode: String,
    pub new_mode: String,
    /// The blob after, all zeroes for a deletion.
    pub blob: String,
}

/// `path` as text, for an env var.
fn utf8(path: &Path) -> Result<&str, GitError> {
    path.to_str()
        .ok_or_else(|| GitError(format!("{} isn't UTF-8", path.display())))
}

/// Reads `git diff-tree -r -z --raw` output: `:old new oldsha newsha
/// status` then the path, each ending in NUL.
fn parse_raw_diff(raw: &[u8]) -> Result<Vec<Change>, GitError> {
    let text = String::from_utf8(raw.to_vec())
        .map_err(|_| GitError("a changed path isn't UTF-8".to_owned()))?;
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    let mut changes = Vec::new();
    while let Some(meta) = fields.next() {
        let path = fields
            .next()
            .ok_or_else(|| GitError(format!("git diff-tree listed `{meta}` without a path")))?;
        let parts: Vec<&str> = meta.trim_start_matches(':').split_whitespace().collect();
        let [old_mode, new_mode, _, blob, status] = parts[..] else {
            return Err(GitError(format!("can't read git diff-tree's `{meta}`")));
        };
        changes.push(Change {
            path: path.to_owned(),
            status: status.chars().next().unwrap_or('?'),
            old_mode: old_mode.to_owned(),
            new_mode: new_mode.to_owned(),
            blob: blob.to_owned(),
        });
    }
    Ok(changes)
}

impl Clones {
    /// Fetches `branch` into the clone at `path` and returns its tip.
    async fn fetch_branch(
        &self,
        path: &Path,
        remote: &GitRemote,
        branch: &str,
    ) -> Result<String, GitError> {
        if branch.starts_with('-') || branch.contains("..") || branch.contains(':') {
            return Err(GitError(format!("`{branch}` isn't a branch name")));
        }
        let local = format!("refs/slopwatch/base/{branch}");
        self.git(
            path,
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
        self.git(path, remote, &["rev-parse", "--verify", &local])
            .await
    }

    /// Runs git in `dir` and returns its stdout, trimmed of the final newline
    /// for one-line answers. Text output keeps everything else as is.
    async fn git(&self, dir: &Path, remote: &GitRemote, args: &[&str]) -> Result<String, GitError> {
        self.git_env(dir, remote, &[], args).await
    }

    /// [`Self::git`] with `env` set besides the remote's.
    async fn git_env(
        &self,
        dir: &Path,
        remote: &GitRemote,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Result<String, GitError> {
        let output = self.git_output_env(dir, remote, env, args).await?;
        let mut text = String::from_utf8(output)
            .map_err(|_| GitError(format!("git {} wrote non-UTF-8", args[0])))?;
        if args[0] != "cat-file" {
            text.truncate(text.trim_end().len());
        }
        Ok(text)
    }

    /// Runs git in `dir` and returns its stdout as it wrote it.
    async fn git_output(
        &self,
        dir: &Path,
        remote: &GitRemote,
        args: &[&str],
    ) -> Result<Vec<u8>, GitError> {
        self.git_output_env(dir, remote, &[], args).await
    }

    /// [`Self::git_output`] with `env` set besides the remote's.
    async fn git_output_env(
        &self,
        dir: &Path,
        remote: &GitRemote,
        env: &[(&str, &str)],
        args: &[&str],
    ) -> Result<Vec<u8>, GitError> {
        let output = Command::new(self.program())
            .current_dir(dir)
            .args(args)
            .envs(remote.env.iter().map(|(k, v)| (k, v)))
            .envs(env.iter().copied())
            .env("GIT_TERMINAL_PROMPT", "0")
            // A checkout fetches the blobs it needs from the remote, but not
            // Git LFS objects, which a review doesn't need and which would
            // need credentials of their own.
            .env("GIT_LFS_SKIP_SMUDGE", "1")
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
        Ok(output.stdout)
    }
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
    async fn reads_the_pipeline_on_the_default_branch_with_its_blob() {
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path());

        let (branch, at, blob) = clones.default_pipeline(&repo(), &remote).await.unwrap();
        assert_eq!(branch, "main");
        assert_eq!((at.text, blob), (None, None));

        let sha = github.add_pipeline(&repo(), "main");
        let (_, at, blob) = clones.default_pipeline(&repo(), &remote).await.unwrap();
        assert_eq!(at.sha, sha);
        assert_eq!(at.text.as_deref(), Some(CI_PIPELINE));
        let blob = blob.unwrap();
        assert_eq!(blob.len(), 40, "a blob SHA: {blob}");
        assert_ne!(blob, sha);
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
    async fn lists_the_files_a_pr_changes_since_it_branched() {
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        github.open_pr(&repo(), 3, "me", "Docs");
        github.push_file(&repo(), 3, "docs/guide.md", "Read me.\n");
        // main moving on doesn't add its files to the PR's.
        let base = github.set_pipeline(&repo(), "main", CI_PIPELINE);
        let head = github.head_sha(&repo(), 3);
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path());

        let files = clones
            .changed_files(&repo(), &remote, 3, &base, &head)
            .await
            .unwrap();

        assert_eq!(files, ["change-3.txt", "docs/guide.md"]);
    }

    #[tokio::test]
    async fn reads_the_diff_a_pr_makes_since_it_branched() {
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        github.open_pr(&repo(), 3, "me", "Docs");
        github.push_file(&repo(), 3, "docs/guide.md", "Read me.\n");
        let base = github.set_pipeline(&repo(), "main", CI_PIPELINE);
        let head = github.head_sha(&repo(), 3);
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path());

        let diff = clones
            .diff(&repo(), &remote, 3, &base, &head)
            .await
            .unwrap();

        assert!(
            diff.contains("diff --git a/docs/guide.md b/docs/guide.md"),
            "{diff}"
        );
        assert!(diff.contains("+Read me."), "{diff}");
        assert!(diff.contains("+A change."), "{diff}");
        assert!(
            !diff.contains(".slopwatch/pipeline.yml"),
            "main moving on isn't part of the PR: {diff}"
        );
        assert!(diff.ends_with('\n'), "kept whole");
    }

    #[tokio::test]
    async fn checks_a_prs_head_out_into_a_worktree_over_whatever_was_there() {
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        github.open_pr(&repo(), 3, "me", "Docs");
        github.push_file(&repo(), 3, "docs/guide.md", "Read me.\n");
        let head = github.head_sha(&repo(), 3);
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path().join("repos"));
        let tree = dir.path().join("worktrees/1/review.1");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("leftover"), "from a crash").unwrap();

        clones
            .add_worktree(&repo(), &remote, 3, &head, &tree)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(tree.join("docs/guide.md")).unwrap(),
            "Read me.\n"
        );
        assert!(!tree.join("leftover").exists());
        // A Step's exit only deletes its directory. The next checkout of
        // the same head still works.
        std::fs::remove_dir_all(&tree).unwrap();
        clones
            .add_worktree(&repo(), &remote, 3, &head, &tree)
            .await
            .unwrap();
        assert!(tree.join("docs/guide.md").exists());
    }

    #[tokio::test]
    async fn a_worktrees_changes_are_a_tree_in_the_clone_and_the_paths_that_differ() {
        use std::os::unix::fs::PermissionsExt as _;
        let github = FakeGitHub::new("me");
        github.add_repo(&repo());
        github.open_pr(&repo(), 3, "me", "Docs");
        github.push_file(&repo(), 3, "docs/guide.md", "Read me.\n");
        github.push_file(&repo(), 3, "old.txt", "Going.\n");
        github.push_file(&repo(), 3, ".gitignore", "target/\n");
        let head = github.head_sha(&repo(), 3);
        let remote = github.git_remote(&repo()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let clones = Clones::new(dir.path().join("repos"));
        let tree = dir.path().join("worktrees/1/fix.1");
        clones
            .add_worktree(&repo(), &remote, 3, &head, &tree)
            .await
            .unwrap();
        let index = dir.path().join("worktrees/1/fix.1.index");

        let unchanged = clones
            .worktree_changes(&repo(), &remote, &tree, &head, &index)
            .await
            .unwrap();
        assert!(unchanged.changes.is_empty());
        assert_eq!(
            clones
                .trees(&repo(), &remote, std::slice::from_ref(&head))
                .await,
            std::slice::from_ref(&unchanged.tree)
        );

        std::fs::write(tree.join("docs/guide.md"), "Read me twice.\n").unwrap();
        std::fs::remove_file(tree.join("old.txt")).unwrap();
        std::fs::write(tree.join("new.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(tree.join("new.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::create_dir(tree.join("target")).unwrap();
        std::fs::write(tree.join("target/out"), "built").unwrap();
        std::os::unix::fs::symlink("docs/guide.md", tree.join("link")).unwrap();

        let changed = clones
            .worktree_changes(&repo(), &remote, &tree, &head, &index)
            .await
            .unwrap();
        let summary: Vec<(char, &str, &str, &str)> = changed
            .changes
            .iter()
            .map(|c| {
                (
                    c.status,
                    c.path.as_str(),
                    c.old_mode.as_str(),
                    c.new_mode.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ('M', "docs/guide.md", "100644", "100644"),
                ('A', "link", "000000", "120000"),
                ('A', "new.sh", "000000", "100755"),
                ('D', "old.txt", "100644", "000000"),
            ]
        );
        assert!(!index.exists(), "the scratch index goes");
        // A Step that repoints the worktree's `.git` file at a repo of its
        // own changes nothing: the clone is named outright.
        std::fs::write(tree.join(".git"), "gitdir: /nonexistent\n").unwrap();
        let again = clones
            .worktree_changes(&repo(), &remote, &tree, &head, &index)
            .await
            .unwrap();
        assert_eq!(again.tree, changed.tree);
        let guide = &changed.changes[0];
        assert_eq!(
            clones.blob(&repo(), &remote, &guide.blob).await.unwrap(),
            b"Read me twice.\n"
        );
        let (parents, head_tree) = clones.commit_of(&repo(), &remote, 3, &head).await.unwrap();
        assert_eq!(parents.len(), 1);
        assert_eq!(head_tree, unchanged.tree);
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
