//! The daemon's state on disk: SQLite in WAL mode, with the daemon as the
//! only writer.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use slopwatch_protocol::RepoName;

use crate::github::OpenPr;

pub use rusqlite::Error as StoreError;

/// Each entry upgrades the schema by one version, tracked in
/// `PRAGMA user_version`. Append only.
const MIGRATIONS: &[&str] = &["
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
"];

pub struct Store {
    db: Connection,
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
        Ok(Self { db })
    }

    /// Added repos, oldest first.
    pub fn repos(&self) -> Result<Vec<RepoName>, StoreError> {
        let mut query = self
            .db
            .prepare("SELECT repo FROM repos ORDER BY added, repo")?;
        let rows = query.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|repo| Ok(parse_repo(&repo?)))
            .collect::<Result<_, StoreError>>()
    }

    pub fn add_repo(&self, repo: &RepoName) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT OR IGNORE INTO repos (repo, added)
             VALUES (?1, (SELECT COALESCE(MAX(added), 0) + 1 FROM repos))",
            params![repo.to_string()],
        )?;
        Ok(())
    }

    pub fn label_created(&self, repo: &RepoName) -> Result<bool, StoreError> {
        let created = self
            .db
            .query_row(
                "SELECT label_created FROM repos WHERE repo = ?1",
                params![repo.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(created.unwrap_or(false))
    }

    pub fn mark_label_created(&self, repo: &RepoName) -> Result<(), StoreError> {
        self.db.execute(
            "UPDATE repos SET label_created = 1 WHERE repo = ?1",
            params![repo.to_string()],
        )?;
        Ok(())
    }

    /// Every stored PR with its repo.
    pub fn prs(&self) -> Result<Vec<(RepoName, OpenPr)>, StoreError> {
        let mut query = self.db.prepare(
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
                },
            ))
        })?;
        rows.collect()
    }

    pub fn put_pr(&self, repo: &RepoName, pr: &OpenPr) -> Result<(), StoreError> {
        self.db.execute(
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
        self.db.execute(
            "DELETE FROM prs WHERE repo = ?1 AND number = ?2",
            params![repo.to_string(), number as i64],
        )?;
        Ok(())
    }
}

/// Repo names come from [`RepoName`]'s `Display`, so they always parse.
fn parse_repo(text: &str) -> RepoName {
    text.parse().expect("the store holds owner/name repos")
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
            .db
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();

        assert_eq!(mode, "wal");
    }
}
