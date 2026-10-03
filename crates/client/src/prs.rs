//! What the main window knows about repos and PRs, kept from the
//! `watched_prs` topic, and what it shows for them. No GPUI here, so it
//! tests without a window.

use std::collections::{BTreeMap, HashSet};

use slopwatch_protocol::{
    PollState, PrStatus, PullRequest, RepoName, StackParent, StorageWarning, TopicUpdate,
    WatchedPrs, WatchedPrsUpdate,
};

/// One row of the PR list. A Stack shows as a tree, each PR one level
/// under its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row<'a> {
    Pr {
        pr: &'a PullRequest,
        depth: usize,
        /// Another listed PR is stacked on it.
        parent: bool,
    },
    /// A Stack parent that isn't one of the developer's open PRs, which
    /// the list knows only by what its child says.
    Parent {
        repo: &'a RepoName,
        parent: &'a StackParent,
        depth: usize,
    },
}

impl Row<'_> {
    pub fn depth(&self) -> usize {
        match self {
            Row::Pr { depth, .. } | Row::Parent { depth, .. } => *depth,
        }
    }

    /// Whether clicking the row opens it. An unwatched parent shows dim
    /// and doesn't open: it has no Runs to show.
    pub fn selectable(&self) -> bool {
        match self {
            Row::Pr { pr, parent, .. } => pr.watched() || !parent,
            Row::Parent { .. } => false,
        }
    }
}

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

    /// Every open PR of `repo`, whichever source is showing.
    pub fn in_repo(&self, repo: &RepoName) -> Vec<PullRequest> {
        let prs = self.topic.prs.iter().filter(|pr| &pr.repo == repo);
        prs.cloned().collect()
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

    /// The "storage over cap" daemon warning, while it stands.
    pub fn storage(&self) -> Option<StorageWarning> {
        self.topic.storage
    }

    /// The PRs of the selected source, by repo then number.
    pub fn prs(&self) -> impl Iterator<Item = &PullRequest> {
        self.topic.prs.iter().filter(|pr| match &self.source {
            Source::All => true,
            Source::Repo(repo) => &pr.repo == repo,
        })
    }

    /// The rows the PR list shows for the selected source: each Stack as a
    /// tree in stack order, its PRs under their parents, and a parent that
    /// isn't one of the developer's PRs as a row of its own.
    pub fn rows(&self) -> Vec<Row<'_>> {
        type Key<'a> = (&'a RepoName, u64);
        let prs: Vec<&PullRequest> = self.prs().collect();
        let listed: HashSet<Key> = prs.iter().map(|pr| (&pr.repo, pr.number)).collect();
        let mut children: BTreeMap<Key, Vec<&PullRequest>> = BTreeMap::new();
        // Parents nobody lists, by repo and number, with the first child's
        // word on them.
        let mut unlisted: BTreeMap<Key, &StackParent> = BTreeMap::new();
        for pr in &prs {
            if let Some(stack) = &pr.stack {
                let parent = (&pr.repo, stack.parent.number);
                children.entry(parent).or_default().push(pr);
                if !listed.contains(&parent) {
                    unlisted.entry(parent).or_insert(&stack.parent);
                }
            }
        }
        for siblings in children.values_mut() {
            siblings.sort_by_key(|pr| {
                let position = pr.stack.as_ref().and_then(|stack| stack.position);
                (position.unwrap_or(u32::MAX), pr.number)
            });
        }

        // Roots: unstacked PRs and unlisted parents, by repo then number.
        let mut roots: Vec<(Key, Option<&PullRequest>)> = prs
            .iter()
            .filter(|pr| pr.stack.is_none())
            .map(|pr| ((&pr.repo, pr.number), Some(*pr)))
            .chain(unlisted.keys().map(|&key| (key, None)))
            .collect();
        roots.sort_by_key(|(key, _)| *key);

        let mut rows = Vec::new();
        let mut shown: HashSet<Key> = HashSet::new();
        let mut todo: Vec<(Key, Option<&PullRequest>, usize)> = roots
            .into_iter()
            .rev()
            .map(|(key, pr)| (key, pr, 0))
            .collect();
        loop {
            while let Some((key, pr, depth)) = todo.pop() {
                if !shown.insert(key) {
                    continue;
                }
                let below = children.get(&key);
                rows.push(match pr {
                    Some(pr) => Row::Pr {
                        pr,
                        depth,
                        parent: below.is_some(),
                    },
                    None => Row::Parent {
                        repo: key.0,
                        parent: unlisted[&key],
                        depth,
                    },
                });
                for child in below.into_iter().flatten().rev() {
                    todo.push(((&child.repo, child.number), Some(child), depth + 1));
                }
            }
            // A cycle of bases, which a poll that caught a retarget halfway
            // could show, has no root. Its PRs still get rows.
            match prs
                .iter()
                .find(|pr| !shown.contains(&(&pr.repo, pr.number)))
            {
                Some(pr) => todo.push(((&pr.repo, pr.number), Some(*pr), 0)),
                None => return rows,
            }
        }
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
        PrStatus::Waiting => format!("Waiting for a Pipeline on {}", pr.root_base()),
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

/// What the sources pane says about the storage warning.
pub fn storage_line(warning: StorageWarning) -> String {
    format!(
        "Storage over cap: Step logs the daemon must keep take {}, over the {} cap",
        size(warning.used_bytes),
        size(warning.cap_bytes)
    )
}

/// A byte count the way people read it.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
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
            stack: None,
        }
    }

    fn snapshot(seq: u64, repos: &[&str], prs: Vec<PullRequest>) -> TopicUpdate {
        TopicUpdate::WatchedPrs {
            seq,
            update: WatchedPrsUpdate::Snapshot(WatchedPrs {
                repos: repos.iter().map(|name| repo(name)).collect(),
                prs,
                poll: PollState::Online,
                storage: None,
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
        prs.prs().map(|pr| pr.number).collect()
    }

    /// PR `number` stacked on `parent`, at `position` in a native stack.
    fn stacked(number: u64, parent: u64, position: Option<u32>) -> PullRequest {
        PullRequest {
            base: format!("pr-{parent}"),
            stack: Some(Box::new(slopwatch_protocol::StackPlace {
                parent: StackParent {
                    number: parent,
                    title: format!("PR {parent}"),
                    url: String::new(),
                },
                position,
                root_base: "main".into(),
            })),
            ..pr("a", number, PrStatus::Ready)
        }
    }

    /// Each row as its number and depth, with `~` marking a parent that
    /// isn't listed and `!` a row that doesn't open.
    fn tree(prs: &Prs) -> Vec<String> {
        prs.rows()
            .iter()
            .map(|row| {
                let mark = if row.selectable() { "" } else { "!" };
                match row {
                    Row::Pr { pr, depth, .. } => format!("{depth}:{}{mark}", pr.number),
                    Row::Parent { parent, depth, .. } => {
                        format!("{depth}:~{}{mark}", parent.number)
                    }
                }
            })
            .collect()
    }

    #[test]
    fn a_stack_shows_as_a_tree_in_stack_order() {
        let mut prs = Prs::default();
        prs.apply(snapshot(
            1,
            &["a"],
            vec![
                pr("a", 1, PrStatus::Ready),
                stacked(2, 1, None),
                stacked(3, 2, None),
                pr("a", 4, PrStatus::Ready),
                stacked(5, 1, None),
            ],
        ));

        assert_eq!(tree(&prs), ["0:1", "1:2", "2:3", "1:5", "0:4"]);
    }

    #[test]
    fn a_native_stack_orders_by_position_and_an_unlisted_parent_gets_a_dim_row() {
        let mut prs = Prs::default();
        let mut unwatched = pr("a", 7, PrStatus::NotWatched);
        unwatched.base = "pr-9".into();
        prs.apply(snapshot(
            1,
            &["a"],
            vec![
                // Someone else's #6 is the bottom, so it isn't listed.
                stacked(8, 6, Some(3)),
                stacked(3, 6, Some(2)),
                pr("a", 5, PrStatus::NotWatched),
                stacked(9, 5, None),
            ],
        ));

        assert_eq!(
            tree(&prs),
            ["0:5!", "1:9", "0:~6!", "1:3", "1:8"],
            "an unwatched parent doesn't open, and an unwatched PR on its own does"
        );
        assert!(
            Row::Pr {
                pr: &unwatched,
                depth: 0,
                parent: false
            }
            .selectable()
        );
    }

    #[test]
    fn a_stacked_pr_waits_for_a_pipeline_on_its_root_base() {
        let mut waiting = stacked(2, 1, None);
        waiting.status = PrStatus::Waiting;

        assert_eq!(status_line(&waiting), "Waiting for a Pipeline on main");
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
