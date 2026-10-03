//! What the main window knows about repos and PRs, kept from the
//! `watched_prs` topic, and what it shows for them. No GPUI here, so it
//! tests without a window.

use slopwatch_protocol::{
    PollState, PrStatus, PullRequest, RepoName, TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

/// The sources pane's selection: every PR, or one repo's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    All,
    Repo(RepoName),
}

#[derive(Debug, Default)]
pub struct Prs {
    topic: WatchedPrs,
    /// The sequence number of the last update applied. `None` until the
    /// first snapshot.
    seq: Option<u64>,
    pub source: Source,
}

impl Prs {
    /// Applies an update from the daemon. A snapshot replaces everything,
    /// and a delta the snapshot already includes is dropped.
    pub fn apply(&mut self, update: TopicUpdate) {
        let TopicUpdate::WatchedPrs { seq, update } = update else {
            return;
        };
        match update {
            WatchedPrsUpdate::Snapshot(snapshot) => {
                self.topic = snapshot;
                if let Source::Repo(repo) = &self.source
                    && !self.topic.repos.contains(repo)
                {
                    self.source = Source::All;
                }
            }
            WatchedPrsUpdate::Delta(delta) => {
                let Some(last) = self.seq else { return };
                if seq <= last {
                    return;
                }
                self.topic.apply(delta);
            }
        }
        self.seq = Some(seq);
    }

    pub fn pr(&self, repo: &RepoName, number: u64) -> Option<&PullRequest> {
        self.topic.pr(repo, number)
    }

    pub fn loaded(&self) -> bool {
        self.seq.is_some()
    }

    pub fn repos(&self) -> &[RepoName] {
        &self.topic.repos
    }

    pub fn poll(&self) -> &PollState {
        &self.topic.poll
    }

    /// The rows the PR list shows for the selected source.
    pub fn rows(&self) -> impl Iterator<Item = &PullRequest> {
        self.topic.prs.iter().filter(|pr| match &self.source {
            Source::All => true,
            Source::Repo(repo) => &pr.repo == repo,
        })
    }

    /// How many watched PRs a repo has, for its sources entry.
    pub fn watched_in(&self, repo: &RepoName) -> usize {
        self.topic
            .prs
            .iter()
            .filter(|pr| &pr.repo == repo && pr.watched())
            .count()
    }
}

/// The status line under a PR's title.
pub fn status_line(pr: &PullRequest) -> String {
    let status = match pr.status {
        PrStatus::NotWatched => "Not watched".to_owned(),
        PrStatus::Waiting => format!("Waiting for a Pipeline on {}", pr.base),
        PrStatus::Ready => match (&pr.blocked, pr.runs.first()) {
            (Some(blocked), _) => blocked.clone(),
            (None, Some(run)) => crate::run_pane::run_label(run),
            (None, None) => "Watched".to_owned(),
        },
    };
    if pr.draft {
        format!("Draft · {status}")
    } else {
        status
    }
}

/// What the footer says about the daemon's last poll, if anything.
pub fn poll_line(poll: &PollState) -> Option<String> {
    match poll {
        PollState::Pending => Some("Checking GitHub…".to_owned()),
        PollState::Online => None,
        PollState::Offline { message } => Some(format!("Offline: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopwatch_protocol::WatchedPrsDelta;

    fn repo(name: &str) -> RepoName {
        RepoName::new("o", name)
    }

    fn pr(repo_name: &str, number: u64, status: PrStatus) -> PullRequest {
        PullRequest {
            repo: repo(repo_name),
            number,
            title: format!("PR {number}"),
            url: String::new(),
            draft: false,
            head_sha: String::new(),
            base: "main".into(),
            status,
            runs: vec![],
            blocked: None,
        }
    }

    fn snapshot(seq: u64, repos: &[&str], prs: Vec<PullRequest>) -> TopicUpdate {
        TopicUpdate::WatchedPrs {
            seq,
            update: WatchedPrsUpdate::Snapshot(WatchedPrs {
                repos: repos.iter().map(|name| repo(name)).collect(),
                prs,
                poll: PollState::Online,
            }),
        }
    }

    fn delta(seq: u64, delta: WatchedPrsDelta) -> TopicUpdate {
        TopicUpdate::WatchedPrs {
            seq,
            update: WatchedPrsUpdate::Delta(delta),
        }
    }

    fn numbers(prs: &Prs) -> Vec<u64> {
        prs.rows().map(|pr| pr.number).collect()
    }

    #[test]
    fn a_repo_source_shows_only_that_repos_prs() {
        let mut prs = Prs::default();
        prs.apply(snapshot(
            3,
            &["a", "b"],
            vec![
                pr("a", 1, PrStatus::Waiting),
                pr("b", 2, PrStatus::NotWatched),
                pr("b", 5, PrStatus::Ready),
            ],
        ));

        assert_eq!(numbers(&prs), [1, 2, 5]);

        prs.source = Source::Repo(repo("b"));

        assert_eq!(numbers(&prs), [2, 5]);
        assert_eq!(prs.watched_in(&repo("b")), 1);
    }

    #[test]
    fn deltas_the_snapshot_already_has_are_dropped() {
        let mut prs = Prs::default();
        prs.apply(delta(
            1,
            WatchedPrsDelta::PrChanged {
                pr: pr("a", 9, PrStatus::Waiting),
            },
        ));
        assert!(!prs.loaded(), "a delta before any snapshot means nothing");

        prs.apply(snapshot(5, &["a"], vec![pr("a", 1, PrStatus::NotWatched)]));
        prs.apply(delta(
            4,
            WatchedPrsDelta::PrGone {
                repo: repo("a"),
                number: 1,
            },
        ));
        prs.apply(delta(
            6,
            WatchedPrsDelta::PrChanged {
                pr: pr("a", 2, PrStatus::Waiting),
            },
        ));

        assert_eq!(numbers(&prs), [1, 2]);
    }

    #[test]
    fn a_fresh_snapshot_replaces_everything_and_drops_a_vanished_source() {
        let mut prs = Prs::default();
        prs.apply(snapshot(2, &["a"], vec![pr("a", 1, PrStatus::Waiting)]));
        prs.source = Source::Repo(repo("a"));

        prs.apply(snapshot(0, &["b"], vec![pr("b", 7, PrStatus::Waiting)]));

        assert_eq!(prs.source, Source::All);
        assert_eq!(numbers(&prs), [7]);
    }

    #[test]
    fn a_watched_pr_without_a_pipeline_says_it_is_waiting() {
        let mut waiting = pr("a", 1, PrStatus::Waiting);
        waiting.base = "release".into();

        assert_eq!(status_line(&waiting), "Waiting for a Pipeline on release");

        waiting.draft = true;
        assert_eq!(
            status_line(&waiting),
            "Draft · Waiting for a Pipeline on release"
        );
        assert_eq!(
            status_line(&pr("a", 1, PrStatus::NotWatched)),
            "Not watched"
        );
    }

    #[test]
    fn the_footer_mentions_the_poll_only_when_it_isnt_online() {
        assert_eq!(poll_line(&PollState::Online), None);
        assert_eq!(
            poll_line(&PollState::Offline {
                message: "no network".into()
            })
            .as_deref(),
            Some("Offline: no network")
        );
    }
}
