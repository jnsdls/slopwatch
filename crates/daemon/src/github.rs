//! What the daemon asks of GitHub. [`api::Api`] is the real one, and
//! [`fake::FakeGitHub`] stands in for it in tests.

pub mod api;
pub mod fake;

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use slopwatch_protocol::RepoName;
use slopwatch_protocol::step::{Checks, LinkedIssue, MergeMethod, MergeState, UpdateMethod};

/// The label that makes a PR a Watched PR.
pub const WATCH_LABEL: &str = "slopwatch";

/// Where a repo's Pipeline lives on a branch.
pub const PIPELINE_PATH: &str = ".slopwatch/pipeline.yml";

#[async_trait]
pub trait GitHub: Send + Sync {
    /// The repos the developer can push to.
    async fn available_repos(&self) -> Result<Vec<RepoName>, GitHubError>;

    /// The developer's open PRs in each of `repos`, in one batch.
    async fn poll(&self, repos: &[RepoName]) -> Result<Poll, GitHubError>;

    /// Creates the `slopwatch` label in `repo`. A label that already exists
    /// counts as created.
    async fn create_label(&self, repo: &RepoName) -> Result<(), GitHubError>;

    /// Adds `label` to a PR, or removes it when `on` is false. Removing a
    /// label the PR doesn't carry succeeds.
    async fn set_label(
        &self,
        repo: &RepoName,
        number: u64,
        label: &str,
        on: bool,
    ) -> Result<(), GitHubError>;

    /// Posts a comment on a PR.
    async fn comment(&self, repo: &RepoName, number: u64, body: &str) -> Result<(), GitHubError>;

    /// Whether any comment on the PR contains `marker`.
    async fn has_comment(
        &self,
        repo: &RepoName,
        number: u64,
        marker: &str,
    ) -> Result<bool, GitHubError>;

    /// Reruns a GitHub Actions job.
    async fn rerun_job(&self, repo: &RepoName, job: u64) -> Result<(), GitHubError>;

    /// Where the PR stands for merging, with `head_sha` compared against
    /// its base.
    async fn merge_state(
        &self,
        repo: &RepoName,
        number: u64,
        head_sha: &str,
    ) -> Result<MergeState, GitHubError>;

    /// Merges the PR through the async merge API, directly or through the
    /// base's merge queue, and waits until GitHub says which (ADR 0011).
    /// Nothing lands unless the head is still `sha`. A merge GitHub
    /// declines, such as one with conflicts, is `Unprocessable`.
    async fn merge(
        &self,
        repo: &RepoName,
        number: u64,
        sha: &str,
        method: Option<MergeMethod>,
    ) -> Result<Merged, GitHubError>;

    /// Brings the PR's branch up to date with its base through
    /// `updatePullRequestBranch`, only if its head is still
    /// `expected_head` (ADR 0004). GitHub pushes the result a moment
    /// later, so the new head shows up in a later poll.
    async fn update_branch(
        &self,
        repo: &RepoName,
        number: u64,
        expected_head: &str,
        method: UpdateMethod,
    ) -> Result<(), GitHubError>;

    /// The issues PR `number` closes when it merges, as GitHub links them
    /// from its description or its sidebar.
    async fn linked_issues(
        &self,
        repo: &RepoName,
        number: u64,
    ) -> Result<Vec<LinkedIssue>, GitHubError>;

    /// Whether PR `number` is open, closed or merged, and how it merged.
    /// The daemon asks after a Stack parent that left the poll.
    async fn pr_fate(&self, repo: &RepoName, number: u64) -> Result<PrFate, GitHubError>;

    /// Retargets the PR onto `base` through `updatePullRequest`, as the
    /// daemon does to a Stack child once its parent merged (ADR 0011).
    /// Retargeting onto the base it already has succeeds.
    async fn set_base(&self, repo: &RepoName, number: u64, base: &str) -> Result<(), GitHubError>;

    /// Where git fetches `repo` from, with what authenticates it.
    async fn git_remote(&self, repo: &RepoName) -> Result<GitRemote, GitHubError>;
}

/// What became of a merge GitHub accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merged {
    /// GitHub merged the PR, now or on an earlier call.
    Merged,
    /// The base's merge queue has it.
    Enqueued,
}

/// What became of a PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrFate {
    Open,
    /// Closed without merging.
    Closed,
    Merged {
        /// The branch it merged into.
        into: String,
        /// The base now has the PR's own commits, as after a merge commit.
        /// A squash or a rebase merge puts copies there instead, while a
        /// child's branch still holds the originals.
        kept_commits: bool,
    },
}

/// A git remote and the environment a git process needs to reach it.
#[derive(Clone)]
pub struct GitRemote {
    pub url: String,
    /// Holds a credential for the real remote, so it never goes on a
    /// command line or into a log.
    pub env: Vec<(String, String)>,
}

impl fmt::Debug for GitRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitRemote")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// One poll's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Poll {
    pub repos: Vec<RepoPoll>,
    /// What the poll cost and what's left, when GitHub said.
    pub rate: Option<RateLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPoll {
    pub repo: RepoName,
    /// `None` when the repo is gone or the developer lost access to it.
    pub prs: Option<Vec<OpenPr>>,
}

/// One of the developer's open PRs, as GitHub reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenPr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub draft: bool,
    pub head_sha: String,
    pub base: String,
    /// The PR carries the `slopwatch` label.
    pub labeled: bool,
    /// The base branch's head has a Pipeline file.
    pub base_has_pipeline: bool,
    /// Where the PR sits in a Stack, when its base is another open PR's
    /// head branch.
    pub stack: Option<StackLink>,
    /// What a poll reads besides, for Step snapshots. The store doesn't
    /// keep these, so they're empty until the first poll after a start.
    pub detail: PrDetail,
}

impl OpenPr {
    /// The branch a Run reads the Pipeline from (ADR 0007): the base, or
    /// for a PR in a Stack, the base of the Stack's bottom PR.
    pub fn root(&self) -> Branch {
        match &self.stack {
            Some(stack) => stack.root.clone(),
            None => Branch {
                name: self.base.clone(),
                sha: self.detail.base_sha.clone(),
                has_pipeline: self.base_has_pipeline,
            },
        }
    }

    /// The open PR this one is stacked on.
    pub fn parent(&self) -> Option<u64> {
        self.stack.as_ref().map(|stack| stack.parent.number)
    }
}

/// A PR's place in a Stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackLink {
    /// The open PR whose head branch is this PR's base. It may be anyone's.
    pub parent: ParentPr,
    /// The PR's position in a GitHub native stack, 1 being the bottom.
    /// `None` for a Stack chained by hand or by another tool.
    pub position: Option<u32>,
    /// The Stack's root base. A poll reads the parent's base, and
    /// [`Watching`](crate::Watching) follows the parents down through the
    /// developer's own PRs to the bottom.
    pub root: Branch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentPr {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// The parent's own base branch.
    pub base: String,
}

/// A branch's tip, as a Run's Pipeline source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    /// Empty until a poll has read it.
    pub sha: String,
    pub has_pipeline: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrDetail {
    pub body: String,
    pub author: String,
    pub labels: Vec<String>,
    /// The base branch's head commit.
    pub base_sha: String,
    /// The checks on the head commit.
    pub checks: Checks,
}

/// GitHub's GraphQL rate limit after a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Points the call spent.
    pub cost: u32,
    /// Points per hour.
    pub limit: u32,
    /// Points left in this hour, shared with every other `gh` call the
    /// developer and their agents make.
    pub remaining: u32,
    pub resets_in: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubError {
    /// The repo or PR doesn't exist or the developer can't see it.
    NotFound(String),
    /// GitHub asked us to slow down, by a secondary rate limit or an empty
    /// primary budget.
    RateLimited { retry_after: Duration },
    /// No usable credential.
    Auth(String),
    /// GitHub refused the change as invalid, such as creating a label that
    /// already exists.
    Unprocessable(String),
    /// Anything else: the network, a 5xx, a response we couldn't read.
    Other(String),
}

impl fmt::Display for GitHubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitHubError::NotFound(message) => write!(f, "not found: {message}"),
            GitHubError::RateLimited { retry_after } => write!(
                f,
                "GitHub rate limit reached, retrying in {}s",
                retry_after.as_secs()
            ),
            GitHubError::Auth(message) => write!(f, "GitHub auth failed: {message}"),
            GitHubError::Unprocessable(message) => write!(f, "GitHub refused: {message}"),
            GitHubError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for GitHubError {}
