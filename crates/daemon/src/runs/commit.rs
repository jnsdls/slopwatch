//! What a write Step's changes become (ADR 0008), and how the daemon
//! commits them (ADR 0002).
//!
//! The rules key on `workspace: write`, never on Fix. A write Step that
//! reports `pass` keeps its Verdict back until its process has exited. Its
//! worktree is then read into a tree in the repo's clone, and judged:
//!
//! - no change: nothing to commit, "nothing actionable", and the Fix loop
//!   stops;
//! - a Guarded path changed: the whole commit is refused, and the Step
//!   errors `guarded_path` listing the files;
//! - a file mode, symlink or submodule changed, which `createCommitOnBranch`
//!   can't express: the Step errors listing them;
//! - a tree the PR's head already had earlier in the Fix streak: "loop
//!   detected", and the loop stops;
//! - anything else is committed.
//!
//! A Step that reports anything but `pass` commits nothing. One commit is
//! made at a time per Run, and the first ends the Run as pushed, which
//! cancels the other write Steps and throws their changes away.
//!
//! The commit goes through `createCommitOnBranch` as the developer, with
//! `expectedHeadOid` set to the Run's head, so a branch that moved while
//! the Step ran refuses it with `STALE_DATA` and nothing is written. Its
//! intent row goes in the store before the call and its SHA after, in one
//! transaction with the push journal and the Run's `committed` event, so
//! the next poll reads the new head as slopwatch's own push. A row a crash
//! left open is settled from the PR's head: a commit on the expected head
//! with the expected tree is the one GitHub made.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use slopwatch_core::{EndReason, Guard, Verdict, guarded};
use slopwatch_protocol::step::{Outcome, Outputs};
use slopwatch_protocol::{RepoName, RunId};

use super::{Engine, Input, Journal, PrKey, Runs, step_dir};
use crate::clones::{Change, Clones, WorktreeChanges};
use crate::github::{GitHub, GitHubError, NewCommit};
use crate::store::{CommitIntent, NewCommitIntent, Store, StoreError};

/// The co-author every commit names, so the history shows slopwatch made
/// it. GitHub makes the developer the author (ADR 0002). The `.invalid`
/// domain is reserved, so no GitHub account can own the address and pick
/// up the attribution.
pub(super) const CO_AUTHOR: &str = "slopwatch <noreply@slopwatch.invalid>";

/// How a Step's reason starts when its changes stopped the Fix loop, as
/// the "Fix stopped" PR entry reads them.
pub(super) const NOTHING_ACTIONABLE: &str = "nothing actionable";
/// How a Step's reason starts when its changes would repeat a tree.
pub(super) const LOOP_DETECTED: &str = "loop detected";

/// The longest commit headline, in characters.
const HEADLINE: usize = 72;

/// A write Step that reported `pass`, on its way to a commit.
#[derive(Debug)]
pub(super) struct Write {
    pub attempt: u32,
    pub outcome: Outcome,
    pub stage: Stage,
}

#[derive(Debug)]
/// Where a write Step's changes are on their way to a commit.
pub(super) enum Stage {
    /// The process hasn't exited, so the worktree may still change.
    Exiting,
    /// Its worktree is being read.
    Collecting,
    /// Judged committable, waiting for another write Step's commit.
    Ready(WorktreeChanges),
    /// The commit call is out.
    Committing,
}

/// A write Step's worktree, read.
pub(super) struct Collected {
    run: RunId,
    step: String,
    attempt: u32,
    /// The changes, with the trees of the Fix streak's heads, or why they
    /// couldn't be read.
    result: Result<(WorktreeChanges, Vec<String>), String>,
}

/// What became of a commit call.
pub(super) struct Done {
    run: RunId,
    step: String,
    result: Committed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// What a commit call came to.
enum Committed {
    /// GitHub made it.
    Made(String),
    /// The branch had moved on from the Run's head.
    Stale(String),
    /// GitHub refused it, or it never reached GitHub.
    Refused(String),
    /// The call failed and the daemon couldn't tell whether GitHub made
    /// the commit. Its intent stays open for the next sync to settle.
    Unknown(String),
}

/// What a write Step's changes come to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Judgement {
    /// Nothing changed.
    Empty,
    /// Guarded paths changed, each with why it's guarded.
    Guarded(Vec<(String, &'static str)>),
    /// Changes the API can't commit, each with what it is.
    Unsupported(Vec<(String, &'static str)>),
    /// The tree is one the streak's heads already had.
    Loop,
    /// Nothing stops the commit.
    Commit,
}

/// Judges a write Step's changes under the Pipeline's `guard`, with the
/// trees the Fix streak has been through.
pub(super) fn judge(changes: &WorktreeChanges, guard: Guard, streak: &[String]) -> Judgement {
    if changes.changes.is_empty() {
        return Judgement::Empty;
    }
    let guarded: Vec<_> = changes
        .changes
        .iter()
        .filter_map(|change| Some((change.path.clone(), guarded(&change.path, guard)?)))
        .collect();
    if !guarded.is_empty() {
        return Judgement::Guarded(guarded);
    }
    let unsupported: Vec<_> = changes
        .changes
        .iter()
        .filter_map(|change| Some((change.path.clone(), unsupported(change)?)))
        .collect();
    if !unsupported.is_empty() {
        return Judgement::Unsupported(unsupported);
    }
    if streak.contains(&changes.tree) {
        return Judgement::Loop;
    }
    Judgement::Commit
}

/// What about `change` `createCommitOnBranch` can't express, if anything.
/// It writes regular files and deletes paths, and keeps nothing but
/// contents.
fn unsupported(change: &Change) -> Option<&'static str> {
    let modes = [change.old_mode.as_str(), change.new_mode.as_str()];
    if modes.contains(&"120000") {
        return Some("symlink");
    }
    if modes.contains(&"160000") {
        return Some("submodule");
    }
    match change.status {
        'A' if change.new_mode != "100644" => Some("executable file"),
        'M' if change.old_mode != change.new_mode => Some("file mode change"),
        'A' | 'M' | 'D' => None,
        _ => Some("type change"),
    }
}

/// The commit message: the Step's note for a headline, and the trailers
/// that name the Run and slopwatch (ADR 0002).
fn message(run: RunId, step: &str, outputs: &Outputs) -> (String, String) {
    let note = outputs.note.as_deref().map(str::trim).unwrap_or_default();
    let first = note.lines().next().unwrap_or_default().trim();
    let long = first.chars().count() > HEADLINE;
    let headline = if first.is_empty() {
        format!("Changes from slopwatch Step `{step}`")
    } else if long {
        let cut: String = first.chars().take(HEADLINE - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        first.to_owned()
    };
    let rest = note
        .strip_prefix(first)
        .map(str::trim)
        .filter(|rest| !rest.is_empty());
    let mut body = String::new();
    if long {
        body.push_str(first);
        body.push_str("\n\n");
    }
    if let Some(rest) = rest {
        body.push_str(rest);
        body.push_str("\n\n");
    }
    body.push_str(&format!(
        "Slopwatch-Run: {run}\nCo-authored-by: {CO_AUTHOR}"
    ));
    (headline, body)
}

/// Where a Step attempt's scratch index goes while its worktree is read:
/// next to the worktree, never in it.
fn index_path(data_dir: &Path, run: RunId, step: &str, attempt: u32) -> PathBuf {
    let dir = step_dir(data_dir, run, step, attempt);
    let mut name = dir.file_name().expect("Step dirs have a name").to_owned();
    name.push(".index");
    dir.with_file_name(name)
}

impl Engine {
    /// A write Step reported `pass`. Its Verdict waits until its changes
    /// are judged, and committed if they may be.
    pub(super) fn hold_write(&mut self, key: &PrKey, step: &str, outcome: Outcome) {
        let run = self
            .active
            .get_mut(key)
            .expect("only active Runs get reports");
        let running = run.running.get_mut(step).expect("the report matched it");
        running.reported = true;
        let attempt = running.attempt;
        run.writes.insert(
            step.to_owned(),
            Write {
                attempt,
                outcome,
                stage: Stage::Exiting,
            },
        );
    }

    /// A Step attempt's process exited. If it's a write Step waiting to
    /// commit, its worktree is read on a task of its own, and stays until
    /// then. Returns whether it did.
    pub(super) fn collect_on_exit(&mut self, run: RunId, step: &str, attempt: u32) -> bool {
        let Some(key) = self.key_of(run) else {
            return false;
        };
        let active = self.active.get_mut(&key).expect("found above");
        let Some(write) = active
            .writes
            .get_mut(step)
            .filter(|write| write.attempt == attempt && matches!(write.stage, Stage::Exiting))
        else {
            return false;
        };
        write.stage = Stage::Collecting;
        let dir = step_dir(&self.data_dir, run, step, attempt);
        let index = index_path(&self.data_dir, run, step, attempt);
        let (repo, head) = (active.repo.clone(), active.head_sha.clone());
        let heads = active.streak.heads.clone();
        let (clones, github) = (Arc::clone(&self.clones), Arc::clone(&self.github));
        let reports = self.reports.clone();
        let step = step.to_owned();
        tokio::spawn(async move {
            let work = tokio::spawn(async move {
                let remote = github
                    .git_remote(&repo)
                    .await
                    .map_err(|error| format!("can't reach the repo: {error}"))?;
                let changes = clones
                    .worktree_changes(&repo, &remote, &dir, &head, &index)
                    .await
                    .map_err(|error| format!("can't read what the Step changed: {error}"))?;
                let _ = tokio::fs::remove_dir_all(&dir).await;
                let trees = clones.trees(&repo, &remote, &heads).await;
                Ok((changes, trees))
            });
            let result = work
                .await
                .unwrap_or_else(|error| Err(format!("reading the changes failed: {error}")));
            let _ = reports.send(Input::Collected(Collected {
                run,
                step,
                attempt,
                result,
            }));
        });
        true
    }

    /// Judges a write Step's changes, once read, and settles the Step or
    /// commits them.
    pub(super) fn on_collected(&mut self, collected: Collected) {
        if self.stopped {
            return;
        }
        if let Err(error) = self.try_on_collected(collected) {
            eprintln!("slopwatchd: can't judge a write Step's changes: {error}");
        }
    }

    fn try_on_collected(&mut self, collected: Collected) -> Result<(), StoreError> {
        let Collected {
            run,
            step,
            attempt,
            result,
        } = collected;
        let _ = std::fs::remove_dir_all(step_dir(&self.data_dir, run, &step, attempt));
        let Some(key) = self.key_of(run) else {
            return Ok(());
        };
        let active = &self.active[&key];
        let waiting = active.writes.get(&step).is_some_and(|write| {
            write.attempt == attempt && matches!(write.stage, Stage::Collecting)
        });
        if !waiting {
            return Ok(());
        }
        let (changes, mut trees) = match result {
            Ok(read) => read,
            Err(error) => {
                let reason = format!("error(workspace): {error}");
                self.settle(
                    &key,
                    &step,
                    Verdict::Error,
                    Some(reason),
                    Outputs::default(),
                )?;
                return self.advance(&key);
            }
        };
        trees.extend(active.streak.trees.iter().cloned());
        let outputs = active.writes[&step].outcome.outputs.clone();
        let list = |files: &[(String, &str)]| {
            files
                .iter()
                .map(|(path, why)| format!("{path} ({why})"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match judge(&changes, active.pipeline.guard(), &trees) {
            Judgement::Empty => {
                let reason = format!("{NOTHING_ACTIONABLE}: the Step passed but changed no file");
                self.settle(&key, &step, Verdict::Pass, Some(reason), outputs)?;
            }
            Judgement::Loop => {
                let reason = format!(
                    "{LOOP_DETECTED}: its changes would put back a tree the PR already had \
                     since the last push from outside"
                );
                self.settle(&key, &step, Verdict::Pass, Some(reason), outputs)?;
            }
            Judgement::Guarded(files) => {
                let reason = format!(
                    "error(guarded_path): it changed Guarded paths, so nothing was committed: {}",
                    list(&files)
                );
                self.settle(&key, &step, Verdict::Error, Some(reason), outputs)?;
            }
            Judgement::Unsupported(files) => {
                let reason = format!(
                    "error(unsupported_change): the GitHub API can't commit file modes, symlinks \
                     or submodules, so nothing was committed: {}",
                    list(&files)
                );
                self.settle(&key, &step, Verdict::Error, Some(reason), outputs)?;
            }
            Judgement::Commit => {
                let write = self
                    .active
                    .get_mut(&key)
                    .and_then(|run| run.writes.get_mut(&step))
                    .expect("checked above");
                write.stage = Stage::Ready(changes);
                self.commit_next(&key)?;
                return Ok(());
            }
        }
        self.advance(&key)
    }

    /// Commits the next write Step's changes that wait for it, unless a
    /// commit is out already.
    fn commit_next(&mut self, key: &PrKey) -> Result<(), StoreError> {
        let run = self.active.get_mut(key).expect("only active Runs commit");
        if run
            .writes
            .values()
            .any(|write| matches!(write.stage, Stage::Committing))
        {
            return Ok(());
        }
        // Pipeline order, so the same Pipeline commits the same Step first.
        let Some(step) = run
            .pipeline
            .ordered_steps()
            .map(|step| step.id.clone())
            .find(|id| matches!(run.writes.get(id).map(|w| &w.stage), Some(Stage::Ready(_))))
        else {
            return Ok(());
        };
        // A commit whose fate is unknown may have moved the branch, so no
        // other commit goes out on the head the Run judged. The Run's
        // snapshot is pinned to its head, and a poll that saw the head move
        // would have ended it, so a moved head there means a stale Run.
        let refusal = if self.store.committing(run.id)? {
            Some("an earlier commit in this Run may have landed, so nothing more is committed")
        } else if run
            .snapshot
            .as_ref()
            .is_some_and(|s| s.head_sha != run.head_sha)
        {
            Some("the PR's head moved on from the one the Run judged")
        } else {
            None
        };
        if let Some(why) = refusal {
            let outputs = run.writes[&step].outcome.outputs.clone();
            let reason = format!("error(commit): {why}");
            self.settle(key, &step, Verdict::Error, Some(reason), outputs)?;
            return self.commit_next(key);
        }
        let write = run.writes.get_mut(&step).expect("found above");
        let Stage::Ready(changes) = std::mem::replace(&mut write.stage, Stage::Committing) else {
            unreachable!("found Ready above");
        };
        let (headline, body) = message(run.id, &step, &write.outcome.outputs);
        let call = CommitCall {
            run: run.id,
            step: step.clone(),
            repo: run.repo.clone(),
            number: run.number,
            expected_head: run.head_sha.clone(),
            changes,
            headline,
            body,
        };
        self.committing
            .lock()
            .expect("no panics holding it")
            .insert(run.id);
        let (store, github, clones) = (
            self.store.clone(),
            Arc::clone(&self.github),
            Arc::clone(&self.clones),
        );
        let journal = Arc::clone(&self.journal);
        let reports = self.reports.clone();
        let (id, step_id) = (run.id, step);
        tokio::spawn(async move {
            let work =
                tokio::spawn(
                    async move { call.run(&store, &journal, github.as_ref(), &clones).await },
                );
            let result = work.await.unwrap_or_else(|error| {
                Committed::Unknown(format!("the commit task failed: {error}"))
            });
            let _ = reports.send(Input::Committed(Done {
                run: id,
                step: step_id,
                result,
            }));
        });
        Ok(())
    }

    /// A commit call came back. A made commit settles the Step pass and
    /// ends the Run as pushed. A stale one ends it as superseded, since
    /// someone else moved the branch. Any other failure errors the Step,
    /// and the next waiting write Step may commit.
    pub(super) fn on_committed(&mut self, done: Done) {
        self.committing
            .lock()
            .expect("no panics holding it")
            .remove(&done.run);
        if self.stopped {
            return;
        }
        if let Err(error) = self.try_on_committed(done) {
            eprintln!("slopwatchd: can't record a commit: {error}");
        }
    }

    fn try_on_committed(&mut self, done: Done) -> Result<(), StoreError> {
        let Done { run, step, result } = done;
        let Some(key) = self.key_of(run) else {
            return Ok(());
        };
        let Some(write) = self.active[&key].writes.get(&step) else {
            return Ok(());
        };
        let outputs = write.outcome.outputs.clone();
        match result {
            Committed::Made(_) => {
                self.settle(&key, &step, Verdict::Pass, None, outputs)?;
                let run = self.active.remove(&key).expect("found above");
                self.end_run(run, EndReason::Pushed)
            }
            Committed::Stale(why) => {
                let reason = format!(
                    "error(commit): the branch moved while the Step ran, so nothing was written \
                     ({why})"
                );
                self.settle(&key, &step, Verdict::Error, Some(reason), outputs)?;
                let run = self.active.remove(&key).expect("found above");
                self.end_run(run, EndReason::Superseded)
            }
            Committed::Refused(why) | Committed::Unknown(why) => {
                let reason = format!("error(commit): {why}");
                self.settle(&key, &step, Verdict::Error, Some(reason), outputs)?;
                self.commit_next(&key)?;
                self.advance(&key)
            }
        }
    }
}

/// One commit, as its task makes it.
struct CommitCall {
    run: RunId,
    step: String,
    repo: RepoName,
    number: u64,
    expected_head: String,
    changes: WorktreeChanges,
    headline: String,
    body: String,
}

impl CommitCall {
    async fn run(
        self,
        store: &Store,
        journal: &Journal,
        github: &dyn GitHub,
        clones: &Clones,
    ) -> Committed {
        let refused = |what: &str, error: &dyn std::fmt::Display| {
            Committed::Refused(format!("{what}: {error}"))
        };
        let head = match github.pr_head(&self.repo, self.number).await {
            Ok(head) => head,
            Err(error) => return refused("can't find the PR's head branch", &error),
        };
        if head.sha != self.expected_head {
            return Committed::Stale(format!("its head is {} now", short(&head.sha)));
        }
        let remote = match github.git_remote(&self.repo).await {
            Ok(remote) => remote,
            Err(error) => return refused("can't reach the repo", &error),
        };
        let mut contents = Vec::new();
        let mut deletions = Vec::new();
        for change in &self.changes.changes {
            if change.status == 'D' {
                deletions.push(change.path.as_str());
                continue;
            }
            match clones.blob(&self.repo, &remote, &change.blob).await {
                Ok(blob) => contents.push((change.path.as_str(), blob)),
                Err(error) => return refused(&format!("can't read {}", change.path), &error),
            }
        }
        let files: Vec<String> = self
            .changes
            .changes
            .iter()
            .map(|c| c.path.clone())
            .collect();
        let intent = match store.insert_commit(&NewCommitIntent {
            run: self.run,
            step: &self.step,
            repo: &self.repo,
            number: self.number,
            head_repo: &head.repo,
            branch: &head.branch,
            expected_head: &self.expected_head,
            tree: &self.changes.tree,
            files: &files,
        }) {
            Ok(intent) => intent,
            Err(error) => return refused("can't record the commit", &error),
        };
        let additions: Vec<(&str, &[u8])> = contents
            .iter()
            .map(|(path, blob)| (*path, blob.as_slice()))
            .collect();
        let call = github
            .commit_files(
                &head.repo,
                &NewCommit {
                    branch: &head.branch,
                    expected_head: &self.expected_head,
                    headline: &self.headline,
                    body: &self.body,
                    files: &additions,
                    deletions: &deletions,
                },
            )
            .await;
        let result = match call {
            Ok(sha) => Committed::Made(sha),
            Err(GitHubError::Stale(why)) => Committed::Stale(why),
            // Answers that say GitHub made nothing.
            Err(
                error @ (GitHubError::Unprocessable(_)
                | GitHubError::NotFound(_)
                | GitHubError::Auth(_)),
            ) => Committed::Refused(format!("GitHub refused the commit: {error}")),
            Err(error) => match settle(github, clones, &intent).await {
                Ok(Some(sha)) => Committed::Made(sha),
                Ok(None) => Committed::Refused(format!("the commit didn't reach GitHub: {error}")),
                Err(also) => {
                    return Committed::Unknown(format!(
                        "the commit call failed ({error}), and the daemon couldn't tell whether \
                         GitHub made it ({also}); it checks again on the next poll"
                    ));
                }
            },
        };
        let made = match &result {
            Committed::Made(sha) => Some(sha.as_str()),
            _ => None,
        };
        if let Err(error) = journal.close_commit(&intent, made) {
            return Committed::Unknown(format!("can't record the commit: {error}"));
        }
        result
    }
}

/// Whether GitHub made the commit `intent` asked for, from the PR's head:
/// its SHA if the head is a commit on the expected head with the expected
/// tree, `None` if anything else is. An error when GitHub or git can't be
/// asked.
async fn settle(
    github: &dyn GitHub,
    clones: &Clones,
    intent: &CommitIntent,
) -> Result<Option<String>, String> {
    let head = match github.pr_head(&intent.repo, intent.number).await {
        Ok(head) => head,
        // A PR that's gone takes no commits.
        Err(GitHubError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if head.sha == intent.expected_head {
        return Ok(None);
    }
    let remote = github
        .git_remote(&intent.repo)
        .await
        .map_err(|error| error.to_string())?;
    let (parents, tree) = clones
        .commit_of(&intent.repo, &remote, intent.number, &head.sha)
        .await
        .map_err(|error| error.to_string())?;
    let made = parents == [intent.expected_head.as_str()] && tree == intent.tree;
    Ok(made.then_some(head.sha))
}

impl Runs {
    /// Settles the commits a crash left open, before a sync decides what
    /// a moved head means. One whose Run has a commit out in this daemon
    /// is left to it. One that can't be settled now stays open, and the
    /// next sync tries again.
    pub(super) async fn settle_open_commits(&self) {
        let open = match self.store.open_commits() {
            Ok(open) => open,
            Err(error) => {
                eprintln!("slopwatchd: can't read open commits: {error}");
                return;
            }
        };
        for intent in open {
            if self
                .committing
                .lock()
                .expect("no panics holding it")
                .contains(&intent.run)
            {
                continue;
            }
            match settle(self.github.as_ref(), &self.clones, &intent).await {
                Ok(made) => {
                    if let Err(error) = self.journal.close_commit(&intent, made.as_deref()) {
                        eprintln!("slopwatchd: can't record commit {}: {error}", intent.id);
                    }
                }
                Err(error) => {
                    eprintln!(
                        "slopwatchd: can't tell yet whether commit {} landed: {error}",
                        intent.id
                    );
                }
            }
        }
    }
}

/// A SHA cut to the seven characters a person reads.
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(status: char, path: &str, old: &str, new: &str) -> Change {
        Change {
            path: path.to_owned(),
            status,
            old_mode: old.to_owned(),
            new_mode: new.to_owned(),
            blob: "b".into(),
        }
    }

    fn changes(changes: Vec<Change>) -> WorktreeChanges {
        WorktreeChanges {
            tree: "t".into(),
            changes,
        }
    }

    #[test]
    fn an_empty_diff_is_nothing_to_commit() {
        assert_eq!(
            judge(&changes(vec![]), Guard::default(), &[]),
            Judgement::Empty
        );
    }

    #[test]
    fn one_guarded_path_refuses_the_whole_commit() {
        let judged = judge(
            &changes(vec![
                change('M', "src/a.rs", "100644", "100644"),
                change('M', ".github/workflows/ci.yml", "100644", "100644"),
                change('D', ".slopwatch/pipeline.yml", "100644", "000000"),
                change('M', "Cargo.lock", "100644", "100644"),
            ]),
            Guard::default(),
            &[],
        );
        assert_eq!(
            judged,
            Judgement::Guarded(vec![
                (".github/workflows/ci.yml".into(), "CI config or Pipeline"),
                (".slopwatch/pipeline.yml".into(), "CI config or Pipeline"),
                ("Cargo.lock".into(), "lockfile"),
            ])
        );
        let unguarded = judge(
            &changes(vec![change('M', "Cargo.lock", "100644", "100644")]),
            Guard { lockfiles: false },
            &[],
        );
        assert_eq!(unguarded, Judgement::Commit);
    }

    #[test]
    fn modes_symlinks_and_submodules_cant_be_committed() {
        let judged = judge(
            &changes(vec![
                change('A', "run.sh", "000000", "100755"),
                change('M', "tool", "100644", "100755"),
                change('A', "link", "000000", "120000"),
                change('D', "old-link", "120000", "000000"),
                change('A', "vendor/lib", "000000", "160000"),
                change('T', "thing", "100644", "120000"),
                change('M', "bin/exe", "100755", "100755"),
                change('D', "gone.txt", "100644", "000000"),
            ]),
            Guard::default(),
            &[],
        );
        assert_eq!(
            judged,
            Judgement::Unsupported(vec![
                ("run.sh".into(), "executable file"),
                ("tool".into(), "file mode change"),
                ("link".into(), "symlink"),
                ("old-link".into(), "symlink"),
                ("vendor/lib".into(), "submodule"),
                ("thing".into(), "symlink"),
            ])
        );
    }

    #[test]
    fn a_tree_from_earlier_in_the_streak_is_a_loop() {
        let diff = changes(vec![change('M', "src/a.rs", "100644", "100644")]);
        assert_eq!(
            judge(&diff, Guard::default(), &["t".into()]),
            Judgement::Loop
        );
        assert_eq!(
            judge(&diff, Guard::default(), &["u".into()]),
            Judgement::Commit
        );
    }

    #[test]
    fn the_message_heads_with_the_note_and_ends_with_the_trailers() {
        let outputs = Outputs {
            note: Some("Handle the empty list.\nThe review found a panic.".into()),
            ..Outputs::default()
        };
        let (headline, body) = message(RunId(42), "fix", &outputs);
        assert_eq!(headline, "Handle the empty list.");
        assert_eq!(
            body,
            "The review found a panic.\n\nSlopwatch-Run: 42\nCo-authored-by: slopwatch \
             <noreply@slopwatch.invalid>"
        );

        let (headline, body) = message(RunId(1), "fix", &Outputs::default());
        assert_eq!(headline, "Changes from slopwatch Step `fix`");
        assert!(body.starts_with("Slopwatch-Run: 1\n"));

        let long = Outputs {
            note: Some("x".repeat(100)),
            ..Outputs::default()
        };
        let (headline, body) = message(RunId(1), "fix", &long);
        assert_eq!(headline.chars().count(), HEADLINE);
        assert!(body.starts_with(&"x".repeat(100)));
    }

    #[test]
    fn the_scratch_index_sits_beside_the_worktree() {
        let index = index_path(Path::new("/data"), RunId(3), "fix", 2);
        assert_eq!(index, Path::new("/data/worktrees/3/fix.2.index"));
    }
}
