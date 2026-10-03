//! Draft Pipelines, the node positions the editor keeps beside them, and
//! the journal of commits publishing makes.

use std::collections::BTreeMap;

use rusqlite::types::Type;
use rusqlite::{OptionalExtension, params};
use slopwatch_core::{Conflict, Edit};
use slopwatch_protocol::RepoName;
use slopwatch_protocol::pipeline::{DraftBase, NodePosition, PipelinePr};

use super::{Store, StoreError};

/// A repo's draft as stored: where it started, and the edits since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDraft {
    pub base: DraftBase,
    /// The Pipeline file at the base. `None` when there was none.
    pub base_text: Option<String>,
    pub edits: Vec<Edit>,
    /// The Pipeline PR the draft was last published as.
    pub published: Option<PipelinePr>,
    /// The nodes that stopped the last publish.
    pub conflicts: Vec<Conflict>,
}

impl StoredDraft {
    /// A draft with no edits yet, from `text` at `base`.
    pub fn fresh(base: DraftBase, text: Option<String>) -> Self {
        StoredDraft {
            base,
            base_text: text,
            edits: Vec::new(),
            published: None,
            conflicts: Vec::new(),
        }
    }
}

/// A commit to `slopwatch/pipeline` the daemon set out to make and hasn't
/// settled yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPipelineCommit {
    pub id: i64,
    pub branch: String,
    /// The head the commit was to go on top of.
    pub expected_head: String,
    /// The Pipeline file it writes.
    pub text: String,
}

impl Store {
    pub fn draft(&self, repo: &RepoName) -> Result<Option<StoredDraft>, StoreError> {
        self.db()
            .query_row(
                "SELECT base, base_text, edits, published, conflicts
                 FROM pipeline_drafts WHERE repo = ?1",
                params![repo.to_string()],
                |row| {
                    let base: String = row.get(0)?;
                    let edits: String = row.get(2)?;
                    let published: Option<String> = row.get(3)?;
                    let conflicts: String = row.get(4)?;
                    // A later build may read edits differently. That's an
                    // error for this draft, not a crash for the daemon.
                    let json = |column, error| {
                        StoreError::FromSqlConversionFailure(column, Type::Text, Box::new(error))
                    };
                    Ok(StoredDraft {
                        base: serde_json::from_str(&base).map_err(|e| json(0, e))?,
                        base_text: row.get(1)?,
                        edits: serde_json::from_str(&edits).map_err(|e| json(2, e))?,
                        published: published
                            .map(|text| serde_json::from_str(&text))
                            .transpose()
                            .map_err(|e| json(3, e))?,
                        conflicts: serde_json::from_str(&conflicts).map_err(|e| json(4, e))?,
                    })
                },
            )
            .optional()
    }

    pub fn save_draft(&self, repo: &RepoName, draft: &StoredDraft) -> Result<(), StoreError> {
        let base = serde_json::to_string(&draft.base).expect("bases serialize");
        let edits = serde_json::to_string(&draft.edits).expect("edits serialize");
        let published = draft
            .published
            .as_ref()
            .map(|pr| serde_json::to_string(pr).expect("PRs serialize"));
        let conflicts = serde_json::to_string(&draft.conflicts).expect("conflicts serialize");
        self.db().execute(
            "INSERT INTO pipeline_drafts (repo, base, base_text, edits, published, conflicts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (repo) DO UPDATE SET
                 base = excluded.base, base_text = excluded.base_text, edits = excluded.edits,
                 published = excluded.published, conflicts = excluded.conflicts",
            params![
                repo.to_string(),
                base,
                draft.base_text,
                edits,
                published,
                conflicts
            ],
        )?;
        Ok(())
    }

    /// Records a commit to `branch` the daemon is about to ask GitHub for,
    /// and returns its id.
    pub fn insert_pipeline_commit(
        &self,
        repo: &RepoName,
        branch: &str,
        expected_head: &str,
        text: &str,
    ) -> Result<i64, StoreError> {
        let db = self.db();
        db.execute(
            "INSERT INTO pipeline_commits (repo, branch, expected_head, text)
             VALUES (?1, ?2, ?3, ?4)",
            params![repo.to_string(), branch, expected_head, text],
        )?;
        Ok(db.last_insert_rowid())
    }

    /// Settles a commit: `sha` is the commit GitHub made, which goes in the
    /// push journal with it (ADR 0002), or `None` if it made none.
    pub fn finish_pipeline_commit(
        &self,
        repo: &RepoName,
        id: i64,
        sha: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE pipeline_commits SET sha = ?2, finished = 1 WHERE id = ?1",
            params![id, sha],
        )?;
        if let Some(sha) = sha {
            tx.execute(
                "INSERT OR IGNORE INTO push_journal (repo, sha) VALUES (?1, ?2)",
                params![repo.to_string(), sha],
            )?;
        }
        tx.commit()
    }

    /// The repo's commits that were asked for and never settled, as a
    /// crash leaves them, oldest first.
    pub fn open_pipeline_commits(
        &self,
        repo: &RepoName,
    ) -> Result<Vec<OpenPipelineCommit>, StoreError> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT id, branch, expected_head, text FROM pipeline_commits
             WHERE repo = ?1 AND finished = 0 ORDER BY id",
        )?;
        let rows = query.query_map(params![repo.to_string()], |row| {
            Ok(OpenPipelineCommit {
                id: row.get(0)?,
                branch: row.get(1)?,
                expected_head: row.get(2)?,
                text: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Where the developer put each node of the repo's Pipeline, by Step id
    /// or `gate`.
    pub fn positions(&self, repo: &RepoName) -> Result<BTreeMap<String, NodePosition>, StoreError> {
        let db = self.db();
        let mut query = db.prepare("SELECT node, x, y FROM pipeline_positions WHERE repo = ?1")?;
        let rows = query.query_map(params![repo.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                NodePosition {
                    x: row.get(1)?,
                    y: row.get(2)?,
                },
            ))
        })?;
        rows.collect()
    }

    pub fn set_position(
        &self,
        repo: &RepoName,
        node: &str,
        position: NodePosition,
    ) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT INTO pipeline_positions (repo, node, x, y) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repo, node) DO UPDATE SET x = excluded.x, y = excluded.y",
            params![repo.to_string(), node, position.x, position.y],
        )?;
        Ok(())
    }

    pub fn remove_position(&self, repo: &RepoName, node: &str) -> Result<(), StoreError> {
        self.db().execute(
            "DELETE FROM pipeline_positions WHERE repo = ?1 AND node = ?2",
            params![repo.to_string(), node],
        )?;
        Ok(())
    }

    pub fn clear_positions(&self, repo: &RepoName) -> Result<(), StoreError> {
        self.db().execute(
            "DELETE FROM pipeline_positions WHERE repo = ?1",
            params![repo.to_string()],
        )?;
        Ok(())
    }
}
