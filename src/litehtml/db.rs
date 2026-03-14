//! Database tables for the litehtml visual reference testing system.

use std::path::Path;

use rusqlite::Connection;

use crate::error::DevError;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS mechanical_runs (
    run_id          TEXT PRIMARY KEY,
    timestamp       TEXT NOT NULL DEFAULT (datetime('now')),
    commit          TEXT NOT NULL,
    dirty           INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS mechanical_results (
    run_id          TEXT NOT NULL REFERENCES mechanical_runs(run_id),
    fixture_id      TEXT NOT NULL,
    pixel_diff_pct  REAL,
    element_match_pct REAL,
    status          TEXT NOT NULL,
    artifact_dir    TEXT,
    PRIMARY KEY (run_id, fixture_id)
);

CREATE TABLE IF NOT EXISTS mechanical_approvals (
    fixture_id          TEXT PRIMARY KEY,
    approved_at         TEXT NOT NULL DEFAULT (datetime('now')),
    commit              TEXT NOT NULL,
    pixel_diff_pct      REAL NOT NULL,
    element_match_pct   REAL
);
";

#[allow(dead_code)]
pub(crate) struct Approval {
    pub fixture_id: String,
    pub approved_at: String,
    pub commit: String,
    pub pixel_diff_pct: f64,
    pub element_match_pct: Option<f64>,
}

#[allow(dead_code)]
pub(crate) struct RunSummary {
    pub run_id: String,
    pub timestamp: String,
    pub commit: String,
    pub dirty: bool,
}

#[allow(dead_code)]
pub(crate) struct ResultRow {
    pub run_id: String,
    pub fixture_id: String,
    pub pixel_diff_pct: Option<f64>,
    pub element_match_pct: Option<f64>,
    pub status: String,
    pub artifact_dir: Option<String>,
}

pub(crate) struct MechanicalDb {
    conn: Connection,
}

impl MechanicalDb {
    pub fn open(path: &Path) -> Result<Self, DevError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn insert_run(&self, run_id: &str, commit: &str, dirty: bool) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT INTO mechanical_runs (run_id, commit, dirty) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id, commit, dirty as i32],
        )?;
        Ok(())
    }

    pub fn insert_result(
        &self,
        run_id: &str,
        fixture_id: &str,
        pixel_diff_pct: Option<f64>,
        element_match_pct: Option<f64>,
        status: &str,
        artifact_dir: Option<&str>,
    ) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT INTO mechanical_results \
             (run_id, fixture_id, pixel_diff_pct, element_match_pct, status, artifact_dir) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![run_id, fixture_id, pixel_diff_pct, element_match_pct, status, artifact_dir],
        )?;
        Ok(())
    }

    pub fn get_approval(&self, fixture_id: &str) -> Result<Option<Approval>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT fixture_id, approved_at, commit, pixel_diff_pct, element_match_pct \
             FROM mechanical_approvals WHERE fixture_id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![fixture_id], map_approval)?;
        match rows.next() {
            Some(Ok(approval)) => Ok(Some(approval)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    pub fn set_approval(
        &self,
        fixture_id: &str,
        commit: &str,
        pixel_diff_pct: f64,
        element_match_pct: Option<f64>,
    ) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO mechanical_approvals \
             (fixture_id, commit, pixel_diff_pct, element_match_pct) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![fixture_id, commit, pixel_diff_pct, element_match_pct],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn latest_run(&self) -> Result<Option<RunSummary>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, timestamp, commit, dirty \
             FROM mechanical_runs ORDER BY timestamp DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], map_run_summary)?;
        match rows.next() {
            Some(Ok(summary)) => Ok(Some(summary)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    pub fn run_results(&self, run_id: &str) -> Result<Vec<ResultRow>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, fixture_id, pixel_diff_pct, element_match_pct, status, artifact_dir \
             FROM mechanical_results WHERE run_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![run_id], map_result_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn all_approvals(&self) -> Result<Vec<Approval>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT fixture_id, approved_at, commit, pixel_diff_pct, element_match_pct \
             FROM mechanical_approvals ORDER BY fixture_id",
        )?;
        let rows = stmt.query_map([], map_approval)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn latest_result_for_fixture(&self, fixture_id: &str) -> Result<Option<ResultRow>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.run_id, r.fixture_id, r.pixel_diff_pct, r.element_match_pct, r.status, r.artifact_dir \
             FROM mechanical_results r \
             JOIN mechanical_runs mr ON mr.run_id = r.run_id \
             WHERE r.fixture_id = ?1 \
             ORDER BY mr.timestamp DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![fixture_id], map_result_row)?;
        match rows.next() {
            Some(Ok(row)) => Ok(Some(row)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }
}

fn map_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    Ok(Approval {
        fixture_id: row.get(0)?,
        approved_at: row.get(1)?,
        commit: row.get(2)?,
        pixel_diff_pct: row.get(3)?,
        element_match_pct: row.get(4)?,
    })
}

#[allow(dead_code)]
fn map_run_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        run_id: row.get(0)?,
        timestamp: row.get(1)?,
        commit: row.get(2)?,
        dirty: row.get::<_, i32>(3)? != 0,
    })
}

fn map_result_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResultRow> {
    Ok(ResultRow {
        run_id: row.get(0)?,
        fixture_id: row.get(1)?,
        pixel_diff_pct: row.get(2)?,
        element_match_pct: row.get(3)?,
        status: row.get(4)?,
        artifact_dir: row.get(5)?,
    })
}
