//! Topics a client subscribes to, and what the daemon sends on them: a
//! snapshot, then deltas with sequence numbers (ADR 0010).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// Added repos, the developer's open PRs in them, and which are watched.
    WatchedPrs,
}

/// One frame on a topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "topic", rename_all = "snake_case")]
pub enum TopicUpdate {
    WatchedPrs {
        /// A snapshot carries the sequence number of the last delta it
        /// includes. Each delta after it carries the next number.
        seq: u64,
        update: WatchedPrsUpdate,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchedPrsUpdate {
    Snapshot(WatchedPrs),
    Delta(WatchedPrsDelta),
}

/// A GitHub repo, `owner/name` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoName {
    pub owner: String,
    pub name: String,
}

impl RepoName {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
        }
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl FromStr for RepoName {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.split_once('/') {
            Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
                Ok(Self::new(owner, name))
            }
            _ => Err(format!("`{text}` isn't an owner/name repo")),
        }
    }
}

impl TryFrom<String> for RepoName {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<RepoName> for String {
    fn from(repo: RepoName) -> Self {
        repo.to_string()
    }
}

/// Everything on the `watched_prs` topic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchedPrs {
    /// Added repos, in the order they were added.
    pub repos: Vec<RepoName>,
    /// The developer's open PRs in those repos, watched or not, sorted by
    /// repo then number.
    pub prs: Vec<PullRequest>,
    pub poll: PollState,
}

impl WatchedPrs {
    /// Applies one delta, the way the daemon applied it to its own copy.
    pub fn apply(&mut self, delta: WatchedPrsDelta) {
        match delta {
            WatchedPrsDelta::RepoAdded { repo } => {
                if !self.repos.contains(&repo) {
                    self.repos.push(repo);
                }
            }
            WatchedPrsDelta::PrChanged { pr } => {
                match self.prs.binary_search_by(|row| row.key().cmp(&pr.key())) {
                    Ok(index) => self.prs[index] = pr,
                    Err(index) => self.prs.insert(index, pr),
                }
            }
            WatchedPrsDelta::PrGone { repo, number } => {
                self.prs
                    .retain(|row| !(row.repo == repo && row.number == number));
            }
            WatchedPrsDelta::Poll { state } => self.poll = state,
        }
    }

    pub fn pr(&self, repo: &RepoName, number: u64) -> Option<&PullRequest> {
        self.prs
            .iter()
            .find(|row| &row.repo == repo && row.number == number)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WatchedPrsDelta {
    RepoAdded {
        repo: RepoName,
    },
    /// A PR showed up or changed. It replaces any row with its repo and
    /// number.
    PrChanged {
        pr: PullRequest,
    },
    /// A PR closed, merged, or otherwise left the developer's open PRs.
    PrGone {
        repo: RepoName,
        number: u64,
    },
    Poll {
        state: PollState,
    },
}

/// One of the developer's open PRs in an added repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub repo: RepoName,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub draft: bool,
    pub head_sha: String,
    /// The base branch's name.
    pub base: String,
    pub status: PrStatus,
}

impl PullRequest {
    fn key(&self) -> (&RepoName, u64) {
        (&self.repo, self.number)
    }

    pub fn watched(&self) -> bool {
        self.status != PrStatus::NotWatched
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    /// The PR doesn't carry the `slopwatch` label.
    NotWatched,
    /// Watched, but its base branch has no Pipeline yet. Its first Run
    /// starts once one lands.
    Waiting,
    /// Watched, and its base branch has a Pipeline.
    Watched,
}

/// How the daemon's last poll of GitHub went.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PollState {
    /// No poll has finished since the daemon started.
    #[default]
    Pending,
    Online,
    /// The last poll failed. The daemon keeps trying.
    Offline {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pr(repo: &str, number: u64, title: &str) -> PullRequest {
        PullRequest {
            repo: repo.parse().unwrap(),
            number,
            title: title.into(),
            url: format!("https://github.com/{repo}/pull/{number}"),
            draft: false,
            head_sha: "abc".into(),
            base: "main".into(),
            status: PrStatus::NotWatched,
        }
    }

    #[test]
    fn a_repo_name_is_owner_slash_name_on_the_wire() {
        let repo = RepoName::new("jnsdls", "slopwatch");

        assert_eq!(
            serde_json::to_value(&repo).unwrap(),
            json!("jnsdls/slopwatch")
        );
        assert_eq!(
            serde_json::from_value::<RepoName>(json!("jnsdls/slopwatch")).unwrap(),
            repo
        );
        assert!(serde_json::from_value::<RepoName>(json!("slopwatch")).is_err());
        assert!(serde_json::from_value::<RepoName>(json!("a/b/c")).is_err());
    }

    #[test]
    fn a_topic_update_names_its_topic_and_sequence_number() {
        let update = TopicUpdate::WatchedPrs {
            seq: 4,
            update: WatchedPrsUpdate::Delta(WatchedPrsDelta::PrGone {
                repo: RepoName::new("o", "r"),
                number: 3,
            }),
        };

        assert_eq!(
            serde_json::to_value(&update).unwrap(),
            json!({
                "topic": "watched_prs",
                "seq": 4,
                "update": { "delta": { "kind": "pr_gone", "repo": "o/r", "number": 3 } },
            })
        );
    }

    #[test]
    fn deltas_keep_prs_sorted_by_repo_then_number() {
        let mut state = WatchedPrs::default();

        state.apply(WatchedPrsDelta::PrChanged {
            pr: pr("o/b", 2, "two"),
        });
        state.apply(WatchedPrsDelta::PrChanged {
            pr: pr("o/a", 9, "nine"),
        });
        state.apply(WatchedPrsDelta::PrChanged {
            pr: pr("o/b", 1, "one"),
        });
        state.apply(WatchedPrsDelta::PrChanged {
            pr: pr("o/b", 2, "two, renamed"),
        });

        let rows: Vec<_> = state
            .prs
            .iter()
            .map(|row| (row.repo.to_string(), row.number, row.title.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("o/a".to_owned(), 9, "nine"),
                ("o/b".to_owned(), 1, "one"),
                ("o/b".to_owned(), 2, "two, renamed"),
            ]
        );

        state.apply(WatchedPrsDelta::PrGone {
            repo: "o/b".parse().unwrap(),
            number: 1,
        });
        assert_eq!(state.prs.len(), 2);
        assert!(state.pr(&"o/b".parse().unwrap(), 1).is_none());
    }

    #[test]
    fn adding_a_repo_twice_lists_it_once() {
        let mut state = WatchedPrs::default();
        let repo = RepoName::new("o", "r");

        state.apply(WatchedPrsDelta::RepoAdded { repo: repo.clone() });
        state.apply(WatchedPrsDelta::RepoAdded { repo: repo.clone() });

        assert_eq!(state.repos, [repo]);
    }
}
