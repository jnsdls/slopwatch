//! The daemon's state on disk: SQLite in WAL mode, with the daemon as the
//! only writer. A [`Store`] is a handle on one connection, and clones share
//! it.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use slopwatch_core::{EndReason, GateState, ReuseKey, Verdict};
use slopwatch_protocol::step::{Effect, EffectKind, EffectResult, Outputs};
use slopwatch_protocol::{EntryId, InboxEntry, RepoName, RunId, RunSummary, Waiver};

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
    // Outcome reuse (ADR 0007): the Plugin version each Step ran with, and
    // the Run whose Outcome a Step took instead of running.
    "
    ALTER TABLE run_steps ADD COLUMN plugin_version TEXT NOT NULL DEFAULT '';
    ALTER TABLE run_steps ADD COLUMN reused_from INTEGER REFERENCES runs(id);
",
    // The paths the Run's head changes, as a JSON array, for `files:`
    // Conditions.
    "
    ALTER TABLE runs ADD COLUMN files TEXT NOT NULL DEFAULT '[]';
",
    // Journal timestamps, for interleaving events with Step logs, and when
    // a Run's detail was pruned.
    "
    ALTER TABLE run_events ADD COLUMN ts INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE runs ADD COLUMN pruned_at INTEGER;
",
    // The Effect intent journal (ADR 0009): a row before the daemon calls
    // GitHub, its result after. `request` is the Step's own id for it, and
    // `head_sha` the head the Run expected.
    "
    CREATE TABLE effects (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id INTEGER NOT NULL REFERENCES runs(id),
        step TEXT NOT NULL,
        request TEXT NOT NULL,
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        head_sha TEXT NOT NULL,
        effect TEXT NOT NULL,
        result TEXT,
        UNIQUE (run_id, step, request)
    );
    CREATE INDEX effects_by_head ON effects (repo, head_sha);
",
    // Waivers belong to a PR's head SHA, not to a Run, so every Run on that
    // SHA counts them. `run_id` is the Run they were made from. A Run that
    // ended shippable only because of Waivers is marked `waived`.
    "
    CREATE TABLE waivers (
        repo TEXT NOT NULL,
        number INTEGER NOT NULL,
        head_sha TEXT NOT NULL,
        step TEXT NOT NULL,
        category TEXT NOT NULL,
        reason TEXT NOT NULL,
        actor TEXT NOT NULL,
        run_id INTEGER NOT NULL REFERENCES runs(id),
        waived_at INTEGER NOT NULL,
        PRIMARY KEY (repo, number, head_sha, step)
    );
    ALTER TABLE runs ADD COLUMN waived INTEGER NOT NULL DEFAULT 0;
",
    // The Inbox. An entry is kept forever, open or closed, as part of the
    // record of every Run it touched.
    "
    CREATE TABLE inbox_entries (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        entry TEXT NOT NULL,
        open INTEGER NOT NULL
    );
    CREATE INDEX inbox_open ON inbox_entries (open, id);
    CREATE TABLE inbox_runs (
        entry_id INTEGER NOT NULL REFERENCES inbox_entries(id),
        run_id INTEGER NOT NULL REFERENCES runs(id),
        PRIMARY KEY (entry_id, run_id)
    );
    CREATE INDEX inbox_runs_by_run ON inbox_runs (run_id, entry_id);
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
    /// The paths the head changes since it branched from the base.
    pub files: &'a [String],
    pub steps: Vec<NewStep>,
}

/// A Run's Step as it starts.
pub struct NewStep {
    pub id: String,
    pub plugin: String,
    pub config_hash: String,
}

/// The newest Run of a PR, as a sync compares it with the PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRun {
    pub id: RunId,
    pub head_sha: String,
    pub base_sha: String,
    pub pipeline: String,
}

/// A settled Outcome a later Run may take instead of running its Step.
#[derive(Debug, Clone, PartialEq)]
pub struct Reusable {
    /// The Run where the Step ran and reported it.
    pub run: RunId,
    pub verdict: Verdict,
    pub reason: Option<String>,
    pub outputs: Outputs,
}

/// One journal event as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub seq: u64,
    /// Milliseconds since the epoch, 0 for events from before timestamps.
    pub ts: i64,
    pub event: String,
}

/// A Run whose detail is still on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnprunedRun {
    pub id: RunId,
    /// Seconds since the epoch. `None` while the Run is going.
    pub ended_at: Option<i64>,
    /// The newest Run of its PR.
    pub latest: bool,
    /// The PR is one of the developer's open PRs.
    pub pr_open: bool,
    /// The PR is open and watched.
    pub pr_watched: bool,
    /// What the Run's event journal takes in the database.
    pub journal_bytes: u64,
    /// A Run whose detail is kept reuses one of this Run's Outcomes.
    pub reused: bool,
}

/// One row of the Effect intent journal.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectRow {
    pub id: i64,
    pub run: RunId,
    pub step: String,
    /// The Step's own id for the request.
    pub request: String,
    pub repo: RepoName,
    pub number: u64,
    /// The head the Run expected when the daemon took the request.
    pub head_sha: String,
    pub effect: Effect,
    /// `None` while the intent is open.
    pub result: Option<EffectResult>,
}

/// An Effect intent as the daemon records it, before calling GitHub.
pub struct NewEffect<'a> {
    pub run: RunId,
    pub step: &'a str,
    pub request: &'a str,
    pub repo: &'a RepoName,
    pub number: u64,
    pub head_sha: &'a str,
    pub effect: &'a Effect,
}

/// A Run as the store keeps it: one that hasn't ended, as a restart picks
/// it up, or any Run a developer's command names.
#[derive(Debug, Clone)]
pub struct ActiveRun {
    pub id: RunId,
    pub repo: RepoName,
    pub number: u64,
    pub head_sha: String,
    pub base: String,
    pub base_sha: String,
    pub pipeline: String,
    pub files: Vec<String>,
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
            "INSERT INTO runs (repo, number, head_sha, base, base_sha, pipeline, started_at, files)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run.repo.to_string(),
                run.number as i64,
                run.head_sha,
                run.base,
                run.base_sha,
                run.pipeline,
                now,
                serde_json::to_string(run.files).expect("paths always serialize"),
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

    /// Records the Plugin version a Step is about to run with, which its
    /// Outcome is reused under.
    pub fn set_plugin_version(
        &self,
        run: RunId,
        step: &str,
        version: &str,
    ) -> Result<(), StoreError> {
        self.db().execute(
            "UPDATE run_steps SET plugin_version = ?3 WHERE run_id = ?1 AND step = ?2",
            params![run.0 as i64, step, version],
        )?;
        Ok(())
    }

    /// Settles the Step with an Outcome reused from an earlier Run under
    /// `key`.
    pub fn put_reused_step(
        &self,
        run: RunId,
        key: &ReuseKey,
        reused: &Reusable,
    ) -> Result<(), StoreError> {
        self.db().execute(
            "UPDATE run_steps
             SET state = 'settled', verdict = ?3, reason = ?4, outputs = ?5,
                 plugin_version = ?6, reused_from = ?7,
                 pgid = NULL, process_started_us = NULL, process_sid = NULL
             WHERE run_id = ?1 AND step = ?2",
            params![
                run.0 as i64,
                key.step,
                reused.verdict.as_str(),
                reused.reason,
                serde_json::to_string(&reused.outputs).expect("outputs always serialize"),
                key.plugin_version,
                reused.run.0 as i64,
            ],
        )?;
        Ok(())
    }

    /// The newest Outcome the PR settled under `key` in a Run before
    /// `before`, if its Verdict may be reused. A reused Outcome names the
    /// Run that first reported it. A `waived` Step takes any Verdict but
    /// skipped, since the Waiver already counts it as pass.
    pub fn reusable_outcome(
        &self,
        repo: &RepoName,
        number: u64,
        key: &ReuseKey,
        before: RunId,
        waived: bool,
    ) -> Result<Option<Reusable>, StoreError> {
        let found = self
            .db()
            .query_row(
                "SELECT s.run_id, s.reused_from, s.verdict, s.reason, s.outputs
                 FROM run_steps s JOIN runs r ON r.id = s.run_id
                 WHERE r.repo = ?1 AND r.number = ?2 AND r.head_sha = ?3 AND r.id < ?4
                   AND s.step = ?5 AND s.config_hash = ?6 AND s.plugin_version = ?7
                   AND s.state = 'settled'
                 ORDER BY s.run_id DESC LIMIT 1",
                params![
                    repo.to_string(),
                    number as i64,
                    key.head_sha,
                    before.0 as i64,
                    key.step,
                    key.config_hash,
                    key.plugin_version,
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((run, reused_from, verdict, reason, outputs)) = found else {
            return Ok(None);
        };
        let Some(verdict) = verdict
            .and_then(|v| serde_json::from_value::<Verdict>(v.into()).ok())
            .filter(|verdict| verdict.reusable() || (waived && *verdict != Verdict::Skipped))
        else {
            return Ok(None);
        };
        Ok(Some(Reusable {
            run: RunId(reused_from.unwrap_or(run) as u64),
            verdict,
            reason,
            outputs: outputs
                .and_then(|o| serde_json::from_str(&o).ok())
                .unwrap_or_default(),
        }))
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
        self.runs_where("end_reason IS NULL", [])
    }

    /// Run `run` with its Steps, ended or not.
    pub fn run(&self, run: RunId) -> Result<Option<ActiveRun>, StoreError> {
        Ok(self.runs_where("id = ?1", params![run.0 as i64])?.pop())
    }

    fn runs_where(
        &self,
        filter: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<ActiveRun>, StoreError> {
        let db = self.db();
        let mut runs = db
            .prepare(&format!(
                "SELECT id, repo, number, head_sha, base, base_sha, pipeline, gate, files
                 FROM runs WHERE {filter} ORDER BY id"
            ))?
            .query_map(args, |row| {
                Ok(ActiveRun {
                    id: RunId(row.get::<_, i64>(0)? as u64),
                    repo: parse_repo(&row.get::<_, String>(1)?),
                    number: row.get::<_, i64>(2)? as u64,
                    head_sha: row.get(3)?,
                    base: row.get(4)?,
                    base_sha: row.get(5)?,
                    pipeline: row.get(6)?,
                    gate: parse_gate(&row.get::<_, String>(7)?),
                    files: serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default(),
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
            "SELECT id, head_sha, gate, end_reason, waived FROM runs
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
                    waived: row.get(4)?,
                })
            },
        )?;
        rows.collect()
    }

    /// The PR's newest Run, ended or not.
    pub fn latest_run(
        &self,
        repo: &RepoName,
        number: u64,
    ) -> Result<Option<LatestRun>, StoreError> {
        self.db()
            .query_row(
                "SELECT id, head_sha, base_sha, pipeline FROM runs
                 WHERE repo = ?1 AND number = ?2 ORDER BY id DESC LIMIT 1",
                params![repo.to_string(), number as i64],
                |row| {
                    Ok(LatestRun {
                        id: RunId(row.get::<_, i64>(0)? as u64),
                        head_sha: row.get(1)?,
                        base_sha: row.get(2)?,
                        pipeline: row.get(3)?,
                    })
                },
            )
            .optional()
    }

    /// The id of the PR's newest Run, ended or not.
    pub fn latest_run_id(&self, repo: &RepoName, number: u64) -> Result<Option<RunId>, StoreError> {
        self.db()
            .query_row(
                "SELECT MAX(id) FROM runs WHERE repo = ?1 AND number = ?2",
                params![repo.to_string(), number as i64],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map(|id| id.map(|id| RunId(id as u64)))
    }

    /// Marks an ended Run as shippable only because of Waivers.
    pub fn mark_waived(&self, run: RunId) -> Result<(), StoreError> {
        self.db().execute(
            "UPDATE runs SET waived = 1 WHERE id = ?1",
            params![run.0 as i64],
        )?;
        Ok(())
    }

    /// Records a Waiver on `step` for the head SHA of Run `run`, the Run it
    /// was made from. A Step has at most one Waiver per SHA, so a second
    /// one is ignored.
    pub fn add_waiver(
        &self,
        run: RunId,
        step: &str,
        waiver: &Waiver,
        now: i64,
    ) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT OR IGNORE INTO waivers
                 (repo, number, head_sha, step, category, reason, actor, run_id, waived_at)
             SELECT repo, number, head_sha, ?2, ?3, ?4, ?5, id, ?6 FROM runs WHERE id = ?1",
            params![
                run.0 as i64,
                step,
                json_str(&waiver.category),
                waiver.reason,
                serde_json::to_string(&waiver.actor).expect("actors always serialize"),
                now,
            ],
        )?;
        Ok(())
    }

    /// The Steps a Waiver covers on the PR's head SHA.
    pub fn waived_steps(
        &self,
        repo: &RepoName,
        number: u64,
        head_sha: &str,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        Ok(self
            .waivers(repo, number, head_sha)?
            .into_iter()
            .map(|(step, _)| step)
            .collect())
    }

    /// Every Waiver on the PR's head SHA, by Step, oldest first.
    pub fn waivers(
        &self,
        repo: &RepoName,
        number: u64,
        head_sha: &str,
    ) -> Result<Vec<(String, Waiver)>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT step, category, reason, actor FROM waivers
             WHERE repo = ?1 AND number = ?2 AND head_sha = ?3 ORDER BY waived_at, rowid",
        )?;
        let rows = query.query_map(params![repo.to_string(), number as i64, head_sha], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut waivers = Vec::new();
        for row in rows {
            let (step, category, reason, actor) = row?;
            // Only this build writes the table, so both always parse.
            let (Ok(category), Ok(actor)) = (
                serde_json::from_value(category.into()),
                serde_json::from_str(&actor),
            ) else {
                continue;
            };
            waivers.push((
                step,
                Waiver {
                    category,
                    reason,
                    actor,
                },
            ));
        }
        Ok(waivers)
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

    /// The latest attempt of the Run's `step`, or `None` if the Run has
    /// no such Step.
    pub fn step_attempt(&self, run: RunId, step: &str) -> Result<Option<u32>, StoreError> {
        self.db()
            .query_row(
                "SELECT attempt FROM run_steps WHERE run_id = ?1 AND step = ?2",
                params![run.0 as i64, step],
                |row| row.get(0),
            )
            .optional()
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
    /// number, counting from 1. `ts` is in milliseconds since the epoch.
    pub fn append_event(&self, run: RunId, ts: i64, event: &str) -> Result<u64, StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        let seq = append_event(&tx, run, ts, event)?;
        tx.commit()?;
        Ok(seq)
    }

    /// The Run's journal after sequence number `after`, in order, with
    /// each event's timestamp.
    pub fn events_after(&self, run: RunId, after: u64) -> Result<Vec<StoredEvent>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT seq, ts, event FROM run_events WHERE run_id = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let rows = query.query_map(params![run.0 as i64, after as i64], |row| {
            Ok(StoredEvent {
                seq: row.get::<_, i64>(0)? as u64,
                ts: row.get(1)?,
                event: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Prunes a Run's journal: keeps only the events in `keep`, appends
    /// `pruned`, and marks the Run pruned at `at`, in seconds.
    pub fn prune_journal(
        &self,
        run: RunId,
        keep: &[u64],
        ts: i64,
        pruned: &str,
        at: i64,
    ) -> Result<u64, StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        // The pruned event numbers on from the last event ever journalled,
        // which may be among those deleted.
        let seq = append_event(&tx, run, ts, pruned)?;
        let mut kept = tx.prepare("SELECT seq FROM run_events WHERE run_id = ?1")?;
        let all: Vec<i64> = kept
            .query_map(params![run.0 as i64], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        drop(kept);
        for old in all {
            if old as u64 != seq && !keep.contains(&(old as u64)) {
                tx.execute(
                    "DELETE FROM run_events WHERE run_id = ?1 AND seq = ?2",
                    params![run.0 as i64, old],
                )?;
            }
        }
        tx.execute(
            "UPDATE runs SET pruned_at = ?2 WHERE id = ?1",
            params![run.0 as i64, at],
        )?;
        tx.commit()?;
        Ok(seq)
    }

    /// When the Run's detail was pruned, in seconds, if it was.
    pub fn pruned_at(&self, run: RunId) -> Result<Option<i64>, StoreError> {
        self.db()
            .query_row(
                "SELECT pruned_at FROM runs WHERE id = ?1",
                params![run.0 as i64],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
    }

    /// Every Run whose detail hasn't been pruned, oldest first, with what
    /// retention needs to know about it.
    pub fn unpruned_runs(&self) -> Result<Vec<UnprunedRun>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT r.id, r.ended_at,
                    r.id = (SELECT MAX(id) FROM runs l WHERE l.repo = r.repo AND l.number = r.number),
                    p.labeled,
                    (SELECT COALESCE(SUM(LENGTH(event)), 0) FROM run_events e WHERE e.run_id = r.id),
                    EXISTS (SELECT 1 FROM run_steps s JOIN runs k ON k.id = s.run_id
                            WHERE s.reused_from = r.id AND k.id != r.id AND k.pruned_at IS NULL)
             FROM runs r LEFT JOIN prs p ON p.repo = r.repo AND p.number = r.number
             WHERE r.pruned_at IS NULL ORDER BY r.id",
        )?;
        let rows = query.query_map([], |row| {
            let labeled: Option<bool> = row.get(3)?;
            Ok(UnprunedRun {
                id: RunId(row.get::<_, i64>(0)? as u64),
                ended_at: row.get(1)?,
                latest: row.get(2)?,
                pr_open: labeled.is_some(),
                pr_watched: labeled.unwrap_or(false),
                journal_bytes: row.get::<_, i64>(4)? as u64,
                reused: row.get(5)?,
            })
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

    /// Records a new Inbox entry and the Runs it touches, and returns its
    /// id. The id in `entry` is ignored.
    pub fn insert_entry(&self, entry: &InboxEntry, runs: &[RunId]) -> Result<EntryId, StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        tx.execute("INSERT INTO inbox_entries (entry, open) VALUES ('', 1)", [])?;
        let id = EntryId(tx.last_insert_rowid() as u64);
        let entry = InboxEntry {
            id,
            ..entry.clone()
        };
        tx.execute(
            "UPDATE inbox_entries SET entry = ?2, open = ?3 WHERE id = ?1",
            params![id.0 as i64, entry_json(&entry), entry.closed.is_none()],
        )?;
        for run in runs {
            link_entry(&tx, id, *run)?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Stores what changed in an entry, and links it to `run`, a Run it
    /// now touches too.
    pub fn put_entry(&self, entry: &InboxEntry, run: Option<RunId>) -> Result<(), StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "UPDATE inbox_entries SET entry = ?2, open = ?3 WHERE id = ?1",
            params![entry.id.0 as i64, entry_json(entry), entry.closed.is_none()],
        )?;
        if let Some(run) = run {
            link_entry(&tx, entry.id, run)?;
        }
        tx.commit()
    }

    /// Every open Inbox entry, oldest first, with the Runs it touches.
    pub fn open_entries(&self) -> Result<Vec<(InboxEntry, Vec<RunId>)>, StoreError> {
        let db = self.db();
        let mut entries =
            db.prepare("SELECT id, entry FROM inbox_entries WHERE open = 1 ORDER BY id")?;
        let mut runs =
            db.prepare("SELECT run_id FROM inbox_runs WHERE entry_id = ?1 ORDER BY run_id")?;
        let rows = entries
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut open = Vec::new();
        for (id, text) in rows {
            let Ok(entry) = serde_json::from_str::<InboxEntry>(&text) else {
                eprintln!("slopwatchd: Inbox entry {id} doesn't read back");
                continue;
            };
            let touched = runs
                .query_map(params![id], |row| Ok(RunId(row.get::<_, i64>(0)? as u64)))?
                .collect::<Result<Vec<_>, _>>()?;
            open.push((entry, touched));
        }
        Ok(open)
    }

    /// The Inbox entries that touched `run`, open or closed, oldest first:
    /// its Inbox history.
    pub fn run_entries(&self, run: RunId) -> Result<Vec<InboxEntry>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT e.entry FROM inbox_entries e JOIN inbox_runs r ON r.entry_id = e.id
             WHERE r.run_id = ?1 ORDER BY e.id",
        )?;
        let rows = query.query_map(params![run.0 as i64], |row| row.get::<_, String>(0))?;
        Ok(rows
            .filter_map(|text| serde_json::from_str(&text.ok()?).ok())
            .collect())
    }

    /// Records a SHA slopwatch pushed.
    pub fn record_push(&self, repo: &RepoName, sha: &str) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT OR IGNORE INTO push_journal (repo, sha) VALUES (?1, ?2)",
            params![repo.to_string(), sha],
        )?;
        Ok(())
    }

    /// Records an open Effect intent.
    pub fn insert_effect(&self, effect: &NewEffect<'_>) -> Result<EffectRow, StoreError> {
        let db = self.db();
        db.execute(
            "INSERT INTO effects (run_id, step, request, repo, number, head_sha, effect)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                effect.run.0 as i64,
                effect.step,
                effect.request,
                effect.repo.to_string(),
                effect.number as i64,
                effect.head_sha,
                to_json(effect.effect),
            ],
        )?;
        Ok(EffectRow {
            id: db.last_insert_rowid(),
            run: effect.run,
            step: effect.step.to_owned(),
            request: effect.request.to_owned(),
            repo: effect.repo.clone(),
            number: effect.number,
            head_sha: effect.head_sha.to_owned(),
            effect: effect.effect.clone(),
            result: None,
        })
    }

    /// Closes an intent with what became of it and appends `event`, which
    /// says so, to its Run's journal, both or neither. Returns the event's
    /// sequence number.
    pub fn finish_effect(
        &self,
        id: i64,
        result: &EffectResult,
        run: RunId,
        ts: i64,
        event: &str,
    ) -> Result<u64, StoreError> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        tx.execute(
            "UPDATE effects SET result = ?2 WHERE id = ?1",
            params![id, to_json(result)],
        )?;
        let seq = append_event(&tx, run, ts, event)?;
        tx.commit()?;
        Ok(seq)
    }

    /// The intent a Step recorded under its own `request` id in a Run.
    pub fn effect_by_request(
        &self,
        run: RunId,
        step: &str,
        request: &str,
    ) -> Result<Option<EffectRow>, StoreError> {
        self.db()
            .query_row(
                &format!("{SELECT_EFFECT} WHERE run_id = ?1 AND step = ?2 AND request = ?3"),
                params![run.0 as i64, step, request],
                effect_row,
            )
            .optional()
    }

    /// Intents the daemon recorded but never closed, oldest first.
    pub fn open_effects(&self) -> Result<Vec<EffectRow>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(&format!("{SELECT_EFFECT} WHERE result IS NULL ORDER BY id"))?;
        let rows = query.query_map([], effect_row)?;
        rows.collect()
    }

    /// Whether any Run asked GitHub to rerun the check named `check` on
    /// `head_sha` in `repo`. A rerun GitHub refused counts too: after a
    /// crash the daemon redoes a rerun, and GitHub refuses one already
    /// under way, so a refusal can't tell that the check never reran.
    pub fn reran(&self, repo: &RepoName, head_sha: &str, check: &str) -> Result<bool, StoreError> {
        self.db()
            .query_row(
                "SELECT 1 FROM effects
                 WHERE repo = ?1 AND head_sha = ?2
                   AND json_extract(effect, '$.kind') = 'rerun'
                   AND json_extract(effect, '$.check') = ?3
                   AND (result IS NULL
                        OR json_extract(result, '$.status') IN ('done', 'failed'))
                 LIMIT 1",
                params![repo.to_string(), head_sha, check],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
    }

    /// Whether a Step in `run` asked for a `kind` Effect, and GitHub took
    /// it or hasn't answered yet.
    pub fn asked_for(&self, run: RunId, kind: EffectKind) -> Result<bool, StoreError> {
        asked_for(&self.db(), run, kind)
    }

    /// How many of the PR's Runs before `run`, newest first and in a row,
    /// ended with a rebase slopwatch made (ADR 0004). The Run after each
    /// one was rebase-started.
    pub fn rebase_streak(
        &self,
        repo: &RepoName,
        number: u64,
        run: RunId,
    ) -> Result<u32, StoreError> {
        let db = self.db();
        let mut earlier = db.prepare(
            "SELECT id, end_reason FROM runs
             WHERE repo = ?1 AND number = ?2 AND id < ?3 ORDER BY id DESC",
        )?;
        let rows = earlier.query_map(
            params![repo.to_string(), number as i64, run.0 as i64],
            |row| {
                Ok((
                    RunId(row.get::<_, i64>(0)? as u64),
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )?;
        let mut streak = 0;
        for row in rows {
            let (id, reason) = row?;
            let pushed = reason.as_deref() == Some(&json_str(&EndReason::Pushed));
            if !pushed || !asked_for(&db, id, EffectKind::Rebase)? {
                break;
            }
            streak += 1;
        }
        Ok(streak)
    }
}

fn asked_for(db: &Connection, run: RunId, kind: EffectKind) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT 1 FROM effects
         WHERE run_id = ?1 AND json_extract(effect, '$.kind') = ?2
           AND (result IS NULL
                OR json_extract(result, '$.status') IN ('done', 'enqueued'))
         LIMIT 1",
        params![run.0 as i64, kind.to_string()],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

const SELECT_EFFECT: &str =
    "SELECT id, run_id, step, request, repo, number, head_sha, effect, result FROM effects";

fn effect_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EffectRow> {
    fn json<T: serde::de::DeserializeOwned>(index: usize, text: String) -> rusqlite::Result<T> {
        serde_json::from_str(&text).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    }
    Ok(EffectRow {
        id: row.get(0)?,
        run: RunId(row.get::<_, i64>(1)? as u64),
        step: row.get(2)?,
        request: row.get(3)?,
        repo: parse_repo(&row.get::<_, String>(4)?),
        number: row.get::<_, i64>(5)? as u64,
        head_sha: row.get(6)?,
        effect: json(7, row.get(7)?)?,
        result: row
            .get::<_, Option<String>>(8)?
            .map(|text| json(8, text))
            .transpose()?,
    })
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("Effects and their results always serialize")
}

fn append_event(db: &Connection, run: RunId, ts: i64, event: &str) -> Result<u64, StoreError> {
    let seq: i64 = db.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM run_events WHERE run_id = ?1",
        params![run.0 as i64],
        |row| row.get(0),
    )?;
    db.execute(
        "INSERT INTO run_events (run_id, seq, ts, event) VALUES (?1, ?2, ?3, ?4)",
        params![run.0 as i64, seq, ts, event],
    )?;
    Ok(seq as u64)
}

fn link_entry(
    tx: &rusqlite::Transaction<'_>,
    entry: EntryId,
    run: RunId,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT OR IGNORE INTO inbox_runs (entry_id, run_id) VALUES (?1, ?2)",
        params![entry.0 as i64, run.0 as i64],
    )?;
    Ok(())
}

fn entry_json(entry: &InboxEntry) -> String {
    serde_json::to_string(entry).expect("Inbox entries always serialize")
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
            files: &[],
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

        assert_eq!(store.append_event(first, 10, "a").unwrap(), 1);
        assert_eq!(store.append_event(first, 20, "b").unwrap(), 2);
        assert_eq!(store.append_event(second, 30, "c").unwrap(), 1);

        assert_eq!(
            store.events_after(first, 1).unwrap(),
            [StoredEvent {
                seq: 2,
                ts: 20,
                event: "b".to_owned()
            }]
        );
    }

    #[test]
    fn pruning_a_journal_keeps_the_listed_events_and_numbers_on() {
        let store = Store::in_memory();
        let repo = RepoName::new("o", "r");
        let run = store.insert_run(&new_run(&repo, "aaa"), 1).unwrap();
        for event in ["a", "b", "c", "d"] {
            store.append_event(run, 1, event).unwrap();
        }
        assert_eq!(store.pruned_at(run).unwrap(), None);
        assert_eq!(store.unpruned_runs().unwrap().len(), 1);

        let seq = store.prune_journal(run, &[1, 3], 5, "pruned", 99).unwrap();

        assert_eq!(seq, 5);
        let left: Vec<_> = store
            .events_after(run, 0)
            .unwrap()
            .into_iter()
            .map(|stored| (stored.seq, stored.event))
            .collect();
        assert_eq!(
            left,
            [
                (1, "a".to_owned()),
                (3, "c".to_owned()),
                (5, "pruned".to_owned())
            ]
        );
        assert_eq!(store.pruned_at(run).unwrap(), Some(99));
        assert!(store.unpruned_runs().unwrap().is_empty());
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

    /// Runs `ci` in `run` with version `1+exe` and settles it.
    fn settle(store: &Store, run: RunId, verdict: Verdict) {
        store.set_plugin_version(run, "ci", "1+exe").unwrap();
        let row = StepRow {
            step: "ci".into(),
            state: StepRowState::Settled {
                verdict,
                reason: Some(format!("why in {run}")),
                outputs: Outputs {
                    note: Some(format!("in {run}")),
                    ..Outputs::default()
                },
            },
            attempt: 1,
        };
        store.put_step(run, &row).unwrap();
    }

    fn ci_key(head_sha: &str) -> ReuseKey {
        ReuseKey {
            head_sha: head_sha.into(),
            step: "ci".into(),
            config_hash: "hash".into(),
            plugin_version: "1+exe".into(),
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

    #[test]
    fn a_settled_outcome_is_reused_under_its_key_and_names_the_run_that_reported_it() {
        let store = Store::in_memory();
        let repo = RepoName::new("o", "r");
        let first = store.insert_run(&new_run(&repo, "aaa"), 1).unwrap();
        settle(&store, first, Verdict::Fail);
        let second = store.insert_run(&new_run(&repo, "aaa"), 2).unwrap();
        let third = store.insert_run(&new_run(&repo, "aaa"), 3).unwrap();

        let found = store
            .reusable_outcome(&repo, 7, &ci_key("aaa"), second, false)
            .unwrap()
            .expect("the first Run's fail is reusable");
        assert_eq!((found.run, found.verdict), (first, Verdict::Fail));
        assert_eq!(found.outputs.note.as_deref(), Some("in 1"));
        assert_eq!(found.reason.as_deref(), Some("why in 1"));

        store
            .put_reused_step(second, &ci_key("aaa"), &found)
            .unwrap();
        let again = store
            .reusable_outcome(&repo, 7, &ci_key("aaa"), third, false)
            .unwrap()
            .unwrap();
        assert_eq!(again.run, first, "a reuse of a reuse names the first");

        assert_eq!(
            store
                .reusable_outcome(&repo, 7, &ci_key("bbb"), third, false)
                .unwrap(),
            None,
            "another head SHA"
        );
        let other_version = ReuseKey {
            plugin_version: "2+exe".into(),
            ..ci_key("aaa")
        };
        assert_eq!(
            store
                .reusable_outcome(&repo, 7, &other_version, third, false)
                .unwrap(),
            None
        );
    }

    #[test]
    fn the_newest_outcome_decides_and_an_errored_one_is_not_reused() {
        let store = Store::in_memory();
        let repo = RepoName::new("o", "r");
        let first = store.insert_run(&new_run(&repo, "aaa"), 1).unwrap();
        settle(&store, first, Verdict::Pass);
        let second = store.insert_run(&new_run(&repo, "aaa"), 2).unwrap();
        settle(&store, second, Verdict::Error);
        let third = store.insert_run(&new_run(&repo, "aaa"), 3).unwrap();

        assert_eq!(
            store
                .reusable_outcome(&repo, 7, &ci_key("aaa"), third, false)
                .unwrap(),
            None
        );
        let waived = store
            .reusable_outcome(&repo, 7, &ci_key("aaa"), third, true)
            .unwrap()
            .expect("a waived Step keeps its errored Outcome");
        assert_eq!((waived.run, waived.verdict), (second, Verdict::Error));
    }

    #[test]
    fn waivers_belong_to_the_head_sha_and_a_second_one_on_a_step_is_ignored() {
        let store = Store::in_memory();
        let repo = RepoName::new("o", "r");
        let first = store.insert_run(&new_run(&repo, "aaa"), 1).unwrap();
        let waiver = |reason: &str| Waiver {
            category: slopwatch_core::WaiverCategory::AcceptedRisk,
            reason: reason.into(),
            actor: slopwatch_protocol::Actor::Developer { via: "gui".into() },
        };
        store.add_waiver(first, "ci", &waiver("first"), 5).unwrap();
        store.add_waiver(first, "ci", &waiver("second"), 6).unwrap();
        store.end_run(first, EndReason::Shippable, 7).unwrap();
        store.mark_waived(first).unwrap();
        let pushed = store.insert_run(&new_run(&repo, "bbb"), 8).unwrap();

        assert_eq!(
            store.waivers(&repo, 7, "aaa").unwrap(),
            [("ci".to_owned(), waiver("first"))]
        );
        assert!(store.waivers(&repo, 7, "bbb").unwrap().is_empty());
        assert_eq!(store.latest_run_id(&repo, 7).unwrap(), Some(pushed));
        let summaries = store.run_summaries(&repo, 7, 5).unwrap();
        assert_eq!(
            summaries.iter().map(|s| s.waived).collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(store.run(first).unwrap().unwrap().head_sha, "aaa");
        assert!(store.run(RunId(99)).unwrap().is_none());
    }

    fn intent<'a>(
        run: RunId,
        repo: &'a RepoName,
        request: &'a str,
        effect: &'a Effect,
    ) -> NewEffect<'a> {
        NewEffect {
            run,
            step: "ci",
            request,
            repo,
            number: 7,
            head_sha: "aaa",
            effect,
        }
    }

    fn finish(store: &Store, row: &EffectRow, result: EffectResult) {
        store
            .finish_effect(row.id, &result, row.run, 1, "{}")
            .unwrap();
    }

    #[test]
    fn an_effect_intent_stays_open_across_a_restart_until_finished() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let repo = RepoName::new("o", "r");
        let comment = Effect::Comment { body: "hi".into() };
        let (run, row) = {
            let store = Store::open(&path).unwrap();
            let run = store.insert_run(&new_run(&repo, "aaa"), 1).unwrap();
            let row = store
                .insert_effect(&intent(run, &repo, "hello", &comment))
                .unwrap();
            (run, row)
        };

        let store = Store::open(&path).unwrap();
        assert_eq!(store.open_effects().unwrap(), std::slice::from_ref(&row));
        assert_eq!(row.effect, comment);

        finish(&store, &row, EffectResult::Done);
        assert!(store.open_effects().unwrap().is_empty());
        let found = store.effect_by_request(run, "ci", "hello").unwrap();
        assert_eq!(found.unwrap().result, Some(EffectResult::Done));
        assert_eq!(store.effect_by_request(run, "ci", "other").unwrap(), None);
        assert_eq!(
            store.events_after(run, 0).unwrap().len(),
            1,
            "closing the intent journals its event"
        );
    }

    #[test]
    fn a_check_counts_as_rerun_on_a_sha_once_github_was_asked() {
        let store = Store::in_memory();
        let repo = RepoName::new("o", "r");
        let run = store.insert_run(&new_run(&repo, "aaa"), 1).unwrap();
        let rerun = |request: &str, check: &str| {
            let effect = Effect::Rerun {
                check: check.into(),
                job: 1,
            };
            store
                .insert_effect(&intent(run, &repo, request, &effect))
                .unwrap()
        };

        let test = rerun("a", "test");
        let lint = rerun("b", "lint");
        let build = rerun("c", "build");
        let reason = "403".to_owned();
        finish(&store, &lint, EffectResult::Failed { reason });
        let reason = "the Run ended while the daemon was down".to_owned();
        finish(&store, &build, EffectResult::Dropped { reason });

        assert!(store.reran(&repo, "aaa", "test").unwrap(), "while open");
        finish(&store, &test, EffectResult::Done);
        assert!(store.reran(&repo, "aaa", "test").unwrap());
        assert!(!store.reran(&repo, "bbb", "test").unwrap(), "another SHA");
        assert!(
            store.reran(&repo, "aaa", "lint").unwrap(),
            "GitHub refused it"
        );
        assert!(!store.reran(&repo, "aaa", "build").unwrap(), "never asked");
    }

    #[test]
    fn the_rebase_streak_counts_runs_in_a_row_that_ended_with_a_rebase() {
        let store = Store::in_memory();
        let repo = RepoName::new("o", "r");
        let rebase = Effect::Rebase {
            method: slopwatch_protocol::step::UpdateMethod::Rebase,
        };
        let run = |head: &str, at: i64, rebased: Option<EffectResult>, ended: EndReason| {
            let run = store.insert_run(&new_run(&repo, head), at).unwrap();
            if let Some(result) = rebased {
                let row = store
                    .insert_effect(&intent(run, &repo, "rebase", &rebase))
                    .unwrap();
                finish(&store, &row, result);
            }
            store.end_run(run, ended, at).unwrap();
            run
        };
        let failed = EffectResult::Failed {
            reason: "conflict".into(),
        };

        run("a", 1, Some(EffectResult::Done), EndReason::Pushed);
        run("b", 2, None, EndReason::Superseded);
        run("c", 3, Some(EffectResult::Done), EndReason::Pushed);
        let refused = run("d", 4, Some(failed), EndReason::NotShippable);
        run("e", 5, Some(EffectResult::Done), EndReason::Pushed);
        run("f", 6, Some(EffectResult::Done), EndReason::Pushed);
        let current = store.insert_run(&new_run(&repo, "g"), 7).unwrap();

        assert_eq!(store.rebase_streak(&repo, 7, current).unwrap(), 2);
        assert_eq!(store.rebase_streak(&repo, 7, refused).unwrap(), 1);
        assert!(
            store
                .asked_for(current, EffectKind::Rebase)
                .is_ok_and(|asked| !asked)
        );
    }
}
