//! Added repos and the developer's open PRs in them, kept in step with
//! GitHub by polling. The `slopwatch` label on GitHub is what makes a PR
//! watched, so watching from the app and labelling on github.com are the
//! same change, and the next poll picks up either.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use slopwatch_protocol::{
    PollState, PrStatus, PullRequest, RepoName, RunSummary, WatchedPrs, WatchedPrsDelta,
};
use tokio::sync::broadcast;

use crate::github::{GitHub, GitHubError, OpenPr, Poll, RepoPoll};
use crate::pace::Pace;
use crate::store::{Store, StoreError};

/// Repos per GraphQL query. One query across many repos risks GitHub's
/// 10 s timeout, so a poll sends a few batches one after another.
const REPOS_PER_QUERY: usize = 5;

/// Deltas a slow subscriber may fall behind by before it gets a fresh
/// snapshot instead.
const BACKLOG: usize = 1024;

pub struct Watching {
    github: Arc<dyn GitHub>,
    state: Mutex<State>,
    /// Held across every GitHub call that reads or changes PRs, so a poll
    /// that started before a watch can't undo it with stale labels.
    github_turn: tokio::sync::Mutex<()>,
    pace: Mutex<Pace>,
}

struct State {
    store: Store,
    repos: Vec<RepoName>,
    prs: BTreeMap<(RepoName, u64), OpenPr>,
    /// What Runs report for each PR, shown on its row.
    runs: BTreeMap<(RepoName, u64), RunInfo>,
    poll: PollState,
    seq: u64,
    deltas: broadcast::Sender<(u64, WatchedPrsDelta)>,
}

/// A PR's Run history and what holds its next Run back, for its row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunInfo {
    /// Newest first.
    pub runs: Vec<RunSummary>,
    pub blocked: Option<String>,
}

/// A subscriber's view of the topic: the state as of `seq`, then every
/// delta after it.
pub struct Subscription {
    pub seq: u64,
    pub snapshot: WatchedPrs,
    pub deltas: broadcast::Receiver<(u64, WatchedPrsDelta)>,
}

#[derive(Debug)]
pub enum WatchError {
    NotFound(String),
    GitHub(GitHubError),
    Store(StoreError),
}

impl From<GitHubError> for WatchError {
    fn from(error: GitHubError) -> Self {
        match error {
            GitHubError::NotFound(what) => WatchError::NotFound(what),
            other => WatchError::GitHub(other),
        }
    }
}

impl From<StoreError> for WatchError {
    fn from(error: StoreError) -> Self {
        WatchError::Store(error)
    }
}

impl Watching {
    /// Loads what `store` holds. Nothing reaches GitHub until a poll.
    pub fn new(store: Store, github: Arc<dyn GitHub>) -> Result<Self, StoreError> {
        let repos = store.repos()?;
        let prs = store
            .prs()?
            .into_iter()
            .map(|(repo, pr)| ((repo, pr.number), pr))
            .collect();
        let (deltas, _) = broadcast::channel(BACKLOG);
        Ok(Self {
            github,
            state: Mutex::new(State {
                store,
                repos,
                prs,
                runs: BTreeMap::new(),
                poll: PollState::Pending,
                seq: 0,
                deltas,
            }),
            github_turn: tokio::sync::Mutex::new(()),
            pace: Mutex::new(Pace::default()),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("no panics while holding the state")
    }

    pub fn subscribe(&self) -> Subscription {
        let state = self.state();
        Subscription {
            seq: state.seq,
            snapshot: state.snapshot(),
            deltas: state.deltas.subscribe(),
        }
    }

    pub async fn available_repos(&self) -> Result<Vec<RepoName>, WatchError> {
        Ok(self.github.available_repos().await?)
    }

    /// Adds `repo` and lists its open PRs by the developer right away.
    pub async fn add_repo(&self, repo: RepoName) -> Result<(), WatchError> {
        let _turn = self.github_turn.lock().await;
        let poll = self.github.poll(std::slice::from_ref(&repo)).await?;
        if let Some(rate) = poll.rate {
            self.pace().record(Instant::now(), rate);
        }
        let Some(RepoPoll { prs: Some(prs), .. }) = poll.repos.into_iter().next() else {
            return Err(WatchError::NotFound(repo.to_string()));
        };
        let mut state = self.state();
        if !state.repos.contains(&repo) {
            state.store.add_repo(&repo)?;
            state.repos.push(repo.clone());
            state.publish(WatchedPrsDelta::RepoAdded { repo: repo.clone() });
        }
        state.reconcile(&repo, prs)?;
        Ok(())
    }

    /// Adds or removes the `slopwatch` label on a PR, creating the label
    /// in the repo the first time.
    pub async fn set_watched(
        &self,
        repo: RepoName,
        number: u64,
        watched: bool,
    ) -> Result<(), WatchError> {
        let _turn = self.github_turn.lock().await;
        let label_created = {
            let state = self.state();
            if !state.prs.contains_key(&(repo.clone(), number)) {
                return Err(WatchError::NotFound(format!("{repo}#{number}")));
            }
            state.store.label_created(&repo)?
        };
        if watched && !label_created {
            self.github.create_label(&repo).await?;
            self.state().store.mark_label_created(&repo)?;
        }
        self.github.set_label(&repo, number, watched).await?;

        let mut state = self.state();
        if let Some(mut pr) = state.prs.get(&(repo.clone(), number)).cloned() {
            pr.labeled = watched;
            state.put(&repo, pr)?;
        }
        Ok(())
    }

    /// Polls every added repo once and publishes what changed.
    pub async fn poll(&self) -> Result<(), WatchError> {
        let _turn = self.github_turn.lock().await;
        let repos = self.state().repos.clone();
        let mut polls = Vec::new();
        for batch in repos.chunks(REPOS_PER_QUERY) {
            match self.github.poll(batch).await {
                Ok(poll) => polls.push(poll),
                Err(error) => {
                    self.pace().record_failure(Instant::now(), &error);
                    self.state().set_poll(PollState::Offline {
                        message: error.to_string(),
                    });
                    return Err(error.into());
                }
            }
        }

        let rates: Vec<_> = polls.iter().filter_map(|poll| poll.rate).collect();
        self.pace().record_poll(Instant::now(), &rates);
        let mut state = self.state();
        for Poll { repos, .. } in polls {
            for RepoPoll { repo, prs } in repos {
                // A repo that vanished keeps its last known PRs.
                if let Some(prs) = prs {
                    state.reconcile(&repo, prs)?;
                }
            }
        }
        state.set_poll(PollState::Online);
        Ok(())
    }

    fn pace(&self) -> MutexGuard<'_, Pace> {
        self.pace.lock().expect("no panics while holding the pace")
    }

    /// When to poll next, at the pace the rate budget allows. `live` means
    /// a Run is going.
    pub fn next_poll(&self, live: bool) -> Instant {
        self.pace().next_poll(Instant::now(), live)
    }

    /// Every open PR the daemon knows, with its repo.
    pub fn prs(&self) -> Vec<(RepoName, OpenPr)> {
        self.state()
            .prs
            .iter()
            .map(|((repo, _), pr)| (repo.clone(), pr.clone()))
            .collect()
    }

    /// Shows `info` on the PR's row.
    pub fn set_run_info(&self, repo: &RepoName, number: u64, info: RunInfo) {
        let mut state = self.state();
        let key = (repo.clone(), number);
        if state.runs.get(&key) == Some(&info) {
            return;
        }
        state.runs.insert(key.clone(), info);
        if let Some(pr) = state.prs.get(&key) {
            let changed = state.row(repo, pr);
            state.publish(WatchedPrsDelta::PrChanged { pr: changed });
        }
    }
}

impl State {
    fn snapshot(&self) -> WatchedPrs {
        WatchedPrs {
            repos: self.repos.clone(),
            prs: self
                .prs
                .iter()
                .map(|((repo, _), pr)| self.row(repo, pr))
                .collect(),
            poll: self.poll.clone(),
        }
    }

    fn publish(&mut self, delta: WatchedPrsDelta) {
        self.seq += 1;
        // No subscribers is fine: the next one starts from a snapshot.
        let _ = self.deltas.send((self.seq, delta));
    }

    fn set_poll(&mut self, poll: PollState) {
        if self.poll != poll {
            self.poll = poll.clone();
            self.publish(WatchedPrsDelta::Poll { state: poll });
        }
    }

    /// Stores `pr`, and publishes its row if what the row shows changed. A
    /// poll often changes only detail, such as a check finishing.
    fn put(&mut self, repo: &RepoName, pr: OpenPr) -> Result<(), StoreError> {
        let key = (repo.clone(), pr.number);
        let old = self.prs.get(&key);
        if old == Some(&pr) {
            return Ok(());
        }
        let old_row = old.map(|old| self.row(repo, old));
        self.store.put_pr(repo, &pr)?;
        let changed = self.row(repo, &pr);
        self.prs.insert(key, pr);
        if old_row.as_ref() != Some(&changed) {
            self.publish(WatchedPrsDelta::PrChanged { pr: changed });
        }
        Ok(())
    }

    fn row(&self, repo: &RepoName, pr: &OpenPr) -> PullRequest {
        let info = self
            .runs
            .get(&(repo.clone(), pr.number))
            .cloned()
            .unwrap_or_default();
        PullRequest {
            repo: repo.clone(),
            number: pr.number,
            title: pr.title.clone(),
            url: pr.url.clone(),
            draft: pr.draft,
            head_sha: pr.head_sha.clone(),
            base: pr.base.clone(),
            status: match (pr.labeled, pr.base_has_pipeline) {
                (false, _) => PrStatus::NotWatched,
                (true, false) => PrStatus::Waiting,
                (true, true) => PrStatus::Ready,
            },
            runs: info.runs,
            blocked: info.blocked,
        }
    }

    /// Makes `repo`'s PRs exactly `prs`.
    fn reconcile(&mut self, repo: &RepoName, prs: Vec<OpenPr>) -> Result<(), StoreError> {
        let gone: Vec<u64> = self
            .prs
            .keys()
            .filter(|(r, number)| r == repo && !prs.iter().any(|pr| pr.number == *number))
            .map(|&(_, number)| number)
            .collect();
        for number in gone {
            self.store.remove_pr(repo, number)?;
            self.prs.remove(&(repo.clone(), number));
            self.publish(WatchedPrsDelta::PrGone {
                repo: repo.clone(),
                number,
            });
        }
        for pr in prs {
            self.put(repo, pr)?;
        }
        Ok(())
    }
}
