//! Draft Pipelines and the node positions the editor keeps beside them.

use std::collections::BTreeMap;

use rusqlite::types::Type;
use rusqlite::{OptionalExtension, params};
use slopwatch_core::Edit;
use slopwatch_protocol::RepoName;
use slopwatch_protocol::pipeline::{DraftBase, NodePosition};

use super::{Store, StoreError};

/// A repo's draft as stored: where it started, and the edits since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDraft {
    pub base: DraftBase,
    /// The Pipeline file at the base. `None` when there was none.
    pub base_text: Option<String>,
    pub edits: Vec<Edit>,
}

impl Store {
    pub fn draft(&self, repo: &RepoName) -> Result<Option<StoredDraft>, StoreError> {
        self.db()
            .query_row(
                "SELECT base, base_text, edits FROM pipeline_drafts WHERE repo = ?1",
                params![repo.to_string()],
                |row| {
                    let base: String = row.get(0)?;
                    let edits: String = row.get(2)?;
                    // A later build may read edits differently. That's an
                    // error for this draft, not a crash for the daemon.
                    let json = |column, error| {
                        StoreError::FromSqlConversionFailure(column, Type::Text, Box::new(error))
                    };
                    Ok(StoredDraft {
                        base: serde_json::from_str(&base).map_err(|e| json(0, e))?,
                        base_text: row.get(1)?,
                        edits: serde_json::from_str(&edits).map_err(|e| json(2, e))?,
                    })
                },
            )
            .optional()
    }

    pub fn save_draft(&self, repo: &RepoName, draft: &StoredDraft) -> Result<(), StoreError> {
        let base = serde_json::to_string(&draft.base).expect("bases serialize");
        let edits = serde_json::to_string(&draft.edits).expect("edits serialize");
        self.db().execute(
            "INSERT INTO pipeline_drafts (repo, base, base_text, edits) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repo) DO UPDATE SET
                 base = excluded.base, base_text = excluded.base_text, edits = excluded.edits",
            params![repo.to_string(), base, draft.base_text, edits],
        )?;
        Ok(())
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
