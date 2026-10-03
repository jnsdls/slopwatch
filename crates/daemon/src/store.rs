//! The daemon's state on disk: SQLite in WAL mode, with the daemon as the
//! only writer. A [`Store`] is a handle on one connection, and clones share
//! it.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use slopwatch_core::{EndReason, GateState, Verdict};
use slopwatch_protocol::step::Outputs;
use slopwatch_protocol::{RepoName, RunId, RunSummary};

use crate::github::OpenPr;

pub use rusqlite::Error as StoreError;

/// Each entry upgrades the schema by one version, tracked in
/// `PRAGMA user_version`. Append only.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE repos (
        repo TEXT PRIMARY KEY,
        added INTEGER NOT NULL,
        label_created INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE prs (
        repo TEXT NOT NULL REFERENCES repos(repo),
        number INTEGER NOT NULL,
        title TEXT NOT NULL,
        url TEXT NOT NULL,
        draft INTEGER NOT NULL,
        head_sha TEXT NOT NULL,
        base TEXT NOT NULL,
        labeled INTEGER NOT NULL,
        base_has_pipeline INTEGER NOT NULL,
        PRIMARY KEY (repo, number)
    );
",
    // Runs are the record, kept forever, so they outlive the PR rows.
    "
    CREATE TABLE runs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        head_sha TEXT NOT NULL,
        base TEXT NOT NULL,
        base_sha TEXT NOT NULL,
        pipeline TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        gate TEXT NOT NULL DEFAULT 'pending',
        ended_at INTEGER,
        end_reason TEXT
    );
    CREATE INDEX runs_by_pr ON runs (repo, number, id);
    CREATE TABLE run_steps (
        run_id INTEGER NOT NULL REFERENCES runs(id),
        step TEXT NOT NULL,
        plugin TEXT NOT NULL,
        config_hash TEXT NOT NULL,
        state TEXT NOT NULL,
        verdict TEXT,
        reason TEXT,
        outputs TEXT,
        attempt INTEGER NOT NULL DEFAULT 0,
        pgid INTEGER,
        process_started_us INTEGER,
        PRIMARY KEY (run_id, step)
    );
    CREATE TABLE run_events (
        run_id INTEGER NOT NULL REFERENCES runs(id),
        seq INTEGER NOT NULL,
        event TEXT NOT NULL,
        PRIMARY KEY (run_id, seq)
    );
    CREATE TABLE push_journal (
        repo TEXT NOT NULL,
        sha TEXT NOT NULL,
        PRIMARY KEY (repo, sha)
    );
",
    // Crash-only restart (ADR 0009): the session a Step's process ran in,
    // and how many restarts in a row have interrupted the Step.
    "
    ALTER TABLE run_steps ADD COLUMN process_sid INTEGER;
    ALTER TABLE run_steps ADD COLUMN restarts INTEGER NOT NULL DEFAULT 0;
",
];

#[derive(Clone)]
pub struct Store {
    db: Arc<Mutex<Connection>>,
}

/// A Run as it starts.
pub struct NewRun<'a> {
    pub repo: &'a RepoName,
    pub number: u64,
    pub head_sha: &'a str,
    pub base: &'a str,
    pub base_sha: &'a str,
    /// The Pipeline file's text, so the Run can load it again after a
    /// restart.
    pub pipeline: &'a str,
    pub steps: Vec<NewStep>,
}

/// A Run's Step as it starts.
pub struct NewStep {
    pub id: String,
    pub plugin: String,
    pub config_hash: String,
}

/// A Run that hasn't ended, as the store keeps it.
#[derive(Debug, Clone)]
pub struct ActiveRun {
    pub id: RunId,
    pub repo: RepoName,
    pub number: u64,
    pub head_sha: String,
    pub base: String,
    pub base_sha: String,
    pub pipeline: String,
    pub gate: GateState,
    pub steps: Vec<StepRow>,
}

/// One Step of a Run.
#[derive(Debug, Clone, PartialEq)]
pub struct StepRow {
    pub step: String,
    pub state: StepRowState,
    /// How many times the Step's process has been spawned in this Run.
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StepRowState {
    Pending,
    /// The process runs in its own group, so `pgid` is also its pid.
    /// `started_us` is when the kernel says it started and `sid` the
    /// session it runs in, which tell a leftover process from a later one
    /// that reused the pid.
    Running {
        pgid: i32,
        started_us: i64,
        sid: i32,
    },
    /// A daemon restart killed the Step's process before it reported. It
    /// starts again from scratch once the first poll has run, unless too
    /// many restarts in a row interrupted it. `restarts` counts them.
    Interrupted {
        restarts: u32,
    },
    Settled {
        verdict: Verdict,
        reason: Option<String>,
        outputs: Outputs,
    },
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        Self::migrate(db)
    }

    /// A store that lives only as long as the process, for tests.
    pub fn in_memory() -> Self {
        Self::migrate(Connection::open_in_memory().expect("open an in-memory database"))
            .expect("migrate an in-memory database")
    }

    fn migrate(db: Connection) -> Result<Self, StoreError> {
        db.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        for (index, migration) in (0_i64..).zip(MIGRATIONS).skip(version as usize) {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", index + 1)?;
            tx.commit()?;
        }
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
        })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.db
            .lock()
            .expect("no panics while holding the database")
    }

    /// Added repos, oldest first.
    pub fn repos(&self) -> Result<Vec<RepoName>, StoreError> {
        let db = self.db();
        let mut query = db.prepare("SELECT repo FROM repos ORDER BY added, repo")?;
        let rows = query.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|repo| Ok(parse_repo(&repo?)))
            .collect::<Result<_, StoreError>>()
    }

    pub fn add_repo(&self, repo: &RepoName) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT OR IGNORE INTO repos (repo, added)
             VALUES (?1, (SELECT COALESCE(MAX(added), 0) + 1 FROM repos))",
            params![repo.to_string()],
        )?;
        Ok(())
    }

    pub fn label_created(&self, repo: &RepoName) -> Result<bool, StoreError> {
        let created = self
            .db()
            .query_row(
                "SELECT label_created FROM repos WHERE repo = ?1",
                params![repo.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(created.unwrap_or(false))
    }

    pub fn mark_label_created(&self, repo: &RepoName) -> Result<(), StoreError> {
        self.db().execute(
            "UPDATE repos SET label_created = 1 WHERE repo = ?1",
            params![repo.to_string()],
        )?;
        Ok(())
    }

    /// Every stored PR with its repo. A PR's detail isn't stored, so it
    /// comes back empty.
    pub fn prs(&self) -> Result<Vec<(RepoName, OpenPr)>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT repo, number, title, url, draft, head_sha, base, labeled, base_has_pipeline
             FROM prs ORDER BY repo, number",
        )?;
        let rows = query.query_map([], |row| {
            Ok((
                parse_repo(&row.get::<_, String>(0)?),
                OpenPr {
                    number: row.get::<_, i64>(1)? as u64,
                    title: row.get(2)?,
                    url: row.get(3)?,
                    draft: row.get(4)?,
                    head_sha: row.get(5)?,
                    base: row.get(6)?,
                    labeled: row.get(7)?,
                    base_has_pipeline: row.get(8)?,
                    detail: Default::default(),
                },
            ))
        })?;
        rows.collect()
    }

    pub fn put_pr(&self, repo: &RepoName, pr: &OpenPr) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT OR REPLACE INTO prs
                 (repo, number, title, url, draft, head_sha, base, labeled, base_has_pipeline)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                repo.to_string(),
                pr.number as i64,
                pr.title,
                pr.url,
                pr.draft,
                pr.head_sha,
                pr.base,
                pr.labeled,
                pr.base_has_pipeline,
            ],
        )?;
        Ok(())
    }

    pub fn remove_pr(&self, repo: &RepoName, number: u64) -> Result<(), StoreError> {
        self.db().execute(
            "DELETE FROM prs WHERE repo = ?1 AND number = ?2",
            params![repo.to_string(), number as i64],
        )?;
        Ok(())
    }

    /// Records a new Run with every Step pending.
    pub fn insert_run(&self, run: &NewRun<'_>, now: i64) -> Result<RunId, StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO runs (repo, number, head_sha, base, base_sha, pipeline, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.repo.to_string(),
                run.number as i64,
                run.head_sha,
                run.base,
                run.base_sha,
                run.pipeline,
                now,
            ],
        )?;
        let id = tx.last_insert_rowid();
        for NewStep {
            id: step,
            plugin,
            config_hash,
        } in &run.steps
        {
            tx.execute(
                "INSERT INTO run_steps (run_id, step, plugin, config_hash, state)
                 VALUES (?1, ?2, ?3, ?4, 'pending')",
                params![id, step, plugin, config_hash],
            )?;
        }
        tx.commit()?;
        Ok(RunId(id as u64))
    }

    pub fn put_step(&self, run: RunId, row: &StepRow) -> Result<(), StoreError> {
        let columns = match &row.state {
            StepRowState::Pending => StepColumns::state("pending"),
            StepRowState::Running {
                pgid,
                started_us,
                sid,
            } => StepColumns {
                pgid: Some(*pgid),
                started_us: Some(*started_us),
                sid: Some(*sid),
                // A respawn keeps counting the restarts before it.
                restarts: None,
                ..StepColumns::state("running")
            },
            StepRowState::Interrupted { restarts } => StepColumns {
                restarts: Some(*restarts),
                ..StepColumns::state("interrupted")
            },
            StepRowState::Settled {
                verdict,
                reason,
                outputs,
            } => StepColumns {
                verdict: Some(verdict.as_str()),
                reason: reason.clone(),
                outputs: Some(serde_json::to_string(outputs).expect("outputs always serialize")),
                ..StepColumns::state("settled")
            },
        };
        self.db().execute(
            "UPDATE run_steps
             SET state = ?3, verdict = ?4, reason = ?5, outputs = ?6, attempt = ?7,
                 pgid = ?8, process_started_us = ?9, process_sid = ?10,
                 restarts = COALESCE(?11, restarts)
             WHERE run_id = ?1 AND step = ?2",
            params![
                run.0 as i64,
                row.step,
                columns.state,
                columns.verdict,
                columns.reason,
                columns.outputs,
                row.attempt,
                columns.pgid,
                columns.started_us,
                columns.sid,
                columns.restarts,
            ],
        )?;
        Ok(())
    }

    /// Records that a restart killed the running Step's process, and
    /// returns how many restarts in a row have now interrupted it.
    pub fn interrupt_step(&self, run: RunId, step: &str) -> Result<u32, StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "UPDATE run_steps
             SET state = 'interrupted', restarts = restarts + 1,
                 pgid = NULL, process_started_us = NULL, process_sid = NULL
             WHERE run_id = ?1 AND step = ?2 AND state = 'running'",
            params![run.0 as i64, step],
        )?;
        let restarts = tx.query_row(
            "SELECT restarts FROM run_steps WHERE run_id = ?1 AND step = ?2",
            params![run.0 as i64, step],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(restarts)
    }

    pub fn set_gate(&self, run: RunId, gate: GateState) -> Result<(), StoreError> {
        self.db().execute(
            "UPDATE runs SET gate = ?2 WHERE id = ?1",
            params![run.0 as i64, gate_str(gate)],
        )?;
        Ok(())
    }

    pub fn end_run(&self, run: RunId, reason: EndReason, now: i64) -> Result<(), StoreError> {
        self.db().execute(
            "UPDATE runs SET end_reason = ?2, ended_at = ?3 WHERE id = ?1",
            params![run.0 as i64, json_str(&reason), now],
        )?;
        Ok(())
    }

    /// Runs that haven't ended, oldest first.
    pub fn active_runs(&self) -> Result<Vec<ActiveRun>, StoreError> {
        let db = self.db();
        let mut runs = db
            .prepare(
                "SELECT id, repo, number, head_sha, base, base_sha, pipeline, gate
                 FROM runs WHERE end_reason IS NULL ORDER BY id",
            )?
            .query_map([], |row| {
                Ok(ActiveRun {
                    id: RunId(row.get::<_, i64>(0)? as u64),
                    repo: parse_repo(&row.get::<_, String>(1)?),
                    number: row.get::<_, i64>(2)? as u64,
                    head_sha: row.get(3)?,
                    base: row.get(4)?,
                    base_sha: row.get(5)?,
                    pipeline: row.get(6)?,
                    gate: parse_gate(&row.get::<_, String>(7)?),
                    steps: Vec::new(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut steps = db.prepare(
            "SELECT step, state, verdict, reason, outputs, attempt, pgid, process_started_us,
                    process_sid, restarts
             FROM run_steps WHERE run_id = ?1",
        )?;
        for run in &mut runs {
            run.steps = steps
                .query_map(params![run.id.0 as i64], |row| {
                    let state = match row.get::<_, String>(1)?.as_str() {
                        "running" => StepRowState::Running {
                            pgid: row.get::<_, Option<i32>>(6)?.unwrap_or_default(),
                            started_us: row.get::<_, Option<i64>>(7)?.unwrap_or_default(),
                            sid: row.get::<_, Option<i32>>(8)?.unwrap_or_default(),
                        },
                        "interrupted" => StepRowState::Interrupted {
                            restarts: row.get(9)?,
                        },
                        "settled" => StepRowState::Settled {
                            verdict: row
                                .get::<_, Option<String>>(2)?
                                .and_then(|v| serde_json::from_value(v.into()).ok())
                                .unwrap_or(Verdict::Missing),
                            reason: row.get(3)?,
                            outputs: row
                                .get::<_, Option<String>>(4)?
                                .and_then(|o| serde_json::from_str(&o).ok())
                                .unwrap_or_default(),
                        },
                        _ => StepRowState::Pending,
                    };
                    Ok(StepRow {
                        step: row.get(0)?,
                        state,
                        attempt: row.get(5)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
        }
        Ok(runs)
    }

    /// The PR's newest `limit` Runs, newest first.
    pub fn run_summaries(
        &self,
        repo: &RepoName,
        number: u64,
        limit: usize,
    ) -> Result<Vec<RunSummary>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT id, head_sha, gate, end_reason FROM runs
             WHERE repo = ?1 AND number = ?2 ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = query.query_map(
            params![repo.to_string(), number as i64, limit as i64],
            |row| {
                Ok(RunSummary {
                    id: RunId(row.get::<_, i64>(0)? as u64),
                    head_sha: row.get(1)?,
                    gate: parse_gate(&row.get::<_, String>(2)?),
                    end: row
                        .get::<_, Option<String>>(3)?
                        .and_then(|reason| serde_json::from_value(reason.into()).ok()),
                })
            },
        )?;
        rows.collect()
    }

    /// Every PR that has a Run.
    pub fn prs_with_runs(&self) -> Result<Vec<(RepoName, u64)>, StoreError> {
        let db = self.db();
        let mut query =
            db.prepare("SELECT DISTINCT repo, number FROM runs ORDER BY repo, number")?;
        let rows = query.query_map([], |row| {
            Ok((
                parse_repo(&row.get::<_, String>(0)?),
                row.get::<_, i64>(1)? as u64,
            ))
        })?;
        rows.collect()
    }

    pub fn run_exists(&self, run: RunId) -> Result<bool, StoreError> {
        self.db()
            .query_row(
                "SELECT 1 FROM runs WHERE id = ?1",
                params![run.0 as i64],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
    }

    /// Appends to the Run's event journal and returns the event's sequence
    /// number, counting from 1.
    pub fn append_event(&self, run: RunId, event: &str) -> Result<u64, StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM run_events WHERE run_id = ?1",
            params![run.0 as i64],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO run_events (run_id, seq, event) VALUES (?1, ?2, ?3)",
            params![run.0 as i64, seq, event],
        )?;
        tx.commit()?;
        Ok(seq as u64)
    }

    /// The Run's journal after sequence number `after`, in order.
    pub fn events_after(&self, run: RunId, after: u64) -> Result<Vec<(u64, String)>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT seq, event FROM run_events WHERE run_id = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let rows = query.query_map(params![run.0 as i64, after as i64], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get(1)?))
        })?;
        rows.collect()
    }

    /// Whether slopwatch itself pushed `sha` to a PR in `repo` (ADR 0002).
    pub fn pushed_by_slopwatch(&self, repo: &RepoName, sha: &str) -> Result<bool, StoreError> {
        self.db()
            .query_row(
                "SELECT 1 FROM push_journal WHERE repo = ?1 AND sha = ?2",
                params![repo.to_string(), sha],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
    }

    /// Records a SHA slopwatch pushed.
    pub fn record_push(&self, repo: &RepoName, sha: &str) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT OR IGNORE INTO push_journal (repo, sha) VALUES (?1, ?2)",
            params![repo.to_string(), sha],
        )?;
        Ok(())
    }
}

/// A [`StepRow`]'s state as `run_steps` columns.
struct StepColumns {
    state: &'static str,
    verdict: Option<&'static str>,
    reason: Option<String>,
    outputs: Option<String>,
    pgid: Option<i32>,
    started_us: Option<i64>,
    sid: Option<i32>,
    /// `None` keeps the stored count.
    restarts: Option<u32>,
}

impl StepColumns {
    /// A state with nothing else to store, which ends a row of restarts.
    fn state(state: &'static str) -> Self {
        Self {
            state,
            verdict: None,
            reason: None,
            outputs: None,
            pgid: None,
            started_us: None,
            sid: None,
            restarts: Some(0),
        }
    }
}

/// Repo names come from [`RepoName`]'s `Display`, so they always parse.
fn parse_repo(text: &str) -> RepoName {
    text.parse().expect("the store holds owner/name repos")
}

fn gate_str(gate: GateState) -> String {
    json_str(&gate)
}

fn parse_gate(text: &str) -> GateState {
    serde_json::from_value(text.into()).unwrap_or(GateState::Pending)
}

/// A unit enum's serde name.
fn json_str<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => unreachable!("only unit enums are stored by name"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64) -> OpenPr {
        OpenPr {
            number,
            title: format!("PR {number}"),
            url: format!("https://example.test/{number}"),
            draft: false,
            head_sha: "abc".into(),
            base: "main".into(),
            labeled: true,
            base_has_pipeline: false,
            detail: Default::default(),
        }
    }

    #[test]
    fn repos_and_prs_survive_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let first = RepoName::new("o", "first");
        let second = RepoName::new("o", "second");
        {
            let store = Store::open(&path).unwrap();
            store.add_repo(&second).unwrap();
            store.add_repo(&first).unwrap();
            store.add_repo(&second).unwrap();
            store.mark_label_created(&first).unwrap();
            store.put_pr(&first, &pr(3)).unwrap();
            store.put_pr(&first, &pr(4)).unwrap();
            store.remove_pr(&first, 3).unwrap();
        }

        let store = Store::open(&path).unwrap();

        assert_eq!(store.repos().unwrap(), [second.clone(), first.clone()]);
        assert!(store.label_created(&first).unwrap());
        assert!(!store.label_created(&second).unwrap());
        assert_eq!(store.prs().unwrap(), [(first, pr(4))]);
    }

    #[test]
    fn the_database_runs_in_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();

        let mode: String = store
            .db()
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();

        assert_eq!(mode, "wal");
    }

    fn new_run<'a>(repo: &'a RepoName, head_sha: &'a str) -> NewRun<'a> {
        NewRun {
            repo,
            number: 7,
            head_sha,
            base: "main",
            base_sha: "base",
            pipeline: "version: 1",
            steps: vec![NewStep {
                id: "ci".into(),
                plugin: "ci".into(),
                config_hash: "hash".into(),
            }],
        }
    }

    #[test]
    fn a_run_keeps_its_steps_until_it_ends_and_its_summary_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let repo = RepoName::new("o", "r");
        let (first, second) = {
            let store = Store::open(&path).unwrap();
            let first = store.insert_run(&new_run(&repo, "aaa"), 10).unwrap();
            store.end_run(first, EndReason::Superseded, 11).unwrap();
            let second = store.insert_run(&new_run(&repo, "bbb"), 12).unwrap();
            store
                .put_step(
                    second,
                    &StepRow {
                        step: "ci".into(),
                        state: StepRowState::Running {
                            pgid: 42,
                            started_us: 1_000,
                            sid: 7,
                        },
                        attempt: 1,
                    },
                )
                .unwrap();
            store.set_gate(second, GateState::Pending).unwrap();
            (first, second)
        };

        let store = Store::open(&path).unwrap();

        let active = store.active_runs().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, second);
        assert_eq!(
            active[0].steps,
            [StepRow {
                step: "ci".into(),
                state: StepRowState::Running {
                    pgid: 42,
                    started_us: 1_000,
                    sid: 7,
                },
                attempt: 1,
            }]
        );
        let summaries = store.run_summaries(&repo, 7, 10).unwrap();
        assert_eq!(
            summaries.iter().map(|run| run.id).collect::<Vec<_>>(),
            [second, first],
            "newest first"
        );
        assert_eq!(summaries[1].end, Some(EndReason::Superseded));
        assert_eq!(summaries[0].end, None);
    }

    #[test]
    fn the_journal_numbers_each_runs_events_from_one() {
        let store = Store::in_memory();
        let repo = RepoName::new("o", "r");
        let first = store.insert_run(&new_run(&repo, "aaa"), 1).unwrap();
        let second = store.insert_run(&new_run(&repo, "bbb"), 1).unwrap();

        assert_eq!(store.append_event(first, "a").unwrap(), 1);
        assert_eq!(store.append_event(first, "b").unwrap(), 2);
        assert_eq!(store.append_event(second, "c").unwrap(), 1);

        assert_eq!(store.events_after(first, 1).unwrap(), [(2, "b".to_owned())]);
    }

    fn running(attempt: u32) -> StepRow {
        StepRow {
            step: "ci".into(),
            state: StepRowState::Running {
                pgid: 42,
                started_us: 1_000,
                sid: 7,
            },
            attempt,
        }
    }

    #[test]
    fn restarts_count_only_in_a_row_until_the_step_settles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let repo = RepoName::new("o", "r");
        let run = {
            let store = Store::open(&path).unwrap();
            let run = store.insert_run(&new_run(&repo, "aaa"), 1).unwrap();
            store.put_step(run, &running(1)).unwrap();
            assert_eq!(store.interrupt_step(run, "ci").unwrap(), 1);
            run
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.active_runs().unwrap()[0].steps[0].state,
            StepRowState::Interrupted { restarts: 1 },
            "an interrupt waiting for its respawn survives another restart"
        );
        assert_eq!(
            store.interrupt_step(run, "ci").unwrap(),
            1,
            "a Step that wasn't running isn't interrupted again"
        );

        store.put_step(run, &running(2)).unwrap();
        assert_eq!(store.interrupt_step(run, "ci").unwrap(), 2);

        store.put_step(run, &running(3)).unwrap();
        store
            .put_step(
                run,
                &StepRow {
                    step: "ci".into(),
                    state: StepRowState::Settled {
                        verdict: Verdict::Pass,
                        reason: None,
                        outputs: Outputs::default(),
                    },
                    attempt: 3,
                },
            )
            .unwrap();
        store.put_step(run, &running(4)).unwrap();
        assert_eq!(
            store.interrupt_step(run, "ci").unwrap(),
            1,
            "settling breaks the row"
        );
    }
}
