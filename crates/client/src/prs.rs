//! What the main window knows about repos and PRs, kept from the
//! `watched_prs` topic, and what it shows for them. No GPUI here, so it
//! tests without a window.

use std::collections::{BTreeMap, HashMap, HashSet};

use slopwatch_core::EndReason;
use slopwatch_protocol::{
    InboxEntry, PollState, PrStatus, PullRequest, RepoName, StackParent, StorageWarning,
    TopicUpdate, WatchedPrs, WatchedPrsUpdate,
};

/// One row of the PR list. A Stack shows as one summary row, and once
/// expanded as a tree under it, each PR one level under its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row<'a> {
    Stack(Stack<'a>),
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
            Row::Stack(_) => 0,
            Row::Pr { depth, .. } | Row::Parent { depth, .. } => *depth,
        }
    }

    /// Whether clicking the row opens it. An unwatched parent shows dim
    /// and doesn't open: it has no Runs to show. A Stack's row toggles
    /// instead.
    pub fn selectable(&self) -> bool {
        match self {
            Row::Pr { pr, parent, .. } => pr.watched() || !parent,
            Row::Stack(_) | Row::Parent { .. } => false,
        }
    }
}

impl<'a> Row<'a> {
    /// The PR or parent the row stands for. A Stack's is its bottom.
    fn key(&self) -> Key<'a> {
        match self {
            Row::Stack(stack) => (stack.repo, stack.root),
            Row::Pr { pr, .. } => (&pr.repo, pr.number),
            Row::Parent { repo, parent, .. } => (repo, parent.number),
        }
    }
}

type Key<'a> = (&'a RepoName, u64);

/// A Stack's summary row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stack<'a> {
    pub repo: &'a RepoName,
    /// The bottom PR's number, or the number of the parent below it that
    /// isn't listed.
    pub root: u64,
    /// The base of the bottom PR, whose Pipeline judges the Stack.
    pub root_base: &'a str,
    /// Every PR in the Stack, including parents that aren't the
    /// developer's.
    pub prs: usize,
    pub watched: usize,
    /// How many watched PRs are in each state, indexed by [`PrState`].
    pub states: [usize; PrState::ALL.len()],
    /// The first PR, in stack order, that needs the developer, or failing
    /// that one whose Run failed.
    pub attention: Option<Attention<'a>>,
    pub expanded: bool,
}

impl Stack<'_> {
    /// "o/r · main".
    pub fn title(&self) -> String {
        format!("{} · {}", self.repo, self.root_base)
    }

    /// "20 PRs, 18 watched".
    pub fn size_line(&self) -> String {
        let prs = if self.prs == 1 { "PR" } else { "PRs" };
        format!("{} {prs}, {} watched", self.prs, self.watched)
    }

    /// The watched PRs' states, worst first: "1 needs you · 3 running · 6
    /// passed".
    pub fn status_line(&self) -> String {
        let parts: Vec<String> = PrState::ALL
            .into_iter()
            .filter(|&state| self.states[state as usize] > 0)
            .map(|state| format!("{} {}", self.states[state as usize], state.label()))
            .collect();
        if parts.is_empty() {
            "Not watched".to_owned()
        } else {
            parts.join(" · ")
        }
    }

    /// The worst state any watched PR is in.
    pub fn worst(&self) -> Option<PrState> {
        PrState::ALL
            .into_iter()
            .find(|&state| self.states[state as usize] > 0)
    }
}

/// The PR a Stack's row points at, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attention<'a> {
    pub pr: &'a PullRequest,
    /// The title of its oldest open Inbox entry, if it has one.
    pub why: Option<&'a str>,
}

impl Attention<'_> {
    /// "#12 Not shippable".
    pub fn line(&self) -> String {
        let why = match self.why {
            Some(why) => why.to_owned(),
            None => status_line(self.pr),
        };
        format!("#{} {why}", self.pr.number)
    }
}

/// Where a watched PR stands, for a Stack's summary row. Worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrState {
    /// It has an open Inbox entry.
    NeedsYou,
    /// Its latest Run ended not shippable or over budget.
    Failed,
    Running,
    /// No Run yet, such as while its root base has no Pipeline.
    Waiting,
    /// Its latest Run ended some other way, such as cancelled.
    Ended,
    /// Its latest Run ended shippable or merged.
    Passed,
}

impl PrState {
    pub const ALL: [PrState; 6] = [
        PrState::NeedsYou,
        PrState::Failed,
        PrState::Running,
        PrState::Waiting,
        PrState::Ended,
        PrState::Passed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PrState::NeedsYou => "needs you",
            PrState::Failed => "failed",
            PrState::Running => "running",
            PrState::Waiting => "waiting",
            PrState::Ended => "ended",
            PrState::Passed => "passed",
        }
    }
}

/// The sources pane's selection: every PR, those whose latest Run is
/// going or has ended, or one repo's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    All,
    Running,
    Ended,
    Repo(RepoName),
}

impl Source {
    fn shows(&self, pr: &PullRequest) -> bool {
        let ended = pr.runs.first().map(|run| run.end.is_some());
        match self {
            Source::All => true,
            Source::Running => ended == Some(false),
            Source::Ended => ended == Some(true),
            Source::Repo(repo) => &pr.repo == repo,
        }
    }
}

#[derive(Debug, Default)]
pub struct Prs {
    topic: WatchedPrs,
    /// The sequence number of the last update applied. `None` until the
    /// first snapshot.
    seq: Option<u64>,
    pub source: Source,
    /// The title of each PR's oldest open Inbox entry.
    needs_you: HashMap<(RepoName, u64), String>,
    /// PRs and parents of the Stacks shown expanded. A Stack is expanded
    /// while any of its PRs is here, so it stays expanded as it lands.
    expanded: HashSet<(RepoName, u64)>,
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

    /// Takes the open Inbox entries, oldest first, which say which PRs
    /// need the developer.
    pub fn set_inbox(&mut self, entries: &[InboxEntry]) {
        self.needs_you.clear();
        for entry in entries {
            for pr in &entry.prs {
                self.needs_you
                    .entry((pr.repo.clone(), pr.number))
                    .or_insert_with(|| entry.title.clone());
            }
        }
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
        self.topic.prs.iter().filter(|pr| self.source.shows(pr))
    }

    /// How many PRs `source` shows, for its sources entry. A Stack counts
    /// each of its PRs.
    pub fn count(&self, source: &Source) -> usize {
        self.topic.prs.iter().filter(|pr| source.shows(pr)).count()
    }

    /// Where a watched PR stands. `None` for one that isn't watched.
    pub fn state(&self, pr: &PullRequest) -> Option<PrState> {
        if !pr.watched() {
            return None;
        }
        if self.needs_you.contains_key(&(pr.repo.clone(), pr.number)) {
            return Some(PrState::NeedsYou);
        }
        Some(match pr.runs.first().map(|run| run.end) {
            None => PrState::Waiting,
            Some(None) => PrState::Running,
            Some(Some(EndReason::NotShippable | EndReason::OverBudget)) => PrState::Failed,
            Some(Some(EndReason::Shippable | EndReason::Merged)) => PrState::Passed,
            Some(Some(_)) => PrState::Ended,
        })
    }

    /// The rows the PR list shows for the selected source. A Stack is one
    /// summary row, followed when expanded by its tree in stack order: its
    /// PRs under their parents, and a parent that isn't one of the
    /// developer's PRs as a row of its own. Under a filtered source, a
    /// Stack shows if any of its PRs matches, and its tree holds only
    /// those PRs and the parents that place them.
    pub fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        for tree in self.trees() {
            let shown = self.shown(&tree);
            if !shown.contains(&true) {
                continue;
            }
            if let [row] = tree[..] {
                rows.push(row);
                continue;
            }
            let stack = self.summary(&tree);
            rows.push(Row::Stack(stack));
            if !stack.expanded {
                continue;
            }
            for (row, _) in tree.into_iter().zip(shown).filter(|(_, shown)| *shown) {
                rows.push(match row {
                    Row::Pr { pr, depth, parent } => Row::Pr {
                        pr,
                        depth: depth + 1,
                        parent,
                    },
                    Row::Parent {
                        repo,
                        parent,
                        depth,
                    } => Row::Parent {
                        repo,
                        parent,
                        depth: depth + 1,
                    },
                    Row::Stack(_) => row,
                });
            }
        }
        rows
    }

    /// Expands or collapses the Stack whose bottom is `root`.
    pub fn toggle(&mut self, repo: &RepoName, root: u64) {
        let members: Vec<(RepoName, u64)> = self
            .trees()
            .into_iter()
            .find(|tree| tree[0].key() == (repo, root))
            .into_iter()
            .flatten()
            .map(|row| {
                let (repo, number) = row.key();
                (repo.clone(), number)
            })
            .collect();
        if members.iter().any(|key| self.expanded.contains(key)) {
            for key in &members {
                self.expanded.remove(key);
            }
        } else {
            self.expanded.extend(members);
        }
    }

    /// Expands the Stack PR `number` is in, if it's in one, so that
    /// opening the PR from the Inbox or a banner shows its row.
    pub fn expand_to(&mut self, repo: &RepoName, number: u64) {
        self.expanded.insert((repo.clone(), number));
    }

    /// Every PR as a tree in stack order, the bottom first and each PR one
    /// level under its parent. A tree of one row is a PR outside any
    /// Stack.
    fn trees(&self) -> Vec<Vec<Row<'_>>> {
        let prs: Vec<&PullRequest> = self.topic.prs.iter().collect();
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

        let mut trees: Vec<Vec<Row>> = Vec::new();
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
                let row = match pr {
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
                };
                match trees.last_mut() {
                    Some(tree) if depth > 0 => tree.push(row),
                    _ => trees.push(vec![row]),
                }
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
                None => return trees,
            }
        }
    }

    /// Which rows of `tree` the selected source shows: the PRs it matches
    /// and the parents that place them.
    fn shown(&self, tree: &[Row<'_>]) -> Vec<bool> {
        let mut shown = vec![false; tree.len()];
        // The rows from the tree's bottom up to this one.
        let mut path: Vec<usize> = Vec::new();
        for (i, row) in tree.iter().enumerate() {
            path.truncate(row.depth());
            path.push(i);
            if let Row::Pr { pr, .. } = row
                && self.source.shows(pr)
            {
                for &below in &path {
                    shown[below] = true;
                }
            }
        }
        shown
    }

    /// A Stack's summary row, over all its PRs whatever the source shows.
    fn summary<'a>(&'a self, tree: &[Row<'a>]) -> Stack<'a> {
        let (repo, root) = tree[0].key();
        let mut stack = Stack {
            repo,
            root,
            root_base: "",
            prs: tree.len(),
            watched: 0,
            states: [0; PrState::ALL.len()],
            attention: None,
            expanded: false,
        };
        let mut attention: Option<(PrState, &PullRequest)> = None;
        for row in tree {
            let (repo, number) = row.key();
            stack.expanded |= self.expanded.contains(&(repo.clone(), number));
            let Row::Pr { pr, .. } = row else { continue };
            // Every stacked PR knows the root base. The bottom PR knows it
            // too, as its own base, when it's listed.
            if stack.root_base.is_empty() || pr.stack.is_some() {
                stack.root_base = pr.root_base();
            }
            let Some(state) = self.state(pr) else {
                continue;
            };
            stack.watched += 1;
            stack.states[state as usize] += 1;
            if state <= PrState::Failed && attention.is_none_or(|(worst, _)| state < worst) {
                attention = Some((state, pr));
            }
        }
        stack.attention = attention.map(|(_, pr)| Attention {
            pr,
            why: self
                .needs_you
                .get(&(pr.repo.clone(), pr.number))
                .map(String::as_str),
        });
        stack
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
    use slopwatch_core::GateState;
    use slopwatch_protocol::{EntryId, PrRef, RunId, RunSummary, Scope, WatchedPrsDelta};

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
    /// isn't listed and `!` a row that doesn't open. A Stack's row is `S`
    /// and its bottom's number.
    fn tree(prs: &Prs) -> Vec<String> {
        prs.rows()
            .iter()
            .map(|row| {
                let mark = if row.selectable() { "" } else { "!" };
                match row {
                    Row::Stack(stack) => format!("S{}", stack.root),
                    Row::Pr { pr, depth, .. } => format!("{depth}:{}{mark}", pr.number),
                    Row::Parent { parent, depth, .. } => {
                        format!("{depth}:~{}{mark}", parent.number)
                    }
                }
            })
            .collect()
    }

    /// The summary row of the Stack whose bottom is `root`.
    fn stack_row(prs: &Prs, root: u64) -> Stack<'_> {
        prs.rows()
            .into_iter()
            .find_map(|row| match row {
                Row::Stack(stack) if stack.root == root => Some(stack),
                _ => None,
            })
            .expect("the Stack has a row")
    }

    fn run(id: u64, end: Option<EndReason>, gate: GateState) -> RunSummary {
        RunSummary {
            id: RunId(id),
            head_sha: String::new(),
            gate,
            end,
            waived: false,
        }
    }

    fn entry(id: u64, title: &str, prs: &[u64]) -> InboxEntry {
        InboxEntry {
            id: EntryId(id),
            scope: Scope::Pr,
            title: title.into(),
            reasons: vec![],
            prs: prs
                .iter()
                .map(|&number| PrRef {
                    repo: repo("a"),
                    number,
                })
                .collect(),
            raised_at: 0,
            budget: None,
            closed: None,
        }
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

        assert_eq!(tree(&prs), ["S1", "0:4"], "collapsed until expanded");

        prs.toggle(&repo("a"), 1);

        assert_eq!(tree(&prs), ["S1", "1:1", "2:2", "3:3", "2:5", "0:4"]);

        prs.toggle(&repo("a"), 1);

        assert_eq!(tree(&prs), ["S1", "0:4"]);
    }

    #[test]
    fn a_twenty_pr_stack_takes_one_row() {
        let mut list = vec![pr("a", 1, PrStatus::Ready), pr("a", 100, PrStatus::Ready)];
        list.extend((2..=20).map(|number| stacked(number, number - 1, None)));
        let mut prs = Prs::default();
        prs.apply(snapshot(1, &["a"], list));

        assert_eq!(tree(&prs), ["S1", "0:100"]);
        assert_eq!(stack_row(&prs, 1).size_line(), "20 PRs, 20 watched");

        prs.toggle(&repo("a"), 1);

        assert_eq!(prs.rows().len(), 22);
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

        assert_eq!(tree(&prs), ["S5", "S6"]);

        prs.toggle(&repo("a"), 5);
        prs.toggle(&repo("a"), 6);

        assert_eq!(
            tree(&prs),
            ["S5", "1:5!", "2:9", "S6", "1:~6!", "2:3", "2:8"],
            "an unwatched parent doesn't open, and an unwatched PR on its own does"
        );
        assert_eq!(
            stack_row(&prs, 5).size_line(),
            "2 PRs, 1 watched",
            "one watched PR on an unwatched parent still groups"
        );
        assert_eq!(stack_row(&prs, 6).title(), "o/a · main");
        assert!(
            Row::Pr {
                pr: &unwatched,
                depth: 0,
                parent: false
            }
            .selectable()
        );
    }

    /// A Stack of #1 to #7 in stack order: one PR that needs the developer,
    /// one failed, two running, one waiting, one passed and one unwatched.
    fn busy_stack() -> Prs {
        let ran = |mut pr: PullRequest, end, gate| {
            pr.runs = vec![run(pr.number, end, gate)];
            pr
        };
        let mut unwatched = stacked(7, 6, None);
        unwatched.status = PrStatus::NotWatched;
        let mut prs = Prs::default();
        prs.apply(snapshot(
            1,
            &["a"],
            vec![
                ran(
                    pr("a", 1, PrStatus::Ready),
                    Some(EndReason::Shippable),
                    GateState::Pass,
                ),
                ran(
                    stacked(2, 1, None),
                    Some(EndReason::NotShippable),
                    GateState::Fail,
                ),
                ran(stacked(3, 2, None), None, GateState::Pending),
                ran(stacked(4, 3, None), None, GateState::Fail),
                stacked(5, 4, None),
                ran(stacked(6, 5, None), None, GateState::Pending),
                unwatched,
                pr("a", 9, PrStatus::Ready),
            ],
        ));
        prs.set_inbox(&[entry(1, "Approve the deploy", &[4])]);
        prs
    }

    #[test]
    fn a_stacks_row_sums_up_its_prs_worst_first() {
        let mut prs = busy_stack();
        let stack = stack_row(&prs, 1);

        assert_eq!(stack.size_line(), "7 PRs, 6 watched");
        assert_eq!(
            stack.status_line(),
            "1 needs you · 1 failed · 2 running · 1 waiting · 1 passed",
            "#4 needs you even though it's running"
        );
        assert_eq!(stack.worst(), Some(PrState::NeedsYou));
        let attention = stack.attention.expect("a PR needs attention");
        assert_eq!(attention.line(), "#4 Approve the deploy");
        assert_eq!(attention.pr.runs[0].gate, GateState::Fail);

        // The entry closes and a Run starts on #5.
        prs.set_inbox(&[]);
        let mut five = stacked(5, 4, None);
        five.runs = vec![run(5, None, GateState::Pending)];
        prs.apply(delta(2, WatchedPrsDelta::PrChanged { pr: five }));
        let stack = stack_row(&prs, 1);

        assert_eq!(stack.status_line(), "1 failed · 4 running · 1 passed");
        assert_eq!(
            stack.attention.map(|attention| attention.line()).as_deref(),
            Some("#2 Not shippable"),
            "without an entry, a failed Run is what needs attention"
        );
    }

    #[test]
    fn a_filtered_source_shows_a_stack_with_only_its_matching_prs() {
        let mut prs = busy_stack();
        prs.toggle(&repo("a"), 1);
        prs.source = Source::Running;

        assert_eq!(
            tree(&prs),
            ["S1", "1:1", "2:2", "3:3", "4:4", "5:5", "6:6"],
            "the running #3, #4 and #6, and the parents that place them"
        );
        assert_eq!(
            stack_row(&prs, 1).status_line(),
            "1 needs you · 1 failed · 2 running · 1 waiting · 1 passed",
            "the row sums up the whole Stack"
        );

        prs.source = Source::Ended;

        assert_eq!(tree(&prs), ["S1", "1:1", "2:2"]);
        assert_eq!(prs.count(&Source::Ended), 2, "the badge counts PRs");
        assert_eq!(prs.count(&Source::Running), 3);

        prs.toggle(&repo("a"), 1);

        assert_eq!(tree(&prs), ["S1"]);
        assert!(
            !tree(&prs).contains(&"0:9".to_owned()),
            "#9 has no Run, so neither source shows it"
        );
    }

    #[test]
    fn a_stack_without_a_matching_pr_is_left_out() {
        let mut prs = Prs::default();
        prs.apply(snapshot(
            1,
            &["a"],
            vec![pr("a", 1, PrStatus::Ready), stacked(2, 1, None)],
        ));
        prs.source = Source::Running;

        assert!(prs.rows().is_empty());
    }

    #[test]
    fn opening_a_pr_in_a_collapsed_stack_expands_it() {
        let mut prs = busy_stack();
        assert_eq!(tree(&prs), ["S1", "0:9"]);

        prs.expand_to(&repo("a"), 4);

        assert!(stack_row(&prs, 1).expanded);
        assert!(tree(&prs).contains(&"4:4".to_owned()));

        prs.toggle(&repo("a"), 1);

        assert_eq!(tree(&prs), ["S1", "0:9"], "a toggle collapses it again");
    }

    #[test]
    fn a_stack_stays_expanded_as_it_lands() {
        let mut prs = Prs::default();
        prs.apply(snapshot(
            1,
            &["a"],
            vec![
                pr("a", 1, PrStatus::Ready),
                stacked(2, 1, None),
                stacked(3, 2, None),
            ],
        ));
        prs.toggle(&repo("a"), 1);

        // #1 merges, and #2 is retargeted onto main.
        prs.apply(delta(
            2,
            WatchedPrsDelta::PrGone {
                repo: repo("a"),
                number: 1,
            },
        ));
        prs.apply(delta(
            3,
            WatchedPrsDelta::PrChanged {
                pr: pr("a", 2, PrStatus::Ready),
            },
        ));

        assert_eq!(tree(&prs), ["S2", "1:2", "2:3"]);
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
