//! What the daemon asks of GitHub. [`api::Api`] is the real one, and
//! [`fake::FakeGitHub`] stands in for it in tests.

pub mod api;
pub mod fake;

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use slopwatch_protocol::RepoName;

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

    /// Adds the `slopwatch` label to a PR, or removes it when `on` is false.
    async fn set_label(&self, repo: &RepoName, number: u64, on: bool) -> Result<(), GitHubError>;
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
