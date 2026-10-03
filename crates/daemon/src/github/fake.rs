//! An in-memory GitHub for tests. It keeps PRs by any author and answers
//! the way the real API does: a poll lists only the developer's open PRs.
//!
//! Each repo is also a real bare git repo in a temporary directory, so the
//! daemon's clones fetch from it the way they fetch from github.com. Every
//! PR has a head branch there, and pushes and Pipeline files are commits.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use slopwatch_protocol::RepoName;
use slopwatch_protocol::step::Checks;

use super::{
    GitHub, GitHubError, GitRemote, OpenPr, PIPELINE_PATH, Poll, PrDetail, RateLimit, RepoPoll,
    WATCH_LABEL,
};

/// The Pipeline [`FakeGitHub::add_pipeline`] commits: one CI Step and a
/// Gate on it.
pub const CI_PIPELINE: &str = "version: 1\nsteps:\n  ci: { uses: ci }\ngate: [ci]\n";

pub struct FakeGitHub {
    state: Mutex<State>,
    origins: tempfile::TempDir,
}

struct State {
    viewer: String,
    repos: BTreeMap<RepoName, Repo>,
    /// Points each poll costs.
    poll_cost: u32,
    remaining: u32,
    polls: usize,
    fail_polls: Option<GitHubError>,
    commits: u64,
}

struct Repo {
    git: PathBuf,
    pushable: bool,
    label_exists: bool,
    label_creations: usize,
    prs: BTreeMap<u64, Pr>,
}

struct Pr {
    author: String,
    title: String,
    base: String,
    head_sha: String,
    open: bool,
    labels: BTreeSet<String>,
    checks: Checks,
}

const LIMIT: u32 = 5000;

impl FakeGitHub {
    /// A GitHub where `viewer` is the developer.
    pub fn new(viewer: &str) -> Self {
        Self {
            state: Mutex::new(State {
                viewer: viewer.to_owned(),
                repos: BTreeMap::new(),
                poll_cost: 1,
                remaining: LIMIT,
                polls: 0,
                fail_polls: None,
                commits: 0,
            }),
            origins: tempfile::tempdir().expect("create a directory for the fake's git repos"),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    /// Adds a repo the developer can push to.
    pub fn add_repo(&self, repo: &RepoName) {
        self.add(repo, true);
    }

    /// Adds a repo the developer can read but not push to.
    pub fn add_read_only_repo(&self, repo: &RepoName) {
        self.add(repo, false);
    }

    fn add(&self, name: &RepoName, pushable: bool) {
        let git = self
            .origins
            .path()
            .join(&name.owner)
            .join(format!("{}.git", name.name));
        self.with(|state| {
            if let Some(repo) = state.repos.get_mut(name) {
                repo.pushable = pushable;
                return;
            }
            std::fs::create_dir_all(&git).unwrap();
            run_git(
                &git,
                &["init", "--quiet", "--bare", "--initial-branch=main"],
            );
            // GitHub serves partial clones, and a local repo only does when
            // asked to.
            run_git(&git, &["config", "uploadpack.allowFilter", "true"]);
            state.commit(&git, "main", None, &[("README.md", Some("A repo.\n"))]);
            state.repos.insert(
                name.clone(),
                Repo {
                    git,
                    pushable,
                    label_exists: false,
                    label_creations: 0,
                    prs: BTreeMap::new(),
                },
            );
        });
    }

    /// Opens PR `number` by `author` against `main`, from a head branch
    /// with one commit on top of `main`.
    pub fn open_pr(&self, repo: &RepoName, number: u64, author: &str, title: &str) {
        self.with(|state| {
            let git = state.repo(repo).git.clone();
            let file = format!("change-{number}.txt");
            let head_sha = state.commit(
                &git,
                &head_branch(number),
                Some("main"),
                &[(&file, Some("A change.\n"))],
            );
            pull_ref(&git, number, &head_sha);
            state.repo(repo).prs.insert(
                number,
                Pr {
                    author: author.to_owned(),
                    title: title.to_owned(),
                    base: "main".to_owned(),
                    head_sha,
                    open: true,
                    labels: BTreeSet::new(),
                    checks: Checks::default(),
                },
            );
        });
    }

    pub fn close_pr(&self, repo: &RepoName, number: u64) {
        self.with(|state| state.pr(repo, number).open = false);
    }

    pub fn rename_pr(&self, repo: &RepoName, number: u64, title: &str) {
        self.with(|state| state.pr(repo, number).title = title.to_owned());
    }

    /// Pushes a commit to the PR's head branch and returns its SHA. The new
    /// head has no checks yet.
    pub fn push(&self, repo: &RepoName, number: u64) -> String {
        let text = format!("Pushed at {}.\n", self.with(|state| state.commits));
        self.push_file(repo, number, &format!("change-{number}.txt"), &text)
    }

    /// Pushes a commit that writes `text` to `path` on the PR's head branch.
    pub fn push_file(&self, repo: &RepoName, number: u64, path: &str, text: &str) -> String {
        self.with(|state| {
            let git = state.repo(repo).git.clone();
            let sha = state.commit(&git, &head_branch(number), None, &[(path, Some(text))]);
            pull_ref(&git, number, &sha);
            let pr = state.pr(repo, number);
            pr.head_sha = sha.clone();
            pr.checks = Checks::default();
            sha
        })
    }

    pub fn head_sha(&self, repo: &RepoName, number: u64) -> String {
        self.with(|state| state.pr(repo, number).head_sha.clone())
    }

    /// Sets what GitHub reports for the checks on the PR's current head.
    pub fn set_checks(&self, repo: &RepoName, number: u64, checks: Checks) {
        self.with(|state| state.pr(repo, number).checks = checks);
    }

    /// Adds or removes the `slopwatch` label the way the developer would on
    /// github.com, creating the label if the repo has none.
    pub fn label_on_github(&self, repo: &RepoName, number: u64, on: bool) {
        self.with(|state| {
            if on {
                state.repo(repo).label_exists = true;
                state.pr(repo, number).labels.insert(WATCH_LABEL.to_owned());
            } else {
                state.pr(repo, number).labels.remove(WATCH_LABEL);
            }
        });
    }

    /// Adds or removes any other label on the PR.
    pub fn set_label(&self, repo: &RepoName, number: u64, label: &str, on: bool) {
        self.with(|state| {
            let labels = &mut state.pr(repo, number).labels;
            if on {
                labels.insert(label.to_owned());
            } else {
                labels.remove(label);
            }
        });
    }

    pub fn is_labeled(&self, repo: &RepoName, number: u64) -> bool {
        self.with(|state| state.pr(repo, number).labels.contains(WATCH_LABEL))
    }

    /// How often the daemon created the label in `repo`.
    pub fn label_creations(&self, repo: &RepoName) -> usize {
        self.with(|state| state.repo(repo).label_creations)
    }

    /// Commits [`CI_PIPELINE`] to `branch`.
    pub fn add_pipeline(&self, repo: &RepoName, branch: &str) -> String {
        self.set_pipeline(repo, branch, CI_PIPELINE)
    }

    /// Commits `text` as the Pipeline file on `branch` and returns the
    /// commit's SHA.
    pub fn set_pipeline(&self, repo: &RepoName, branch: &str, text: &str) -> String {
        self.with(|state| {
            let git = state.repo(repo).git.clone();
            state.commit(&git, branch, None, &[(PIPELINE_PATH, Some(text))])
        })
    }

    /// The SHA at the tip of `branch`.
    pub fn branch_sha(&self, repo: &RepoName, branch: &str) -> String {
        self.with(|state| tip(&state.repo(repo).git, branch).expect("the branch exists"))
    }

    /// Makes every poll cost `points`.
    pub fn set_poll_cost(&self, points: u32) {
        self.with(|state| state.poll_cost = points);
    }

    /// Makes polls fail with `error` until cleared with `None`.
    pub fn fail_polls(&self, error: Option<GitHubError>) {
        self.with(|state| state.fail_polls = error);
    }

    pub fn polls(&self) -> usize {
        self.with(|state| state.polls)
    }
}

fn head_branch(number: u64) -> String {
    format!("pr-{number}")
}

/// Points `refs/pull/<n>/head` at the PR's head, as GitHub does.
fn pull_ref(git: &Path, number: u64, sha: &str) {
    run_git(
        git,
        &["update-ref", &format!("refs/pull/{number}/head"), sha],
    );
}

impl State {
    fn repo(&mut self, repo: &RepoName) -> &mut Repo {
        self.repos
            .get_mut(repo)
            .unwrap_or_else(|| panic!("the fake has no repo {repo}"))
    }

    fn pr(&mut self, repo: &RepoName, number: u64) -> &mut Pr {
        self.repo(repo)
            .prs
            .get_mut(&number)
            .unwrap_or_else(|| panic!("the fake has no PR {repo}#{number}"))
    }

    /// Commits `files` (a `None` text deletes the path) on top of
    /// `branch`, or of `from` when `branch` doesn't exist yet, and moves
    /// `branch` to the commit.
    fn commit(
        &mut self,
        git: &Path,
        branch: &str,
        from: Option<&str>,
        files: &[(&str, Option<&str>)],
    ) -> String {
        self.commits += 1;
        let index = git.join(format!("fake-index-{}", self.commits));
        let index_env = [("GIT_INDEX_FILE", index.to_str().unwrap())];
        let parent = tip(git, branch).or_else(|| from.and_then(|from| tip(git, from)));
        if let Some(parent) = &parent {
            git_with(git, &["read-tree", parent], &index_env, None);
        }
        for (path, text) in files {
            match text {
                Some(text) => {
                    let blob = git_with(git, &["hash-object", "-w", "--stdin"], &[], Some(text));
                    let entry = format!("100644,{blob},{path}");
                    git_with(
                        git,
                        &["update-index", "--add", "--cacheinfo", &entry],
                        &index_env,
                        None,
                    );
                }
                None => {
                    git_with(
                        git,
                        &["update-index", "--force-remove", path],
                        &index_env,
                        None,
                    );
                }
            }
        }
        let tree = git_with(git, &["write-tree"], &index_env, None);
        let _ = std::fs::remove_file(&index);
        let message = format!("Commit {}", self.commits);
        let mut args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
        if let Some(parent) = &parent {
            args.extend(["-p", parent.as_str()]);
        }
        let sha = git_with(git, &args, &[], None);
        run_git(git, &["update-ref", &format!("refs/heads/{branch}"), &sha]);
        sha
    }
}

/// The SHA at the tip of `branch`, if it exists.
fn tip(git: &Path, branch: &str) -> Option<String> {
    let output = git_command(git, &[])
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

fn has_file(git: &Path, branch: &str, path: &str) -> bool {
    tip(git, branch).is_some_and(|sha| {
        !git_with(
            git,
            &["ls-tree", "--name-only", &sha, "--", path],
            &[],
            None,
        )
        .is_empty()
    })
}

fn run_git(git: &Path, args: &[&str]) -> String {
    git_with(git, args, &[], None)
}

/// Runs git in `git` and returns its trimmed stdout, panicking on failure.
fn git_with(git: &Path, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> String {
    use std::io::Write as _;
    let mut child = git_command(git, env)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// git in `dir`, cut off from the developer's own git config.
fn git_command(dir: &Path, env: &[(&str, &str)]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fake")
        .env("GIT_AUTHOR_EMAIL", "fake@example.test")
        .env("GIT_COMMITTER_NAME", "Fake")
        .env("GIT_COMMITTER_EMAIL", "fake@example.test")
        .envs(env.iter().copied());
    command
}

#[async_trait]
impl GitHub for FakeGitHub {
    async fn available_repos(&self) -> Result<Vec<RepoName>, GitHubError> {
        Ok(self.with(|state| {
            state
                .repos
                .iter()
                .filter(|(_, repo)| repo.pushable)
                .map(|(name, _)| name.clone())
                .collect()
        }))
    }

    async fn poll(&self, repos: &[RepoName]) -> Result<Poll, GitHubError> {
        self.with(|state| {
            state.polls += 1;
            if let Some(error) = &state.fail_polls {
                return Err(error.clone());
            }
            state.remaining = state.remaining.saturating_sub(state.poll_cost);
            let viewer = state.viewer.clone();
            let polled = repos
                .iter()
                .map(|name| RepoPoll {
                    repo: name.clone(),
                    prs: state.repos.get(name).map(|repo| {
                        repo.prs
                            .iter()
                            .filter(|(_, pr)| pr.open && pr.author == viewer)
                            .map(|(&number, pr)| OpenPr {
                                number,
                                title: pr.title.clone(),
                                url: format!("https://github.com/{name}/pull/{number}"),
                                draft: false,
                                head_sha: pr.head_sha.clone(),
                                base: pr.base.clone(),
                                labeled: pr.labels.contains(WATCH_LABEL),
                                base_has_pipeline: has_file(&repo.git, &pr.base, PIPELINE_PATH),
                                detail: PrDetail {
                                    body: String::new(),
                                    author: pr.author.clone(),
                                    labels: pr.labels.iter().cloned().collect(),
                                    base_sha: tip(&repo.git, &pr.base).unwrap_or_default(),
                                    checks: pr.checks.clone(),
                                },
                            })
                            .collect()
                    }),
                })
                .collect();
            Ok(Poll {
                repos: polled,
                rate: Some(RateLimit {
                    cost: state.poll_cost,
                    limit: LIMIT,
                    remaining: state.remaining,
                    resets_in: Duration::from_secs(3600),
                }),
            })
        })
    }

    async fn create_label(&self, repo: &RepoName) -> Result<(), GitHubError> {
        self.with(|state| {
            let repo = state
                .repos
                .get_mut(repo)
                .ok_or_else(|| GitHubError::NotFound(repo.to_string()))?;
            repo.label_creations += 1;
            repo.label_exists = true;
            Ok(())
        })
    }

    async fn set_label(&self, repo: &RepoName, number: u64, on: bool) -> Result<(), GitHubError> {
        self.with(|state| {
            let not_found = || GitHubError::NotFound(format!("{repo}#{number}"));
            let fake = state.repos.get_mut(repo).ok_or_else(not_found)?;
            if on && !fake.label_exists {
                // The REST API would create a bare label here. The daemon
                // should have created it first, so fail loudly instead.
                return Err(GitHubError::Other(format!("{repo} has no slopwatch label")));
            }
            let pr = fake.prs.get_mut(&number).ok_or_else(not_found)?;
            if on {
                pr.labels.insert(WATCH_LABEL.to_owned());
            } else {
                pr.labels.remove(WATCH_LABEL);
            }
            Ok(())
        })
    }

    async fn git_remote(&self, repo: &RepoName) -> Result<GitRemote, GitHubError> {
        self.with(|state| {
            let repo = state
                .repos
                .get(repo)
                .ok_or_else(|| GitHubError::NotFound(repo.to_string()))?;
            Ok(GitRemote {
                url: format!("file://{}", repo.git.display()),
                env: Vec::new(),
            })
        })
    }
}
