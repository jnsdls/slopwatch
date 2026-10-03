//! Budgets: what Steps spent, a PR's raised Budget, the lifts "run anyway
//! once" leaves for a PR's next Run, and the daemon's own settings.

use rusqlite::{OptionalExtension, params};
use slopwatch_protocol::{BudgetKind, DaemonSettings, RepoName, RunId};

use super::{Store, StoreError, json_str};

impl Store {
    /// Records one priced model call of `step` in `run`, at `at` seconds.
    pub fn add_usage(&self, run: RunId, step: &str, usd: f64, at: i64) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT INTO step_usage (run_id, step, usd, at) VALUES (?1, ?2, ?3, ?4)",
            params![run.0 as i64, step, usd, at],
        )?;
        Ok(())
    }

    /// What the Runs of the Budget window that `window` opened spent.
    pub fn window_spent(&self, window: RunId) -> Result<f64, StoreError> {
        self.db().query_row(
            "SELECT COALESCE(SUM(step_usage.usd), 0) FROM step_usage
             JOIN runs ON runs.id = step_usage.run_id
             WHERE runs.budget_window = ?1",
            params![window.0 as i64],
            |row| row.get(0),
        )
    }

    /// What every Step spent from `since`, in seconds, on.
    pub fn spent_since(&self, since: i64) -> Result<f64, StoreError> {
        self.db().query_row(
            "SELECT COALESCE(SUM(usd), 0) FROM step_usage WHERE at >= ?1",
            params![since],
            |row| row.get(0),
        )
    }

    /// Raises the PR's Budget to `usd` for the window `window` opened. A
    /// later window, after an outside push, goes back to the Pipeline's.
    pub fn raise_pr_budget(
        &self,
        repo: &RepoName,
        number: u64,
        window: RunId,
        usd: f64,
    ) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT INTO pr_budgets (repo, number, window, usd) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (repo, number) DO UPDATE SET window = excluded.window,
                                                     usd = excluded.usd",
            params![repo.to_string(), number as i64, window.0 as i64, usd],
        )?;
        Ok(())
    }

    /// The PR's raised Budget in the window `window` opened, if raised.
    pub fn raised_pr_budget(
        &self,
        repo: &RepoName,
        number: u64,
        window: RunId,
    ) -> Result<Option<f64>, StoreError> {
        self.db()
            .query_row(
                "SELECT usd FROM pr_budgets WHERE repo = ?1 AND number = ?2 AND window = ?3",
                params![repo.to_string(), number as i64, window.0 as i64],
                |row| row.get(0),
            )
            .optional()
    }

    /// Lets the PR's next Run past `kind`, once.
    pub fn add_lift(
        &self,
        repo: &RepoName,
        number: u64,
        kind: BudgetKind,
    ) -> Result<(), StoreError> {
        self.db().execute(
            "INSERT OR IGNORE INTO budget_lifts (repo, number, kind) VALUES (?1, ?2, ?3)",
            params![repo.to_string(), number as i64, json_str(&kind)],
        )?;
        Ok(())
    }

    /// The lifts waiting for the PR's next Run.
    pub fn lifts(&self, repo: &RepoName, number: u64) -> Result<Vec<BudgetKind>, StoreError> {
        let db = self.db();
        let mut query =
            db.prepare("SELECT kind FROM budget_lifts WHERE repo = ?1 AND number = ?2")?;
        let kinds = query.query_map(params![repo.to_string(), number as i64], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(kinds
            .filter_map(|kind| serde_json::from_value(kind.ok()?.into()).ok())
            .collect())
    }

    /// Spends the PR's lifts on the Run that just started.
    pub fn clear_lifts(&self, repo: &RepoName, number: u64) -> Result<(), StoreError> {
        self.db().execute(
            "DELETE FROM budget_lifts WHERE repo = ?1 AND number = ?2",
            params![repo.to_string(), number as i64],
        )?;
        Ok(())
    }

    /// The daemon's own settings, or the defaults until the developer
    /// saves some.
    pub fn daemon_settings(&self) -> Result<DaemonSettings, StoreError> {
        let saved: Option<String> = self
            .db()
            .query_row(
                "SELECT value FROM settings WHERE name = 'daemon'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(saved
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default())
    }

    pub fn put_daemon_settings(&self, settings: &DaemonSettings) -> Result<(), StoreError> {
        let json = serde_json::to_string(settings).expect("settings always serialize");
        self.db().execute(
            "INSERT INTO settings (name, value) VALUES ('daemon', ?1)
             ON CONFLICT (name) DO UPDATE SET value = excluded.value",
            params![json],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use slopwatch_protocol::Cents;

    use super::*;
    use crate::store::NewRun;

    fn run(store: &Store, window: Option<RunId>) -> RunId {
        store
            .insert_run(
                &NewRun {
                    repo: &RepoName::new("o", "r"),
                    number: 7,
                    head_sha: "abc",
                    base: "main",
                    pr_base: "main",
                    base_sha: "def",
                    pipeline: "",
                    files: &[],
                    linked_issues: &[],
                    steps: Vec::new(),
                    budget_window: window,
                    lifted: &[],
                },
                0,
            )
            .unwrap()
    }

    #[test]
    fn a_window_adds_up_its_runs_and_the_day_adds_up_since_midnight() {
        let store = Store::in_memory();
        let first = run(&store, None);
        let second = run(&store, Some(first));
        let fresh = run(&store, None);
        store.add_usage(first, "review", 1.5, 100).unwrap();
        store.add_usage(second, "review", 2.0, 200).unwrap();
        store.add_usage(fresh, "review", 0.25, 300).unwrap();

        assert_eq!(store.window_spent(first).unwrap(), 3.5);
        assert_eq!(store.window_spent(fresh).unwrap(), 0.25);
        assert_eq!(store.spent_since(200).unwrap(), 2.25);
        assert_eq!(store.run(second).unwrap().unwrap().budget_window, first);
        assert_eq!(store.run(fresh).unwrap().unwrap().budget_window, fresh);
    }

    #[test]
    fn a_raise_holds_for_its_window_and_lifts_wait_for_the_next_run() {
        let store = Store::in_memory();
        let repo = RepoName::new("o", "r");
        let first = run(&store, None);
        store.raise_pr_budget(&repo, 7, first, 20.0).unwrap();

        assert_eq!(store.raised_pr_budget(&repo, 7, first).unwrap(), Some(20.0));
        assert_eq!(store.raised_pr_budget(&repo, 7, RunId(99)).unwrap(), None);

        store.add_lift(&repo, 7, BudgetKind::Pr).unwrap();
        store.add_lift(&repo, 7, BudgetKind::Pr).unwrap();
        assert_eq!(store.lifts(&repo, 7).unwrap(), [BudgetKind::Pr]);
        store.clear_lifts(&repo, 7).unwrap();
        assert!(store.lifts(&repo, 7).unwrap().is_empty());
    }

    #[test]
    fn settings_default_until_saved() {
        let store = Store::in_memory();
        assert_eq!(store.daemon_settings().unwrap(), DaemonSettings::default());
        let off = DaemonSettings { daily_budget: None };
        store.put_daemon_settings(&off).unwrap();
        assert_eq!(store.daemon_settings().unwrap(), off);
        let raised = DaemonSettings {
            daily_budget: Some(Cents(4000)),
        };
        store.put_daemon_settings(&raised).unwrap();
        assert_eq!(store.daemon_settings().unwrap(), raised);
    }
}
