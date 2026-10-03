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
use slopwatch_protocol::step::{
    CheckState, Checks, ChecksState, LinkedIssue, MergeMethod, MergeState, MergeStatus,
    UpdateMethod,
};

use super::{
    Branch, GitHub, GitHubError, GitRemote, Merged, NewCommit, NewPr, OpenPr, PIPELINE_PATH,
    ParentPr, Poll, PrLink,
    PrDetail, PrFate, RateLimit, RepoPoll, StackLink, WATCH_LABEL,
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
    hold_comments: Option<Hold>,
    /// Comment calls so far, held ones included.
    comment_calls: usize,
    /// Every Actions job rerun, in order.
    reruns: Vec<u64>,
    /// The id the next Actions job gets.
    next_job: u64,
    /// Every merge call, as `(PR, sha, method)`, in order.
    merges: Vec<(u64, String, Option<MergeMethod>)>,
    /// Every branch update, as `(PR, method)`, in order.
    updates: Vec<(u64, UpdateMethod)>,
    /// Every retarget the daemon asked for, as `(PR, base)`, in order.
    retargets: Vec<(u64, String)>,
    /// Branch update calls hang and never return while set.
    hold_updates: bool,
    /// Every commit made through the API, as `(branch, sha)`, in order.
    api_commits: Vec<(String, String)>,
}

/// Where a comment call stops and never returns, the way a daemon killed
/// mid-call leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    BeforePosting,
    AfterPosting,
}

struct Repo {
    git: PathBuf,
    pushable: bool,
    label_exists: bool,
    label_creations: usize,
    prs: BTreeMap<u64, Pr>,
    /// Every base branch has a merge queue.
    merge_queue: bool,
    /// Every base branch only takes signed commits.
    requires_signatures: bool,
}

struct Pr {
    author: String,
    title: String,
    base: String,
    /// The head branch.
    head: String,
    head_sha: String,
    open: bool,
    labels: BTreeSet<String>,
    checks: Checks,
    comments: Vec<String>,
    draft: bool,
    merged: bool,
    in_merge_queue: bool,
    /// The head conflicts with the base.
    conflicts: bool,
    /// Something outside the Run, such as a missing review, blocks it.
    blocked: bool,
    linked_issues: Vec<LinkedIssue>,
    /// The PR is in a GitHub native stack.
    native: bool,
    /// It merged with a merge commit, which keeps its commits on the base.
    kept_commits: bool,
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
                hold_comments: None,
                comment_calls: 0,
                reruns: Vec::new(),
                next_job: 1_000,
                merges: Vec::new(),
                updates: Vec::new(),
                retargets: Vec::new(),
                hold_updates: false,
                api_commits: Vec::new(),
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
                    merge_queue: false,
                    requires_signatures: false,
                },
            );
        });
    }

    /// Opens PR `number` by `author` against `main`, from a head branch
    /// with one commit on top of `main`.
    pub fn open_pr(&self, repo: &RepoName, number: u64, author: &str, title: &str) {
        self.open(repo, number, author, title, "main", false);
    }

    /// Opens PR `number` stacked on PR `parent`: its base is the parent's
    /// head branch, and its head branch has one commit on top of it.
    pub fn open_stacked_pr(
        &self,
        repo: &RepoName,
        number: u64,
        author: &str,
        title: &str,
        parent: u64,
    ) {
        self.open(repo, number, author, title, &head_branch(parent), false);
    }

    /// Like [`FakeGitHub::open_stacked_pr`], in a GitHub native stack. The
    /// parent must be in the stack too, or be its bottom.
    pub fn open_native_stacked_pr(
        &self,
        repo: &RepoName,
        number: u64,
        author: &str,
        title: &str,
        parent: u64,
    ) {
        self.with(|state| state.pr(repo, parent).native = true);
        self.open(repo, number, author, title, &head_branch(parent), true);
    }

    fn open(
        &self,
        repo: &RepoName,
        number: u64,
        author: &str,
        title: &str,
        base: &str,
        native: bool,
    ) {
        self.with(|state| {
            let git = state.repo(repo).git.clone();
            let file = format!("change-{number}.txt");
            let head_sha = state.commit(
                &git,
                &head_branch(number),
                Some(base),
                &[(&file, Some("A change.\n"))],
            );
            pull_ref(&git, number, &head_sha);
            state.repo(repo).prs.insert(
                number,
                Pr {
                    author: author.to_owned(),
                    title: title.to_owned(),
                    base: base.to_owned(),
                    head: head_branch(number),
                    head_sha,
                    open: true,
                    labels: BTreeSet::new(),
                    checks: Checks::default(),
                    comments: Vec::new(),
                    draft: false,
                    merged: false,
                    in_merge_queue: false,
                    conflicts: false,
                    blocked: false,
                    linked_issues: Vec::new(),
                    native,
                    kept_commits: false,
                },
            );
        });
    }

    /// Links an issue the PR closes, as `Fixes #n` in its description
    /// would.
    pub fn link_issue(&self, repo: &RepoName, number: u64, issue: LinkedIssue) {
        self.with(|state| state.pr(repo, number).linked_issues.push(issue));
    }

    /// Merges the PR the way someone on github.com would: with a merge
    /// commit for [`MergeMethod::Merge`], as one squashed commit
    /// otherwise.
    pub fn merge_on_github(&self, repo: &RepoName, number: u64, method: MergeMethod) {
        self.with(|state| state.land(repo, number, method));
    }

    /// The PR's base branch.
    pub fn base(&self, repo: &RepoName, number: u64) -> String {
        self.with(|state| state.pr(repo, number).base.clone())
    }

    /// Retargets the PR the way someone on github.com would.
    pub fn retarget_on_github(&self, repo: &RepoName, number: u64, base: &str) {
        self.with(|state| state.pr(repo, number).base = base.to_owned());
    }

    /// Every retarget the daemon asked for so far, as `(PR, base)`.
    pub fn retargets(&self) -> Vec<(u64, String)> {
        self.with(|state| state.retargets.clone())
    }

    /// Makes branch update calls hang, the way a daemon killed mid-call
    /// leaves them, until cleared. A call already held stays held.
    pub fn hold_updates(&self, hold: bool) {
        self.with(|state| state.hold_updates = hold);
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

    /// The comments on the PR, oldest first.
    pub fn comments(&self, repo: &RepoName, number: u64) -> Vec<String> {
        self.with(|state| state.pr(repo, number).comments.clone())
    }

    /// The labels on the PR.
    pub fn labels(&self, repo: &RepoName, number: u64) -> BTreeSet<String> {
        self.with(|state| state.pr(repo, number).labels.clone())
    }

    /// Makes comment calls hang at `hold` until cleared with `None`. A
    /// call already held stays held.
    pub fn hold_comments(&self, hold: Option<Hold>) {
        self.with(|state| state.hold_comments = hold);
    }

    /// Comment calls so far, held ones included.
    pub fn comment_calls(&self) -> usize {
        self.with(|state| state.comment_calls)
    }

    /// Every Actions job rerun so far, in order.
    pub fn reruns(&self) -> Vec<u64> {
        self.with(|state| state.reruns.clone())
    }

    pub fn set_draft(&self, repo: &RepoName, number: u64, draft: bool) {
        self.with(|state| state.pr(repo, number).draft = draft);
    }

    /// Makes the PR conflict with its base, or stop conflicting.
    pub fn set_conflicts(&self, repo: &RepoName, number: u64, conflicts: bool) {
        self.with(|state| state.pr(repo, number).conflicts = conflicts);
    }

    /// Makes GitHub block the PR's merge, as a missing review would.
    pub fn set_blocked(&self, repo: &RepoName, number: u64, blocked: bool) {
        self.with(|state| state.pr(repo, number).blocked = blocked);
    }

    /// Gives every base branch in `repo` a merge queue, or takes it away.
    pub fn set_merge_queue(&self, repo: &RepoName, on: bool) {
        self.with(|state| state.repo(repo).merge_queue = on);
    }

    /// Makes every base branch in `repo` require signed commits.
    pub fn set_requires_signatures(&self, repo: &RepoName, on: bool) {
        self.with(|state| state.repo(repo).requires_signatures = on);
    }

    /// Commits a file to `branch`, as someone landing other work on the
    /// base would, and returns the commit's SHA.
    pub fn commit_to(&self, repo: &RepoName, branch: &str, path: &str, text: &str) -> String {
        self.with(|state| {
            let git = state.repo(repo).git.clone();
            state.commit(&git, branch, None, &[(path, Some(text))])
        })
    }

    /// The merge queue lands the PR: its change goes onto its base, and
    /// the PR closes as merged.
    pub fn land_from_queue(&self, repo: &RepoName, number: u64) {
        self.with(|state| {
            assert!(state.pr(repo, number).in_merge_queue, "the PR isn't queued");
            state.land(repo, number, MergeMethod::Squash);
        });
    }

    /// The merge queue drops the PR, which stays open.
    pub fn eject_from_queue(&self, repo: &RepoName, number: u64) {
        self.with(|state| state.pr(repo, number).in_merge_queue = false);
    }

    pub fn is_merged(&self, repo: &RepoName, number: u64) -> bool {
        self.with(|state| state.pr(repo, number).merged)
    }

    pub fn is_in_merge_queue(&self, repo: &RepoName, number: u64) -> bool {
        self.with(|state| state.pr(repo, number).in_merge_queue)
    }

    /// Every merge call so far, as `(PR, sha, method)`.
    pub fn merges(&self) -> Vec<(u64, String, Option<MergeMethod>)> {
        self.with(|state| state.merges.clone())
    }

    /// Every branch update so far, as `(PR, method)`.
    pub fn updates(&self) -> Vec<(u64, UpdateMethod)> {
        self.with(|state| state.updates.clone())
    }

    /// Every commit made through the API so far, as `(branch, sha)`.
    pub fn api_commits(&self) -> Vec<(String, String)> {
        self.with(|state| state.api_commits.clone())
    }

    /// Every PR from `branch`, open or not, by number.
    pub fn prs_from(&self, repo: &RepoName, branch: &str) -> Vec<u64> {
        self.with(|state| {
            let prs = &state.repo(repo).prs;
            prs.iter()
                .filter(|(_, pr)| pr.head == branch)
                .map(|(&number, _)| number)
                .collect()
        })
    }

    pub fn is_open(&self, repo: &RepoName, number: u64) -> bool {
        self.with(|state| state.pr(repo, number).open)
    }

    /// The text of `path` at the tip of `branch`, if both exist.
    pub fn file(&self, repo: &RepoName, branch: &str, path: &str) -> Option<String> {
        self.with(|state| {
            let git = &state.repo(repo).git;
            let sha = tip(git, branch)?;
            has_file(git, branch, path)
                .then(|| git_raw(git, &["cat-file", "blob", &format!("{sha}:{path}")]))
        })
    }

    /// Whether `branch` exists.
    pub fn has_branch(&self, repo: &RepoName, branch: &str) -> bool {
        self.with(|state| tip(&state.repo(repo).git, branch).is_some())
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

    /// Moves the open PRs from `branch` to its new tip `sha`, as GitHub
    /// does on a push. The new head has no checks yet.
    fn follow_branch(&mut self, repo: &RepoName, branch: &str, sha: &str) {
        let fake = self.repo(repo);
        for (&number, pr) in &mut fake.prs {
            if pr.open && pr.head == branch && pr.head_sha != sha {
                pull_ref(&fake.git, number, sha);
                pr.head_sha = sha.to_owned();
                pr.checks = Checks::default();
            }
        }
    }

    /// Puts the PR's head tree on its base and closes the PR as merged:
    /// in a merge commit for [`MergeMethod::Merge`], which keeps the PR's
    /// commits, or in one new commit otherwise, as a squash or a rebase
    /// merge leaves copies. A native stack's next PR then moves onto the
    /// base and gets the base merged in, as GitHub does for native stacks.
    fn land(&mut self, repo: &RepoName, number: u64, method: MergeMethod) {
        let git = self.repo(repo).git.clone();
        let (base, head) = {
            let pr = self.pr(repo, number);
            (pr.base.clone(), pr.head_sha.clone())
        };
        let parent = tip(&git, &base).expect("the base exists");
        let tree = run_git(&git, &["rev-parse", &format!("{head}^{{tree}}")]);
        let kept_commits = method == MergeMethod::Merge;
        let mut args = vec!["commit-tree", &tree, "-p", &parent];
        if kept_commits {
            args.extend(["-p", &head]);
        }
        args.extend(["-m", "Merge"]);
        let sha = run_git(&git, &args);
        run_git(&git, &["update-ref", &format!("refs/heads/{base}"), &sha]);
        let pr = self.pr(repo, number);
        pr.open = false;
        pr.merged = true;
        pr.kept_commits = kept_commits;
        pr.in_merge_queue = false;
        let native = pr.native;
        let landed = head_branch(number);
        let children: Vec<u64> = self
            .repo(repo)
            .prs
            .iter()
            .filter(|(_, pr)| native && pr.native && pr.open && pr.base == landed)
            .map(|(&child, _)| child)
            .collect();
        for child in children {
            self.pr(repo, child).base = base.clone();
            self.bring_up_to_date(repo, child);
        }
    }

    /// Merges the PR's base into its head branch, which gets a new head
    /// with no checks yet.
    fn bring_up_to_date(&mut self, repo: &RepoName, number: u64) {
        let git = self.repo(repo).git.clone();
        let pr = self.pr(repo, number);
        let head = pr.head_sha.clone();
        let head_ref = format!("refs/heads/{}", pr.head);
        let base = tip(&git, &pr.base.clone()).expect("the base exists");
        let tree = run_git(&git, &["merge-tree", "--write-tree", &head, &base]);
        let sha = run_git(
            &git,
            &[
                "commit-tree",
                &tree,
                "-p",
                &head,
                "-p",
                &base,
                "-m",
                "Update branch",
            ],
        );
        run_git(&git, &["update-ref", &head_ref, &sha]);
        pull_ref(&git, number, &sha);
        let pr = self.pr(repo, number);
        pr.head_sha = sha;
        pr.checks = Checks::default();
    }

    /// Where an open PR sits in a Stack: the open PR whose head branch is
    /// its base, and that PR's base.
    fn stack_link(&self, name: &RepoName, number: u64) -> Option<StackLink> {
        let repo = &self.repos[name];
        let pr = &repo.prs[&number];
        let parent_of = |pr: &Pr| {
            repo.prs
                .iter()
                .find(|(n, parent)| parent.open && head_branch(**n) == pr.base)
        };
        let (&parent_number, parent) = parent_of(pr)?;
        // A native stack's bottom PR is position 1.
        let position = pr.native.then(|| {
            let mut position = 2;
            let mut below = parent;
            while let Some((_, next)) = parent_of(below).filter(|(_, next)| next.native) {
                position += 1;
                below = next;
            }
            position
        });
        Some(StackLink {
            parent: ParentPr {
                number: parent_number,
                title: parent.title.clone(),
                url: format!("https://github.com/{name}/pull/{parent_number}"),
                base: parent.base.clone(),
            },
            position,
            root: Branch {
                name: parent.base.clone(),
                sha: tip(&repo.git, &parent.base).unwrap_or_default(),
                has_pipeline: has_file(&repo.git, &parent.base, PIPELINE_PATH),
            },
        })
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

/// Runs git in `git` and returns its stdout untrimmed, such as a file's
/// text.
fn git_raw(git: &Path, args: &[&str]) -> String {
    let output = git_command(git, &[]).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap()
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
                                draft: pr.draft,
                                head_sha: pr.head_sha.clone(),
                                base: pr.base.clone(),
                                labeled: pr.labels.contains(WATCH_LABEL),
                                base_has_pipeline: has_file(&repo.git, &pr.base, PIPELINE_PATH),
                                stack: state.stack_link(name, number),
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

    async fn set_label(
        &self,
        repo: &RepoName,
        number: u64,
        label: &str,
        on: bool,
    ) -> Result<(), GitHubError> {
        self.with(|state| {
            let not_found = || GitHubError::NotFound(format!("{repo}#{number}"));
            let fake = state.repos.get_mut(repo).ok_or_else(not_found)?;
            if on && label == WATCH_LABEL && !fake.label_exists {
                // The REST API would create a bare label here. The daemon
                // should have created it first, so fail loudly instead.
                return Err(GitHubError::Other(format!("{repo} has no slopwatch label")));
            }
            let pr = fake.prs.get_mut(&number).ok_or_else(not_found)?;
            if on {
                pr.labels.insert(label.to_owned());
            } else {
                pr.labels.remove(label);
            }
            Ok(())
        })
    }

    async fn comment(&self, repo: &RepoName, number: u64, body: &str) -> Result<(), GitHubError> {
        let hold = self.with(|state| {
            state.comment_calls += 1;
            let hold = state.hold_comments;
            if hold != Some(Hold::BeforePosting) {
                let pr = state
                    .repos
                    .get_mut(repo)
                    .and_then(|fake| fake.prs.get_mut(&number))
                    .ok_or_else(|| GitHubError::NotFound(format!("{repo}#{number}")))?;
                pr.comments.push(body.to_owned());
            }
            Ok(hold)
        })?;
        if hold.is_some() {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn has_comment(
        &self,
        repo: &RepoName,
        number: u64,
        marker: &str,
    ) -> Result<bool, GitHubError> {
        self.with(|state| {
            let pr = state
                .repos
                .get(repo)
                .and_then(|fake| fake.prs.get(&number))
                .ok_or_else(|| GitHubError::NotFound(format!("{repo}#{number}")))?;
            Ok(pr.comments.iter().any(|comment| comment.contains(marker)))
        })
    }

    /// Like GitHub, replaces the job's check with a pending one on a new
    /// job.
    async fn rerun_job(&self, repo: &RepoName, job: u64) -> Result<(), GitHubError> {
        self.with(|state| {
            let next = state.next_job;
            let check = state
                .repos
                .get_mut(repo)
                .into_iter()
                .flat_map(|fake| fake.prs.values_mut())
                .find_map(|pr| {
                    let found = pr
                        .checks
                        .runs
                        .iter_mut()
                        .find(|check| check.actions_job == Some(job))?;
                    found.state = CheckState::Pending;
                    found.actions_job = Some(next);
                    pr.checks.state = ChecksState::Pending;
                    Some(())
                });
            check.ok_or_else(|| GitHubError::NotFound(format!("job {job} in {repo}")))?;
            state.next_job += 1;
            state.reruns.push(job);
            Ok(())
        })
    }

    async fn linked_issues(
        &self,
        repo: &RepoName,
        number: u64,
    ) -> Result<Vec<LinkedIssue>, GitHubError> {
        self.with(|state| {
            let not_found = || GitHubError::NotFound(format!("{repo}#{number}"));
            let fake = state.repos.get(repo).ok_or_else(not_found)?;
            let pr = fake.prs.get(&number).ok_or_else(not_found)?;
            Ok(pr.linked_issues.clone())
        })
    }

    async fn merge_state(
        &self,
        repo: &RepoName,
        number: u64,
        head_sha: &str,
    ) -> Result<MergeState, GitHubError> {
        self.with(|state| {
            let not_found = || GitHubError::NotFound(format!("{repo}#{number}"));
            let fake = state.repos.get(repo).ok_or_else(not_found)?;
            let pr = fake.prs.get(&number).ok_or_else(not_found)?;
            let behind_by = tip(&fake.git, &pr.base).map(|base| {
                run_git(
                    &fake.git,
                    &["rev-list", "--count", &format!("{head_sha}..{base}")],
                )
                .parse()
                .expect("git counts in digits")
            });
            Ok(MergeState {
                merged: pr.merged,
                status: if pr.draft {
                    MergeStatus::Draft
                } else if pr.conflicts {
                    MergeStatus::Dirty
                } else if pr.blocked {
                    MergeStatus::Blocked
                } else {
                    MergeStatus::Clean
                },
                conflicts: pr.conflicts,
                behind_by,
                merge_queue: fake.merge_queue,
                in_merge_queue: pr.in_merge_queue,
                requires_signatures: fake.requires_signatures,
                methods: vec![MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase],
            })
        })
    }

    /// Like GitHub's async merge API: refuses a moved head, conflicts and
    /// blocks, queues the PR when the base has a merge queue, and merges
    /// it otherwise.
    async fn merge(
        &self,
        repo: &RepoName,
        number: u64,
        sha: &str,
        method: Option<MergeMethod>,
    ) -> Result<Merged, GitHubError> {
        self.with(|state| {
            state.merges.push((number, sha.to_owned(), method));
            let queue = state.repo(repo).merge_queue;
            let pr = state.pr(repo, number);
            let refused = |message: &str| Err(GitHubError::Unprocessable(message.to_owned()));
            if pr.merged {
                return Ok(Merged::Merged);
            }
            if pr.head_sha != sha {
                return refused("Pull request head branch was modified.");
            }
            if pr.in_merge_queue {
                return Ok(Merged::Enqueued);
            }
            if pr.conflicts {
                return refused("Pull Request has merge conflicts");
            }
            if pr.blocked || pr.draft {
                return refused("Pull request is not mergeable");
            }
            if queue {
                pr.in_merge_queue = true;
                return Ok(Merged::Enqueued);
            }
            state.land(repo, number, method.unwrap_or(MergeMethod::Squash));
            Ok(Merged::Merged)
        })
    }

    /// Like `updatePullRequestBranch`, but with a merge commit whichever
    /// method: the head ends up with the base in its history, as either
    /// method leaves it, and has no checks yet.
    async fn update_branch(
        &self,
        repo: &RepoName,
        number: u64,
        expected_head: &str,
        method: UpdateMethod,
    ) -> Result<(), GitHubError> {
        let held = self.with(|state| state.hold_updates);
        if held {
            std::future::pending::<()>().await;
        }
        self.with(|state| {
            let pr = state.pr(repo, number);
            if pr.head_sha != expected_head {
                return Err(GitHubError::Unprocessable(
                    "head sha didn't match the current head ref.".into(),
                ));
            }
            if pr.conflicts {
                return Err(GitHubError::Unprocessable(
                    "merge conflict between base and head".into(),
                ));
            }
            state.bring_up_to_date(repo, number);
            state.updates.push((number, method));
            Ok(())
        })
    }

    async fn pr_fate(&self, repo: &RepoName, number: u64) -> Result<PrFate, GitHubError> {
        self.with(|state| {
            let pr = state
                .repos
                .get(repo)
                .and_then(|fake| fake.prs.get(&number))
                .ok_or_else(|| GitHubError::NotFound(format!("{repo}#{number}")))?;
            Ok(match (pr.merged, pr.open) {
                (true, _) => PrFate::Merged {
                    into: pr.base.clone(),
                    kept_commits: pr.kept_commits,
                },
                (false, true) => PrFate::Open,
                (false, false) => PrFate::Closed,
            })
        })
    }

    async fn set_base(&self, repo: &RepoName, number: u64, base: &str) -> Result<(), GitHubError> {
        self.with(|state| {
            let git = state.repo(repo).git.clone();
            if tip(&git, base).is_none() {
                return Err(GitHubError::Unprocessable(format!(
                    "Proposed base branch '{base}' was not found"
                )));
            }
            state.pr(repo, number).base = base.to_owned();
            state.retargets.push((number, base.to_owned()));
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

    async fn open_pr_from(
        &self,
        repo: &RepoName,
        branch: &str,
    ) -> Result<Option<PrLink>, GitHubError> {
        self.with(|state| {
            let fake = state
                .repos
                .get(repo)
                .ok_or_else(|| GitHubError::NotFound(repo.to_string()))?;
            Ok(fake
                .prs
                .iter()
                .find(|(_, pr)| pr.open && pr.head == branch)
                .map(|(&number, pr)| link(repo, number, pr)))
        })
    }

    /// Like GitHub, refuses a PR whose head has nothing the base lacks.
    async fn create_pr(&self, repo: &RepoName, new: &NewPr<'_>) -> Result<PrLink, GitHubError> {
        self.with(|state| {
            let viewer = state.viewer.clone();
            let fake = state
                .repos
                .get_mut(repo)
                .ok_or_else(|| GitHubError::NotFound(repo.to_string()))?;
            let invalid = |message: &str| GitHubError::Unprocessable(message.to_owned());
            let head_sha = tip(&fake.git, new.head).ok_or_else(|| invalid("head is invalid"))?;
            let base_sha = tip(&fake.git, new.base).ok_or_else(|| invalid("base is invalid"))?;
            if is_ancestor(&fake.git, &head_sha, &base_sha) {
                return Err(invalid(&format!(
                    "No commits between {} and {}",
                    new.base, new.head
                )));
            }
            let number = fake.prs.keys().max().map_or(1, |n| n + 1);
            pull_ref(&fake.git, number, &head_sha);
            let pr = Pr {
                author: viewer,
                title: new.title.to_owned(),
                base: new.base.to_owned(),
                head: new.head.to_owned(),
                head_sha,
                open: true,
                labels: BTreeSet::new(),
                checks: Checks::default(),
                comments: Vec::new(),
                draft: false,
                merged: false,
                in_merge_queue: false,
                conflicts: false,
                blocked: false,
                linked_issues: Vec::new(),
                native: false,
                kept_commits: false,
            };
            let link = link(repo, number, &pr);
            fake.prs.insert(number, pr);
            Ok(link)
        })
    }

    async fn set_branch(
        &self,
        repo: &RepoName,
        branch: &str,
        sha: &str,
    ) -> Result<(), GitHubError> {
        self.with(|state| {
            let git = state.repo(repo).git.clone();
            run_git(&git, &["update-ref", &format!("refs/heads/{branch}"), sha]);
            state.follow_branch(repo, branch, sha);
            Ok(())
        })
    }

    /// Like `createCommitOnBranch`, refuses a branch that isn't at the
    /// expected head.
    async fn commit_files(
        &self,
        repo: &RepoName,
        commit: &NewCommit<'_>,
    ) -> Result<String, GitHubError> {
        self.with(|state| {
            let git = state.repo(repo).git.clone();
            let head = tip(&git, commit.branch);
            if head.as_deref() != Some(commit.expected_head) {
                return Err(GitHubError::Other(format!(
                    "Expected branch to point to \"{}\" but it did not",
                    commit.expected_head
                )));
            }
            let files: Vec<(&str, Option<&str>)> = commit
                .files
                .iter()
                .map(|(path, text)| (*path, Some(*text)))
                .collect();
            let sha = state.commit(&git, commit.branch, None, &files);
            state.follow_branch(repo, commit.branch, &sha);
            state
                .api_commits
                .push((commit.branch.to_owned(), sha.clone()));
            Ok(sha)
        })
    }
}

fn link(repo: &RepoName, number: u64, pr: &Pr) -> PrLink {
    PrLink {
        number,
        url: format!("https://github.com/{repo}/pull/{number}"),
        head_sha: pr.head_sha.clone(),
    }
}

/// Whether `ancestor` is in `of`'s history.
fn is_ancestor(git: &Path, ancestor: &str, of: &str) -> bool {
    git_command(git, &[])
        .args(["merge-base", "--is-ancestor", ancestor, of])
        .status()
        .unwrap()
        .success()
}
