//! Added repos and the developer's open PRs in them, kept in step with
//! GitHub by polling. The `slopwatch` label on GitHub is what makes a PR
//! watched, so watching from the app and labelling on github.com are the
//! same change, and the next poll picks up either.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use slopwatch_protocol::{PollState, PrStatus, PullRequest, RepoName, WatchedPrs, WatchedPrsDelta};
use tokio::sync::{Notify, broadcast};

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
    poll_now: Notify,
}

struct State {
    store: Store,
    repos: Vec<RepoName>,
    prs: BTreeMap<(RepoName, u64), OpenPr>,
    poll: PollState,
    seq: u64,
    deltas: broadcast::Sender<(u64, WatchedPrsDelta)>,
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
                poll: PollState::Pending,
                seq: 0,
                deltas,
            }),
            github_turn: tokio::sync::Mutex::new(()),
            pace: Mutex::new(Pace::default()),
            poll_now: Notify::new(),
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

        let now = Instant::now();
        let mut state = self.state();
        for Poll { repos, rate } in polls {
            if let Some(rate) = rate {
                self.pace().record(now, rate);
            }
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

    /// Wakes the poll loop for a poll right away, such as after the Mac
    /// wakes.
    pub fn poll_soon(&self) {
        self.poll_now.notify_one();
    }

    /// Polls forever, at the pace the rate budget allows.
    pub async fn run(self: Arc<Self>) {
        loop {
            if let Err(error) = self.poll().await {
                eprintln!("slopwatchd: poll failed: {error:?}");
            }
            // No Runs exist yet, so nothing is ever live.
            let next = self.pace().next_poll(Instant::now(), false);
            tokio::select! {
                () = tokio::time::sleep_until(next.into()) => {}
                () = self.poll_now.notified() => {}
            }
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
                .map(|((repo, _), pr)| row(repo, pr))
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

    fn put(&mut self, repo: &RepoName, pr: OpenPr) -> Result<(), StoreError> {
        let key = (repo.clone(), pr.number);
        if self.prs.get(&key) == Some(&pr) {
            return Ok(());
        }
        self.store.put_pr(repo, &pr)?;
        let changed = row(repo, &pr);
        self.prs.insert(key, pr);
        self.publish(WatchedPrsDelta::PrChanged { pr: changed });
        Ok(())
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

fn row(repo: &RepoName, pr: &OpenPr) -> PullRequest {
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
            (true, true) => PrStatus::Watched,
        },
    }
}
