//! Topics a client subscribes to, and what the daemon sends on them: a
//! snapshot, then deltas with sequence numbers (ADR 0010).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::inbox::InboxUpdate;
use crate::logs::{LogKey, LogRecord, StorageWarning};
use crate::runs::{RunEvent, RunId, RunSummary};

/// A topic, by its name on the wire: `watched_prs`, `inbox`, `run/<id>` or
/// `log/<run>/<step>/<attempt>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Topic {
    /// Added repos, the developer's open PRs in them, and which are watched.
    WatchedPrs,
    /// The open Escalations, oldest first.
    Inbox,
    /// One Run's event journal.
    Run(RunId),
    /// One attempt's Step log, as it's written.
    StepLog(LogKey),
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Topic::WatchedPrs => f.write_str("watched_prs"),
            Topic::Inbox => f.write_str("inbox"),
            Topic::Run(id) => write!(f, "run/{id}"),
            Topic::StepLog(key) => write!(f, "log/{key}"),
        }
    }
}

impl TryFrom<String> for Topic {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        match text.as_str() {
            "watched_prs" => return Ok(Topic::WatchedPrs),
            "inbox" => return Ok(Topic::Inbox),
            _ => {}
        }
        if let Some(key) = text.strip_prefix("log/").and_then(LogKey::parse) {
            return Ok(Topic::StepLog(key));
        }
        text.strip_prefix("run/")
            .and_then(|id| id.parse().ok())
            .map(|id| Topic::Run(RunId(id)))
            .ok_or_else(|| format!("`{text}` isn't a topic"))
    }
}

impl From<Topic> for String {
    fn from(topic: Topic) -> Self {
        topic.to_string()
    }
}

/// One frame on a topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "topic", rename_all = "snake_case")]
pub enum TopicUpdate {
    WatchedPrs {
        /// A snapshot carries the sequence number of the last delta it
        /// includes. Each delta after it carries the next number.
        seq: u64,
        update: WatchedPrsUpdate,
    },
    /// Like `watched_prs`: a snapshot, then deltas.
    Inbox { seq: u64, update: InboxUpdate },
    /// One event from a Run's journal. A Run topic has no snapshot: its
    /// events from sequence number 1 are the whole Run.
    Run {
        id: RunId,
        seq: u64,
        /// When the daemon journalled the event, in milliseconds since the
        /// Unix epoch. 0 for events journalled before timestamps existed.
        #[serde(default)]
        ts: i64,
        event: RunEvent,
    },
    /// Records just written to a Step log. A log topic has no snapshot:
    /// subscribing sends the latest records, then each new one. A gap in
    /// sequence numbers means records the client didn't get, which it can
    /// page in with `read_step_log`.
    StepLog {
        key: LogKey,
        records: Vec<LogRecord>,
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
    /// Set while Step logs and journals the daemon must keep take more
    /// room than the storage cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageWarning>,
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
            WatchedPrsDelta::Storage { warning } => self.storage = warning,
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
    /// The storage warning came or went.
    Storage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        warning: Option<StorageWarning>,
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
    /// The PR's latest Runs, newest first.
    #[serde(default)]
    pub runs: Vec<RunSummary>,
    /// Why the PR can't get a Run right now, such as an invalid Pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
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
    /// Watched, and its base branch has a Pipeline, so a Run can start.
    Ready,
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
            runs: vec![],
            blocked: None,
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
    fn topics_are_named_the_way_adr_0010_writes_them() {
        assert_eq!(
            serde_json::to_value(Topic::WatchedPrs).unwrap(),
            json!("watched_prs")
        );
        assert_eq!(
            serde_json::to_value(Topic::Run(RunId(12))).unwrap(),
            json!("run/12")
        );
        assert_eq!(
            serde_json::from_value::<Topic>(json!("run/12")).unwrap(),
            Topic::Run(RunId(12))
        );
        assert!(serde_json::from_value::<Topic>(json!("run/x")).is_err());
        let log = Topic::StepLog(LogKey {
            run: RunId(12),
            step: "ci".into(),
            attempt: 1,
        });
        assert_eq!(serde_json::to_value(&log).unwrap(), json!("log/12/ci/1"));
        assert_eq!(
            serde_json::from_value::<Topic>(json!("log/12/ci/1")).unwrap(),
            log
        );
        assert_eq!(
            serde_json::from_value::<Topic>(json!("inbox")).unwrap(),
            Topic::Inbox
        );
        assert!(serde_json::from_value::<Topic>(json!("outbox")).is_err());
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
