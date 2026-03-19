//! Database tables for the sluggrs visual snapshot testing system.

use std::path::Path;

use rusqlite::Connection;

use crate::error::DevError;

const SCHEMA_RUNS: &str = "\
CREATE TABLE IF NOT EXISTS snapshot_runs (
    run_id          TEXT PRIMARY KEY,
    timestamp       TEXT NOT NULL DEFAULT (datetime('now')),
    [commit]        TEXT NOT NULL,
    dirty           INTEGER NOT NULL DEFAULT 0,
    adapter_name    TEXT
)";

const SCHEMA_RESULTS: &str = "\
CREATE TABLE IF NOT EXISTS snapshot_results (
    run_id          TEXT NOT NULL REFERENCES snapshot_runs(run_id),
    snapshot_id     TEXT NOT NULL,
    pixel_diff_pct  REAL,
    status          TEXT NOT NULL,
    PRIMARY KEY (run_id, snapshot_id)
)";

const SCHEMA_APPROVALS: &str = "\
CREATE TABLE IF NOT EXISTS snapshot_approvals (
    snapshot_id         TEXT PRIMARY KEY,
    approved_at         TEXT NOT NULL DEFAULT (datetime('now')),
    [commit]            TEXT NOT NULL,
    pixel_diff_pct      REAL NOT NULL
)";

#[allow(dead_code)]
pub(crate) struct Approval {
    pub snapshot_id: String,
    pub approved_at: String,
    pub commit: String,
    pub pixel_diff_pct: f64,
}

#[allow(dead_code)]
pub(crate) struct RunSummary {
    pub run_id: String,
    pub timestamp: String,
    pub commit: String,
    pub dirty: bool,
    pub adapter_name: Option<String>,
}

#[allow(dead_code)]
pub(crate) struct ResultRow {
    pub run_id: String,
    pub snapshot_id: String,
    pub pixel_diff_pct: Option<f64>,
    pub status: String,
}

pub(crate) struct SnapshotDb {
    conn: Connection,
}

impl SnapshotDb {
    pub fn open(path: &Path) -> Result<Self, DevError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute(SCHEMA_RUNS, [])?;
        conn.execute(SCHEMA_RESULTS, [])?;
        conn.execute(SCHEMA_APPROVALS, [])?;
        Ok(Self { conn })
    }

    pub fn insert_run(
        &self,
        run_id: &str,
        commit: &str,
        dirty: bool,
        adapter_name: Option<&str>,
    ) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT INTO snapshot_runs (run_id, [commit], dirty, adapter_name) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![run_id, commit, dirty as i32, adapter_name],
        )?;
        Ok(())
    }

    pub fn insert_result(
        &self,
        run_id: &str,
        snapshot_id: &str,
        pixel_diff_pct: Option<f64>,
        status: &str,
    ) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT INTO snapshot_results \
             (run_id, snapshot_id, pixel_diff_pct, status) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![run_id, snapshot_id, pixel_diff_pct, status],
        )?;
        Ok(())
    }

    pub fn get_approval(&self, snapshot_id: &str) -> Result<Option<Approval>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT snapshot_id, approved_at, [commit], pixel_diff_pct \
             FROM snapshot_approvals WHERE snapshot_id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![snapshot_id], map_approval)?;
        match rows.next() {
            Some(Ok(approval)) => Ok(Some(approval)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    pub fn set_approval(
        &self,
        snapshot_id: &str,
        commit: &str,
        pixel_diff_pct: f64,
    ) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO snapshot_approvals \
             (snapshot_id, [commit], pixel_diff_pct) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![snapshot_id, commit, pixel_diff_pct],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn latest_run(&self) -> Result<Option<RunSummary>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, timestamp, [commit], dirty, adapter_name \
             FROM snapshot_runs ORDER BY timestamp DESC LIMIT 1",
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
            "SELECT run_id, snapshot_id, pixel_diff_pct, status \
             FROM snapshot_results WHERE run_id = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![run_id], map_result_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn run_summary(&self, run_id: &str) -> Result<Option<RunSummary>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, timestamp, [commit], dirty, adapter_name \
             FROM snapshot_runs WHERE run_id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![run_id], map_run_summary)?;
        match rows.next() {
            Some(Ok(summary)) => Ok(Some(summary)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    pub fn all_approvals(&self) -> Result<Vec<Approval>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT snapshot_id, approved_at, [commit], pixel_diff_pct \
             FROM snapshot_approvals ORDER BY snapshot_id",
        )?;
        let rows = stmt.query_map([], map_approval)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn update_run_adapter(&self, run_id: &str, adapter_name: &str) -> Result<(), DevError> {
        self.conn.execute(
            "UPDATE snapshot_runs SET adapter_name = ?1 WHERE run_id = ?2",
            rusqlite::params![adapter_name, run_id],
        )?;
        Ok(())
    }

    pub fn latest_result_for_snapshot(&self, snapshot_id: &str) -> Result<Option<ResultRow>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT r.run_id, r.snapshot_id, r.pixel_diff_pct, r.status \
             FROM snapshot_results r \
             JOIN snapshot_runs sr ON sr.run_id = r.run_id \
             WHERE r.snapshot_id = ?1 \
             ORDER BY sr.timestamp DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![snapshot_id], map_result_row)?;
        match rows.next() {
            Some(Ok(row)) => Ok(Some(row)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }
}

fn map_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    Ok(Approval {
        snapshot_id: row.get(0)?,
        approved_at: row.get(1)?,
        commit: row.get(2)?,
        pixel_diff_pct: row.get(3)?,
    })
}

#[allow(dead_code)]
fn map_run_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        run_id: row.get(0)?,
        timestamp: row.get(1)?,
        commit: row.get(2)?,
        dirty: row.get::<_, i32>(3)? != 0,
        adapter_name: row.get(4)?,
    })
}

fn map_result_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResultRow> {
    Ok(ResultRow {
        run_id: row.get(0)?,
        snapshot_id: row.get(1)?,
        pixel_diff_pct: row.get(2)?,
        status: row.get(3)?,
    })
}
