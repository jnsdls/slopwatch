//! The journal of commits the daemon makes from write Steps' changes (ADR
//! 0002, ADR 0008), and the Fix round streak it counts from them.
//!
//! A commit gets a row before the daemon calls GitHub, with the head it
//! expects and the tree it should make. The row is finished with the SHA
//! GitHub made, which goes in the push journal in the same transaction, or
//! with none if GitHub made none. A row left open by a crash is settled by
//! comparing the PR's head with what the row expected.

use rusqlite::{OptionalExtension, params};
use slopwatch_core::EndReason;
use slopwatch_protocol::step::EffectKind;
use slopwatch_protocol::{RepoName, RunId};

use super::{Store, StoreError, append_event, json_str, parse_repo};

/// A commit the daemon is about to ask GitHub for.
pub struct NewCommitIntent<'a> {
    pub run: RunId,
    pub step: &'a str,
    /// The PR's repo, which the push journal keys on.
    pub repo: &'a RepoName,
    pub number: u64,
    /// Where the PR's head branch lives, which can be a fork.
    pub head_repo: &'a RepoName,
    pub branch: &'a str,
    pub expected_head: &'a str,
    /// The tree the commit should have: the head's with the Step's changes.
    pub tree: &'a str,
    /// The paths it changes.
    pub files: &'a [String],
}

/// One row of the commit journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitIntent {
    pub id: i64,
    pub run: RunId,
    pub step: String,
    pub repo: RepoName,
    pub number: u64,
    pub head_repo: RepoName,
    pub branch: String,
    pub expected_head: String,
    pub tree: String,
    pub files: Vec<String>,
    /// The commit GitHub made, once finished with one.
    pub sha: Option<String>,
    pub finished: bool,
}

/// The Fix rounds a Run on a PR's head continues (CONTEXT.md, Fix round).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Streak {
    /// Commits from write Steps since the last push from outside. A
    /// rebase or branch update slopwatch made neither counts nor resets.
    pub rounds: u32,
    /// The heads the streak went through, the Run's own first, back to
    /// the one an outside push made.
    pub heads: Vec<String>,
    /// The trees the daemon's commits in the streak made.
    pub trees: Vec<String>,
}

const SELECT_COMMIT: &str = "SELECT id, run_id, step, repo, number, head_repo, branch,
    expected_head, tree, sha, finished, files FROM commits";

fn commit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommitIntent> {
    Ok(CommitIntent {
        id: row.get(0)?,
        run: RunId(row.get::<_, i64>(1)? as u64),
        step: row.get(2)?,
        repo: parse_repo(&row.get::<_, String>(3)?),
        number: row.get::<_, i64>(4)? as u64,
        head_repo: parse_repo(&row.get::<_, String>(5)?),
        branch: row.get(6)?,
        expected_head: row.get(7)?,
        tree: row.get(8)?,
        sha: row.get(9)?,
        finished: row.get(10)?,
        files: serde_json::from_str(&row.get::<_, String>(11)?).unwrap_or_default(),
    })
}

impl Store {
    /// Records a commit the daemon is about to ask GitHub for.
    pub fn insert_commit(&self, new: &NewCommitIntent<'_>) -> Result<CommitIntent, StoreError> {
        let db = self.db();
        db.execute(
            "INSERT INTO commits (run_id, step, repo, number, head_repo, branch, expected_head,
                                  tree, files)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                new.run.0 as i64,
                new.step,
                new.repo.to_string(),
                new.number as i64,
                new.head_repo.to_string(),
                new.branch,
                new.expected_head,
                new.tree,
                serde_json::to_string(new.files).expect("paths always serialize"),
            ],
        )?;
        Ok(CommitIntent {
            id: db.last_insert_rowid(),
            run: new.run,
            step: new.step.to_owned(),
            repo: new.repo.clone(),
            number: new.number,
            head_repo: new.head_repo.clone(),
            branch: new.branch.to_owned(),
            expected_head: new.expected_head.to_owned(),
            tree: new.tree.to_owned(),
            files: new.files.to_vec(),
            sha: None,
            finished: false,
        })
    }

    /// Settles a commit with the SHA GitHub made, or `None` if it made
    /// none. A made commit goes in the push journal, and `event`, the Run
    /// event that says so, in its Run's journal, all or nothing. Returns
    /// the event's sequence number. A commit settled already stays as it
    /// was, and nothing is appended.
    pub fn finish_commit(
        &self,
        intent: &CommitIntent,
        made: Option<(&str, i64, &str)>,
    ) -> Result<Option<u64>, StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        let settled = tx.execute(
            "UPDATE commits SET sha = ?2, finished = 1 WHERE id = ?1 AND finished = 0",
            params![intent.id, made.map(|(sha, _, _)| sha)],
        )?;
        if settled == 0 {
            return Ok(None);
        }
        let mut seq = None;
        if let Some((sha, ts, event)) = made {
            tx.execute(
                "INSERT OR IGNORE INTO push_journal (repo, sha) VALUES (?1, ?2)",
                params![intent.repo.to_string(), sha],
            )?;
            seq = Some(append_event(&tx, intent.run, ts, event)?);
        }
        tx.commit()?;
        Ok(seq)
    }

    /// Commits asked for and never settled, oldest first.
    pub fn open_commits(&self) -> Result<Vec<CommitIntent>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(&format!("{SELECT_COMMIT} WHERE finished = 0 ORDER BY id"))?;
        let rows = query.query_map([], commit_row)?;
        rows.collect()
    }

    /// Whether Run `run` has a commit GitHub may be making right now.
    pub fn committing(&self, run: RunId) -> Result<bool, StoreError> {
        self.db()
            .query_row(
                "SELECT 1 FROM commits WHERE run_id = ?1 AND finished = 0 LIMIT 1",
                params![run.0 as i64],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
    }

    /// The Fix round streak a Run on PR `number`'s `head` continues,
    /// counting the PR's Runs before `before`, or all of them.
    ///
    /// The Runs are walked newest first. One on the same head as the Run
    /// after it is a same-SHA Run, which no push separates. Otherwise a
    /// push came between them: a commit of the earlier Run's that made the
    /// later head is a Fix round. A rebase the earlier Run asked for, a
    /// Stack update of its head or a push in the push journal is
    /// slopwatch's own, which neither counts nor resets. Anything else came
    /// from outside and ends the streak.
    pub fn fix_streak(
        &self,
        repo: &RepoName,
        number: u64,
        head: &str,
        before: Option<RunId>,
    ) -> Result<Streak, StoreError> {
        let db = self.db();
        let mut runs = db.prepare(
            "SELECT id, head_sha, end_reason FROM runs
             WHERE repo = ?1 AND number = ?2 AND id < ?3 ORDER BY id DESC",
        )?;
        let before = before.map_or(i64::MAX, |run| run.0 as i64);
        let rows = runs.query_map(params![repo.to_string(), number as i64, before], |row| {
            Ok((
                RunId(row.get::<_, i64>(0)? as u64),
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut commit = db.prepare(
            "SELECT tree FROM commits WHERE run_id = ?1 AND sha = ?2 AND finished = 1 LIMIT 1",
        )?;
        let pushed = json_str(&EndReason::Pushed);
        let mut streak = Streak {
            heads: vec![head.to_owned()],
            ..Streak::default()
        };
        let mut later = head.to_owned();
        for row in rows {
            let (id, run_head, reason) = row?;
            if run_head == later {
                continue;
            }
            let fixed: Option<String> = commit
                .query_row(params![id.0 as i64, later], |row| row.get(0))
                .optional()?;
            match fixed {
                Some(tree) => {
                    streak.rounds += 1;
                    streak.trees.push(tree);
                }
                None if reason.as_deref() == Some(pushed.as_str())
                    && slopwatch_pushed(&db, repo, number, id, &run_head, &later)? => {}
                None => break,
            }
            streak.heads.push(run_head.clone());
            later = run_head;
        }
        Ok(streak)
    }
}

/// Whether slopwatch made the push from Run `run`, on `head`, to `later`:
/// a SHA in the push journal, a rebase the Run asked GitHub for, or a
/// Stack update of that head. A rebase or an update doesn't say what head
/// it made, so any head after one counts (ADR 0004).
fn slopwatch_pushed(
    db: &rusqlite::Connection,
    repo: &RepoName,
    number: u64,
    run: RunId,
    head: &str,
    later: &str,
) -> Result<bool, StoreError> {
    let journaled = db
        .query_row(
            "SELECT 1 FROM push_journal WHERE repo = ?1 AND sha = ?2",
            params![repo.to_string(), later],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let updated = db
        .query_row(
            "SELECT 1 FROM stack_updates WHERE repo = ?1 AND number = ?2 AND head_sha = ?3",
            params![repo.to_string(), number as i64, head],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(journaled || updated || super::asked_for(db, run, EffectKind::Rebase)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewRun, NewStep};

    fn repo() -> RepoName {
        RepoName::new("o", "r")
    }

    /// Starts a Run on `head` and ends it with `reason`.
    fn run(store: &Store, head: &str, reason: EndReason) -> RunId {
        let repo = repo();
        let id = store
            .insert_run(
                &NewRun {
                    repo: &repo,
                    number: 7,
                    head_sha: head,
                    base: "main",
                    pr_base: "main",
                    base_sha: "base",
                    pipeline: "version: 1",
                    files: &[],
                    linked_issues: &[],
                    budget_window: None,
                    lifted: &[],
                    steps: vec![NewStep {
                        id: "fix".into(),
                        plugin: "fix".into(),
                        config_hash: "hash".into(),
                    }],
                },
                0,
            )
            .unwrap();
        store.end_run(id, reason, 0).unwrap();
        id
    }

    /// Run `run` committed `sha` with `tree` on top of `head`.
    fn commit(store: &Store, run: RunId, head: &str, sha: &str, tree: &str) {
        let repo = repo();
        let intent = store
            .insert_commit(&NewCommitIntent {
                run,
                step: "fix",
                repo: &repo,
                number: 7,
                head_repo: &repo,
                branch: "feature",
                expected_head: head,
                tree,
                files: &[],
            })
            .unwrap();
        store.finish_commit(&intent, Some((sha, 0, "{}"))).unwrap();
    }

    #[test]
    fn a_commit_stays_open_until_settled_and_a_made_one_is_a_push_of_ours() {
        let store = Store::in_memory();
        let id = run(&store, "a", EndReason::Pushed);
        let repo = repo();
        let new = NewCommitIntent {
            run: id,
            step: "fix",
            repo: &repo,
            number: 7,
            head_repo: &repo,
            branch: "feature",
            expected_head: "a",
            tree: "t1",
            files: &[],
        };
        let made = store.insert_commit(&new).unwrap();
        let lost = store.insert_commit(&new).unwrap();
        assert!(store.committing(id).unwrap());
        assert_eq!(store.open_commits().unwrap(), [made.clone(), lost.clone()]);

        store.finish_commit(&made, Some(("b", 0, "{}"))).unwrap();
        store.finish_commit(&lost, None).unwrap();

        assert!(store.open_commits().unwrap().is_empty());
        assert!(!store.committing(id).unwrap());
        assert!(store.pushed_by_slopwatch(&repo, "b").unwrap());
    }

    #[test]
    fn fix_rounds_count_commits_since_the_last_outside_push() {
        let store = Store::in_memory();
        // An outside push made `a`. Fix committed `b` on it, and `c` on `b`.
        let first = run(&store, "a", EndReason::Pushed);
        commit(&store, first, "a", "b", "tb");
        let second = run(&store, "b", EndReason::Pushed);
        commit(&store, second, "b", "c", "tc");

        let streak = store.fix_streak(&repo(), 7, "c", None).unwrap();
        assert_eq!(streak.rounds, 2);
        assert_eq!(streak.heads, ["c", "b", "a"]);
        assert_eq!(streak.trees, ["tc", "tb"]);

        // A same-SHA Run on `c`, as after a Waiver, continues it.
        let same = run(&store, "c", EndReason::NotShippable);
        assert_eq!(store.fix_streak(&repo(), 7, "c", None).unwrap().rounds, 2);
        // Counting only the Runs before one of them.
        assert_eq!(
            store
                .fix_streak(&repo(), 7, "b", Some(second))
                .unwrap()
                .rounds,
            1
        );

        // An outside push to `d` resets it.
        let _ = same;
        let streak = store.fix_streak(&repo(), 7, "d", None).unwrap();
        assert_eq!(streak.rounds, 0);
        assert_eq!(streak.heads, ["d"]);
    }

    #[test]
    fn a_rebase_slopwatch_made_neither_counts_nor_resets_the_streak() {
        let store = Store::in_memory();
        let first = run(&store, "a", EndReason::Pushed);
        commit(&store, first, "a", "b", "tb");
        // The Run on `b` ended with a rebase, which made `r`.
        let rebased = run(&store, "b", EndReason::Pushed);
        let effect = slopwatch_protocol::step::Effect::Rebase {
            method: slopwatch_protocol::step::UpdateMethod::Merge,
        };
        let name = repo();
        store
            .insert_effect(&crate::store::NewEffect {
                run: rebased,
                step: "merge",
                request: "rebase",
                repo: &name,
                number: 7,
                head_sha: "b",
                effect: &effect,
            })
            .unwrap();

        let streak = store.fix_streak(&repo(), 7, "r", None).unwrap();
        assert_eq!(streak.rounds, 1);
        assert_eq!(streak.heads, ["r", "b", "a"]);
    }

    #[test]
    fn an_outside_push_after_a_fix_commit_resets_the_streak() {
        let store = Store::in_memory();
        let first = run(&store, "a", EndReason::Pushed);
        commit(&store, first, "a", "b", "tb");
        // The Run on `b` ended pushed, but `x` came from someone else
        // before a Run on it started.
        run(&store, "b", EndReason::Pushed);

        let streak = store.fix_streak(&repo(), 7, "x", None).unwrap();
        assert_eq!(streak.rounds, 0);
        assert_eq!(streak.heads, ["x"]);
    }

    #[test]
    fn a_commit_that_didnt_make_the_next_head_isnt_a_round() {
        let store = Store::in_memory();
        // Fix committed `b`, but the Run ended superseded by an outside
        // push of `x` that GitHub took first.
        let first = run(&store, "a", EndReason::Superseded);
        commit(&store, first, "a", "b", "tb");

        assert_eq!(store.fix_streak(&repo(), 7, "x", None).unwrap().rounds, 0);
    }
}
