//! The Inbox: every open Escalation across repos, oldest first, on the
//! `inbox` topic as a snapshot, then deltas with sequence numbers. Each
//! Escalation belongs to a Run, a Watched PR or a cause shared across PRs,
//! and that scope decides what closes it. A closed entry leaves the Inbox
//! and stays in the record of every Run it touched, through the Run's
//! `inbox` events.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Actor, RepoName, RunId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntryId(pub u64);

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One Escalation, open or closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEntry {
    /// Ids grow in the order entries open, so they sort oldest first.
    pub id: EntryId,
    pub scope: Scope,
    /// One line, such as "Not shippable".
    pub title: String,
    /// What the developer needs to know, one line each.
    pub reasons: Vec<String>,
    /// The PRs it holds back: the one PR of a Run or PR entry, every
    /// affected PR of a cause.
    pub prs: Vec<PrRef>,
    /// Seconds since the Unix epoch.
    pub raised_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Closed>,
}

impl InboxEntry {
    /// Only a PR entry can be dismissed.
    pub fn dismissable(&self) -> bool {
        matches!(self.scope, Scope::Pr)
    }

    pub fn holds(&self, repo: &RepoName, number: u64) -> bool {
        self.prs
            .iter()
            .any(|pr| &pr.repo == repo && pr.number == number)
    }
}

/// What an Escalation belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    /// A problem mid-Run, such as a Step error or a stall. It closes when
    /// answered, such as by retrying the Step, or when the Run ends.
    Run {
        run: RunId,
        /// The Step it's about, if it's about one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
    },
    /// A Run that ended needing the developer, such as not shippable. A PR
    /// has at most one open, and it closes when the PR's next Run starts,
    /// when the PR stops being watched, or when the developer dismisses it.
    Pr,
    /// A cause that holds back every PR it lists. It closes when the daemon
    /// sees it cleared, which starts a Run for every PR it held back.
    Cause { cause: Cause },
}

/// A cause shared across PRs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum Cause {
    /// The Pipeline on `base` doesn't load, such as one that uses a
    /// missing Library Step.
    InvalidPipeline { repo: RepoName, base: String },
    /// No value is set for the Secret `name`, which a Step requires. It
    /// clears once the developer sets it.
    MissingSecret { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PrRef {
    pub repo: RepoName,
    pub number: u64,
}

impl fmt::Display for PrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    /// Seconds since the Unix epoch.
    pub at: i64,
    pub how: Closing,
}

/// How an entry closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum Closing {
    /// The developer acted on it, such as retrying the errored Step.
    Answered {
        /// What they did, such as "retry `ci`".
        action: String,
        actor: Actor,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Dismissed {
        actor: Actor,
    },
    RunEnded,
    NextRunStarted,
    /// The PR was unwatched or closed. A cause closes this way once every
    /// PR it held left.
    LeftWatched,
    CauseCleared,
    /// The cause may still be there, but no PR it held hits it any more,
    /// say because the PR moved to another base.
    NothingHeld,
}

impl fmt::Display for Closing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Closing::Answered { action, .. } => write!(f, "answered: {action}"),
            Closing::Dismissed { .. } => f.write_str("dismissed"),
            Closing::RunEnded => f.write_str("the Run ended"),
            Closing::NextRunStarted => f.write_str("the next Run started"),
            Closing::LeftWatched => f.write_str("the PR left Watched"),
            Closing::CauseCleared => f.write_str("the cause cleared"),
            Closing::NothingHeld => f.write_str("no PR hits the cause any more"),
        }
    }
}

/// Everything on the `inbox` topic: the open entries, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inbox {
    pub entries: Vec<InboxEntry>,
}

impl Inbox {
    /// Applies one delta, the way the daemon applied it to its own copy.
    pub fn apply(&mut self, delta: InboxDelta) {
        let InboxDelta::Put { entry } = delta;
        let entry = *entry;
        let found = self.entries.binary_search_by(|open| open.id.cmp(&entry.id));
        match (found, entry.closed.is_some()) {
            (Ok(index), true) => {
                self.entries.remove(index);
            }
            (Err(_), true) => {}
            (Ok(index), false) => self.entries[index] = entry,
            (Err(index), false) => self.entries.insert(index, entry),
        }
    }

    /// What the Dock badge counts. A cause counts once, however many PRs
    /// it holds back.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// The open entries that hold back `repo#number`, oldest first.
    pub fn for_pr(&self, repo: &RepoName, number: u64) -> impl Iterator<Item = &InboxEntry> {
        let repo = repo.clone();
        self.entries
            .iter()
            .filter(move |entry| entry.holds(&repo, number))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxUpdate {
    Snapshot(Inbox),
    Delta(InboxDelta),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboxDelta {
    /// An entry opened or changed, such as a cause holding back one more
    /// PR. One that carries `closed` leaves the Inbox.
    Put { entry: Box<InboxEntry> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: u64, scope: Scope) -> InboxEntry {
        InboxEntry {
            id: EntryId(id),
            scope,
            title: "Not shippable".into(),
            reasons: vec!["`ci` failed".into()],
            prs: vec![PrRef {
                repo: RepoName::new("o", "r"),
                number: 7,
            }],
            raised_at: 100,
            closed: None,
        }
    }

    #[test]
    fn an_entry_names_its_scope_and_how_it_closed_on_the_wire() {
        let mut closed = entry(3, Scope::Pr);
        closed.closed = Some(Closed {
            at: 200,
            how: Closing::Dismissed {
                actor: Actor::Developer { via: "gui".into() },
            },
        });

        assert_eq!(
            serde_json::to_value(&closed).unwrap(),
            json!({
                "id": 3,
                "scope": { "kind": "pr" },
                "title": "Not shippable",
                "reasons": ["`ci` failed"],
                "prs": [{ "repo": "o/r", "number": 7 }],
                "raised_at": 100,
                "closed": {
                    "at": 200,
                    "how": { "how": "dismissed", "actor": { "kind": "developer", "via": "gui" } },
                },
            })
        );
        let cause = Scope::Cause {
            cause: Cause::InvalidPipeline {
                repo: RepoName::new("o", "r"),
                base: "main".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&cause).unwrap(),
            json!({ "kind": "cause", "cause": { "cause": "invalid_pipeline", "repo": "o/r", "base": "main" } })
        );
        let secret = Scope::Cause {
            cause: Cause::MissingSecret {
                name: "JEV_API_KEY".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(&secret).unwrap(),
            json!({ "kind": "cause", "cause": { "cause": "missing_secret", "name": "JEV_API_KEY" } })
        );
        assert_eq!(
            serde_json::to_value(Scope::Run {
                run: RunId(4),
                step: Some("ci".into())
            })
            .unwrap(),
            json!({ "kind": "run", "run": 4, "step": "ci" })
        );
    }

    #[test]
    fn deltas_keep_open_entries_oldest_first_and_a_closed_one_leaves() {
        let mut inbox = Inbox::default();
        inbox.apply(InboxDelta::Put {
            entry: Box::new(entry(5, Scope::Pr)),
        });
        inbox.apply(InboxDelta::Put {
            entry: Box::new(entry(2, Scope::Pr)),
        });
        let mut changed = entry(5, Scope::Pr);
        changed.reasons.push("`lint` failed".into());
        inbox.apply(InboxDelta::Put {
            entry: Box::new(changed),
        });

        assert_eq!(
            inbox.entries.iter().map(|e| e.id.0).collect::<Vec<_>>(),
            [2, 5]
        );
        assert_eq!(inbox.entries[1].reasons.len(), 2);

        let mut closed = entry(2, Scope::Pr);
        closed.closed = Some(Closed {
            at: 1,
            how: Closing::NextRunStarted,
        });
        inbox.apply(InboxDelta::Put {
            entry: Box::new(closed),
        });

        assert_eq!(inbox.count(), 1);
        assert_eq!(inbox.for_pr(&RepoName::new("o", "r"), 7).count(), 1);
        assert_eq!(inbox.for_pr(&RepoName::new("o", "r"), 8).count(), 0);
    }

    #[test]
    fn only_a_pr_entry_can_be_dismissed() {
        assert!(entry(1, Scope::Pr).dismissable());
        assert!(
            !entry(
                1,
                Scope::Run {
                    run: RunId(1),
                    step: None
                }
            )
            .dismissable()
        );
    }
}
