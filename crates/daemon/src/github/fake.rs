//! An in-memory GitHub for tests. It keeps PRs by any author and answers
//! the way the real API does: a poll lists only the developer's open PRs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use slopwatch_protocol::RepoName;

use super::{GitHub, GitHubError, OpenPr, Poll, RateLimit, RepoPoll, WATCH_LABEL};

pub struct FakeGitHub {
    state: Mutex<State>,
}

struct State {
    viewer: String,
    repos: BTreeMap<RepoName, Repo>,
    /// Points each poll costs.
    poll_cost: u32,
    remaining: u32,
    polls: usize,
    fail_polls: Option<GitHubError>,
}

#[derive(Default)]
struct Repo {
    pushable: bool,
    label_exists: bool,
    label_creations: usize,
    branches_with_pipeline: BTreeSet<String>,
    prs: BTreeMap<u64, Pr>,
}

struct Pr {
    author: String,
    title: String,
    base: String,
    head_sha: String,
    open: bool,
    labels: BTreeSet<String>,
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
            }),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    /// Adds a repo the developer can push to.
    pub fn add_repo(&self, repo: &RepoName) {
        self.with(|state| {
            state.repos.entry(repo.clone()).or_default().pushable = true;
        });
    }

    /// Adds a repo the developer can read but not push to.
    pub fn add_read_only_repo(&self, repo: &RepoName) {
        self.with(|state| {
            state.repos.entry(repo.clone()).or_default().pushable = false;
        });
    }

    /// Opens PR `number` by `author` against `main`.
    pub fn open_pr(&self, repo: &RepoName, number: u64, author: &str, title: &str) {
        self.with(|state| {
            state.repo(repo).prs.insert(
                number,
                Pr {
                    author: author.to_owned(),
                    title: title.to_owned(),
                    base: "main".to_owned(),
                    head_sha: format!("{number:040x}"),
                    open: true,
                    labels: BTreeSet::new(),
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

    pub fn is_labeled(&self, repo: &RepoName, number: u64) -> bool {
        self.with(|state| state.pr(repo, number).labels.contains(WATCH_LABEL))
    }

    /// How often the daemon created the label in `repo`.
    pub fn label_creations(&self, repo: &RepoName) -> usize {
        self.with(|state| state.repo(repo).label_creations)
    }

    pub fn add_pipeline(&self, repo: &RepoName, branch: &str) {
        self.with(|state| {
            state
                .repo(repo)
                .branches_with_pipeline
                .insert(branch.to_owned());
        });
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
                                base_has_pipeline: repo.branches_with_pipeline.contains(&pr.base),
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
}
