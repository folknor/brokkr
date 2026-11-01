//! Database adapter for litehtml's visual result tables.

use std::path::Path;

use crate::error::DevError;
use crate::visual::db::{self, DbSubject, VisualDb};
use crate::visual::status::Status;

pub(crate) type Approval = db::Approval;
pub(crate) type RunSummary = db::RunSummary;
pub(crate) type ResultRow = db::ResultRow;
pub(crate) type RunCompletion = db::RunCompletion;
pub(crate) type ReferenceStamp<'a> = db::ReferenceStamp<'a>;

/// The width a stored result or approval was measured at.
///
/// Rows written before the store recorded a width (store version 3) read as
/// 800, and that is a fact about them, not a default: brokkr never passed the
/// pipeline a width until then, so it rendered at its own default of 800, and
/// every litehtml configuration that existed until then set
/// `viewport_width = 800` with no fixture override, so the Chrome side was 800
/// as well.
pub(crate) fn recorded_width(width: Option<u32>) -> u32 {
    width.unwrap_or(LEGACY_VIEWPORT_WIDTH)
}

const LEGACY_VIEWPORT_WIDTH: u32 = 800;

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

    /// Stamp the run as finished. Called once the fixture loop has run to the
    /// end; a run without the stamp (interrupted, aborted, killed) stays
    /// `Incomplete` and is hidden from `status` and `approve`.
    pub fn mark_run_complete(&self, run_id: &str) -> Result<(), DevError> {
        self.inner.mark_run_complete(run_id)
    }

    pub fn run_completion(&self, run_id: &str) -> Result<RunCompletion, DevError> {
        self.inner.run_completion(run_id)
    }

    /// `stamp` is the comparison a scored result measured; an `ERROR` row
    /// scored nothing and passes `None`.
    pub fn insert_result(
        &self,
        run_id: &str,
        fixture_id: &str,
        pixel_diff_pct: Option<f64>,
        element_match_pct: Option<f64>,
        status: Status,
        stamp: Option<ReferenceStamp<'_>>,
    ) -> Result<(), DevError> {
        self.inner.insert_result(
            run_id,
            fixture_id,
            pixel_diff_pct,
            element_match_pct,
            status,
            stamp,
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
        viewport_width: u32,
    ) -> Result<(), DevError> {
        self.inner.set_approval(
            fixture_id,
            commit,
            pixel_diff_pct,
            element_match_pct,
            Some(viewport_width),
        )
    }

    pub fn run_summary(&self, run_id: &str) -> Result<Option<RunSummary>, DevError> {
        self.inner.run_summary(run_id)
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
    use crate::visual::status::Status;
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

    fn score(db: &MechanicalDb, run: &str, fixture: &str) {
        let stamp = ReferenceStamp {
            viewport_width: 720,
            reference_id: "ref",
        };
        db.insert_result(
            run,
            fixture,
            Some(1.0),
            Some(99.0),
            Status::Pass,
            Some(stamp),
        )
        .unwrap();
    }

    /// A run that never reached the end of its loop reports as incomplete and
    /// is invisible to the latest-result lookups; finishing it flips both.
    #[test]
    fn unfinished_run_is_incomplete_and_not_the_latest_result() {
        let dir = crate::test_scratch::scratch("litehtml_db", "incomplete");
        let db = MechanicalDb::open(&dir.join("results.db")).unwrap();

        db.insert_run("done", "abc", false).unwrap();
        score(&db, "done", "a");
        db.mark_run_complete("done").unwrap();

        db.insert_run("cut", "abc", false).unwrap();
        score(&db, "cut", "a");

        assert_eq!(db.run_completion("done").unwrap(), RunCompletion::Complete);
        assert_eq!(db.run_completion("cut").unwrap(), RunCompletion::Incomplete);
        assert_eq!(db.run_completion("nope").unwrap(), RunCompletion::Unknown);
        // The partial rows stay readable for diagnosis.
        assert_eq!(db.run_results("cut").unwrap().len(), 1);
        assert_eq!(
            db.latest_result_for_fixture("a").unwrap().unwrap().run_id,
            "done"
        );

        db.mark_run_complete("cut").unwrap();
        assert_eq!(
            db.latest_result_for_fixture("a").unwrap().unwrap().run_id,
            "cut"
        );
    }

    /// A version 1 store upgrades on a read-only open; its runs have unknown
    /// completion and still count as the latest result, as they did before.
    #[test]
    fn version_one_store_upgrades_with_unknown_completion() {
        let dir = crate::test_scratch::scratch("litehtml_db", "v1_upgrade");
        let path = dir.join("results.db");
        {
            let conn = Connection::open(dir.join("litehtml-visual.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE mechanical_runs (
                    run_id TEXT PRIMARY KEY,
                    timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                    [commit] TEXT NOT NULL,
                    dirty INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE mechanical_results (
                    run_id TEXT NOT NULL REFERENCES mechanical_runs(run_id),
                    fixture_id TEXT NOT NULL, pixel_diff_pct REAL,
                    element_match_pct REAL, status TEXT NOT NULL,
                    PRIMARY KEY (run_id, fixture_id));
                CREATE TABLE mechanical_approvals (
                    fixture_id TEXT PRIMARY KEY,
                    approved_at TEXT NOT NULL DEFAULT (datetime('now')),
                    [commit] TEXT NOT NULL, pixel_diff_pct REAL NOT NULL,
                    element_match_pct REAL);
                CREATE TABLE brokkr_imports (migration TEXT PRIMARY KEY);
                INSERT INTO mechanical_runs (run_id, [commit]) VALUES ('old', 'abc');
                INSERT INTO mechanical_results VALUES ('old', 'a', 1.0, 99.0, 'PASS');
                PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let db = MechanicalDb::open_read_only(&path).unwrap();
        assert_eq!(db.run_completion("old").unwrap(), RunCompletion::Unknown);
        assert_eq!(
            db.latest_result_for_fixture("a").unwrap().unwrap().run_id,
            "old"
        );
    }

    /// A scored result keeps the comparison it measured and an approval the
    /// width it was measured at.
    #[test]
    fn results_and_approvals_record_their_comparison() {
        let dir = crate::test_scratch::scratch("litehtml_db", "reference_stamp");
        let db = MechanicalDb::open(&dir.join("results.db")).unwrap();
        db.insert_run("run", "abc", false).unwrap();
        score(&db, "run", "a");
        db.insert_result("run", "b", None, None, Status::Error, None)
            .unwrap();
        db.mark_run_complete("run").unwrap();
        db.set_approval("a", "abc", 1.0, Some(99.0), 720).unwrap();

        let latest = db.latest_result_for_fixture("a").unwrap().unwrap();
        assert_eq!(latest.viewport_width, Some(720));
        assert_eq!(latest.reference_id.as_deref(), Some("ref"));
        let error = db.latest_result_for_fixture("b").unwrap().unwrap();
        assert_eq!(error.viewport_width, None);
        assert_eq!(error.reference_id, None);
        assert_eq!(
            db.get_approval("a").unwrap().unwrap().viewport_width,
            Some(720)
        );
        assert_eq!(db.all_approvals().unwrap()[0].viewport_width, Some(720));
        assert_eq!(db.run_results("run").unwrap().len(), 2);
    }

    /// A version 2 store gains the comparison columns; its rows keep NULL,
    /// which reads as the width every such row was measured at.
    #[test]
    fn version_two_store_upgrades_with_legacy_width() {
        let dir = crate::test_scratch::scratch("litehtml_db", "v2_upgrade");
        let path = dir.join("results.db");
        {
            let conn = Connection::open(dir.join("litehtml-visual.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE mechanical_runs (
                    run_id TEXT PRIMARY KEY,
                    timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                    [commit] TEXT NOT NULL,
                    dirty INTEGER NOT NULL DEFAULT 0, completed INTEGER);
                CREATE TABLE mechanical_results (
                    run_id TEXT NOT NULL REFERENCES mechanical_runs(run_id),
                    fixture_id TEXT NOT NULL, pixel_diff_pct REAL,
                    element_match_pct REAL, status TEXT NOT NULL,
                    PRIMARY KEY (run_id, fixture_id));
                CREATE TABLE mechanical_approvals (
                    fixture_id TEXT PRIMARY KEY,
                    approved_at TEXT NOT NULL DEFAULT (datetime('now')),
                    [commit] TEXT NOT NULL, pixel_diff_pct REAL NOT NULL,
                    element_match_pct REAL);
                CREATE TABLE brokkr_imports (migration TEXT PRIMARY KEY);
                INSERT INTO mechanical_runs (run_id, [commit], completed) VALUES ('old', 'abc', 1);
                INSERT INTO mechanical_results VALUES ('old', 'a', 1.0, 99.0, 'PASS');
                INSERT INTO mechanical_approvals (fixture_id, [commit], pixel_diff_pct, element_match_pct)
                    VALUES ('a', 'abc', 1.0, 99.0);
                PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let db = MechanicalDb::open_read_only(&path).unwrap();
        let latest = db.latest_result_for_fixture("a").unwrap().unwrap();
        assert_eq!(latest.viewport_width, None);
        assert_eq!(latest.reference_id, None);
        assert_eq!(recorded_width(latest.viewport_width), 800);
        let approval = db.get_approval("a").unwrap().unwrap();
        assert_eq!(recorded_width(approval.viewport_width), 800);
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
