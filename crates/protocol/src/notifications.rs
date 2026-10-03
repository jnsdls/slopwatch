//! Notifications the daemon decides on and the GUI posts (ADR 0013). The
//! daemon keeps each one until a client acks it, and sends the unacked ones
//! on the `notifications` topic as a snapshot, then deltas with sequence
//! numbers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{EntryId, PrRef, RunId};

/// What a notification is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum About {
    /// A new Inbox entry.
    Entry(EntryId),
    /// A Run that ended shippable.
    Shippable(RunId),
    /// A Run whose Merge Step merged the PR.
    Merged(RunId),
}

/// A notification's id, which the GUI posts it under, so a repost replaces
/// the banner. It names the PR a click opens, so a banner left over from an
/// earlier GUI still finds its PR. On the wire: `inbox:12@owner/name#7`,
/// `shippable:40@owner/name#7` or `merged:40@owner/name#7`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NotificationId {
    pub about: About,
    pub pr: PrRef,
}

impl fmt::Display for NotificationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.about {
            About::Entry(entry) => write!(f, "inbox:{entry}")?,
            About::Shippable(run) => write!(f, "shippable:{run}")?,
            About::Merged(run) => write!(f, "merged:{run}")?,
        }
        write!(f, "@{}", self.pr)
    }
}

impl FromStr for NotificationId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || format!("`{text}` isn't a notification id");
        let (about, pr) = text.split_once('@').ok_or_else(bad)?;
        let (kind, n) = about.split_once(':').ok_or_else(bad)?;
        let n: u64 = n.parse().map_err(|_| bad())?;
        let about = match kind {
            "inbox" => About::Entry(EntryId(n)),
            "shippable" => About::Shippable(RunId(n)),
            "merged" => About::Merged(RunId(n)),
            _ => return Err(bad()),
        };
        let (repo, number) = pr.rsplit_once('#').ok_or_else(bad)?;
        let pr = PrRef {
            repo: repo.parse()?,
            number: number.parse().map_err(|_| bad())?,
        };
        Ok(Self { about, pr })
    }
}

impl TryFrom<String> for NotificationId {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<NotificationId> for String {
    fn from(id: NotificationId) -> Self {
        id.to_string()
    }
}

/// Something the daemon wants the GUI to do in Notification Center.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notification {
    /// Post a banner.
    Post {
        id: NotificationId,
        title: String,
        body: String,
    },
    /// Remove a banner posted earlier, because its Inbox entry closed.
    Retract { id: NotificationId },
}

impl Notification {
    pub fn id(&self) -> &NotificationId {
        match self {
            Notification::Post { id, .. } | Notification::Retract { id } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationsUpdate {
    /// Every notification no client has acked, oldest first.
    Snapshot(Vec<Notification>),
    Delta(NotificationsDelta),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationsDelta {
    /// A notification to act on. One with an id already pending replaces it.
    Put { notification: Notification },
    /// A client acked it, or it went stale before any did, such as a Post
    /// whose Inbox entry closed first. Nobody needs to act on it any more.
    Done { id: NotificationId },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoName;
    use serde_json::json;

    fn pr() -> PrRef {
        PrRef {
            repo: RepoName::new("o", "r"),
            number: 7,
        }
    }

    #[test]
    fn an_id_names_what_it_is_about_and_the_pr_a_click_opens() {
        let entry = NotificationId {
            about: About::Entry(EntryId(12)),
            pr: pr(),
        };
        let shippable = NotificationId {
            about: About::Shippable(RunId(40)),
            pr: pr(),
        };

        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            json!("inbox:12@o/r#7")
        );
        assert_eq!(
            serde_json::to_value(&shippable).unwrap(),
            json!("shippable:40@o/r#7")
        );
        assert_eq!("inbox:12@o/r#7".parse::<NotificationId>().unwrap(), entry);
        assert_eq!(
            "merged:3@o/r#7".parse::<NotificationId>().unwrap().about,
            About::Merged(RunId(3))
        );
        for bad in [
            "inbox:12",
            "inbox:x@o/r#7",
            "later:1@o/r#7",
            "inbox:1@o#7",
            "inbox:1@o/r",
        ] {
            assert!(bad.parse::<NotificationId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn a_post_and_a_retract_on_the_wire() {
        let id = NotificationId {
            about: About::Entry(EntryId(12)),
            pr: pr(),
        };
        let post = Notification::Post {
            id: id.clone(),
            title: "Not shippable".into(),
            body: "o/r#7: `ci` failed".into(),
        };

        assert_eq!(
            serde_json::to_value(&post).unwrap(),
            json!({
                "kind": "post",
                "id": "inbox:12@o/r#7",
                "title": "Not shippable",
                "body": "o/r#7: `ci` failed",
            })
        );
        assert_eq!(
            serde_json::to_value(NotificationsDelta::Done { id: id.clone() }).unwrap(),
            json!({ "kind": "done", "id": "inbox:12@o/r#7" })
        );
        assert_eq!(
            serde_json::to_value(Notification::Retract { id }).unwrap(),
            json!({ "kind": "retract", "id": "inbox:12@o/r#7" })
        );
    }
}
