//! Database adapter for litehtml's visual result tables.

use std::path::Path;

use crate::error::DevError;
use crate::visual::db::{self, DbSubject, VisualDb};
use crate::visual::status::Status;

pub(crate) type Approval = db::Approval;
pub(crate) type RunSummary = db::RunSummary;
pub(crate) type ResultRow = db::ResultRow;

#[cfg(test)]
const SCHEMA_RUNS: &str = "\
CREATE TABLE IF NOT EXISTS mechanical_runs (
    run_id          TEXT PRIMARY KEY,
    timestamp       TEXT NOT NULL DEFAULT (datetime('now')),
    [commit]        TEXT NOT NULL,
    dirty           INTEGER NOT NULL DEFAULT 0
)";

pub(crate) struct MechanicalDb {
    inner: VisualDb,
}

impl MechanicalDb {
    /// Open litehtml's visual store for writing (`visual`, `approve`),
    /// importing legacy tables from the given `results.db` path on first open.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        Ok(Self {
            inner: VisualDb::open(path, DbSubject::Litehtml)?,
        })
    }

    /// Open litehtml's visual store for queries (`list`, `report`,
    /// `visual-status`). Never writes except to perform the one-time import.
    pub fn open_read_only(path: &Path) -> Result<Self, DevError> {
        Ok(Self {
            inner: VisualDb::open_read_only(path, DbSubject::Litehtml)?,
        })
    }

    pub fn insert_run(&self, run_id: &str, commit: &str, dirty: bool) -> Result<(), DevError> {
        self.inner.insert_run(run_id, commit, dirty, None)
    }

    pub fn insert_result(
        &self,
        run_id: &str,
        fixture_id: &str,
        pixel_diff_pct: Option<f64>,
        element_match_pct: Option<f64>,
        status: Status,
    ) -> Result<(), DevError> {
        self.inner.insert_result(
            run_id,
            fixture_id,
            pixel_diff_pct,
            element_match_pct,
            status,
        )
    }

    pub fn get_approval(&self, fixture_id: &str) -> Result<Option<Approval>, DevError> {
        self.inner.get_approval(fixture_id)
    }

    pub fn set_approval(
        &self,
        fixture_id: &str,
        commit: &str,
        pixel_diff_pct: f64,
        element_match_pct: Option<f64>,
    ) -> Result<(), DevError> {
        self.inner
            .set_approval(fixture_id, commit, pixel_diff_pct, element_match_pct)
    }

    #[allow(dead_code)]
    pub fn latest_run(&self) -> Result<Option<RunSummary>, DevError> {
        self.inner.latest_run()
    }

    pub fn run_results(&self, run_id: &str) -> Result<Vec<ResultRow>, DevError> {
        self.inner.run_results(run_id)
    }

    pub fn all_approvals(&self) -> Result<Vec<Approval>, DevError> {
        self.inner.all_approvals()
    }

    pub fn latest_result_for_fixture(
        &self,
        fixture_id: &str,
    ) -> Result<Option<ResultRow>, DevError> {
        self.inner.latest_result_for_subject(fixture_id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rusqlite::Connection;

    fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
        conn.prepare(&format!("SELECT {column} FROM {table} LIMIT 0"))
            .is_ok()
    }

    /// Opening the litehtml visual store must leave `user_version` untouched
    /// in the legacy `results.db`; the main results store owns that stamp.
    #[test]
    fn open_leaves_user_version_alone() {
        let dir = crate::test_scratch::scratch("litehtml_db", "user_version");
        let path = dir.join("results.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 0).unwrap();
        }
        drop(MechanicalDb::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        let v: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(v, 0);

        conn.pragma_update(None, "user_version", 18).unwrap();
        drop(conn);
        drop(MechanicalDb::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        let v: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(v, 18);
    }

    /// Importing a legacy table omits its retired `artifact_dir` column from
    /// the new litehtml store and leaves the original shared file as it was.
    #[test]
    fn import_omits_retired_artifact_dir_column() {
        let dir = crate::test_scratch::scratch("litehtml_db", "legacy_column");
        let path = dir.join("results.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(SCHEMA_RUNS, []).unwrap();
            conn.execute(
                "CREATE TABLE mechanical_results (
                    run_id TEXT NOT NULL, fixture_id TEXT NOT NULL,
                    pixel_diff_pct REAL, element_match_pct REAL,
                    status TEXT NOT NULL, artifact_dir TEXT,
                    PRIMARY KEY (run_id, fixture_id))",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 18).unwrap();
        }
        drop(MechanicalDb::open(&path).unwrap());
        let legacy = Connection::open(&path).unwrap();
        assert!(has_column(&legacy, "mechanical_results", "artifact_dir"));
        let visual_path = dir.join("litehtml-visual.db");
        let visual = Connection::open(&visual_path).unwrap();
        assert!(!has_column(&visual, "mechanical_results", "artifact_dir"));
        assert!(has_column(&visual, "mechanical_results", "status"));
    }
}
