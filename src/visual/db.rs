//! Shared SQLite operations for the two visual result stores.

use std::path::Path;

use rusqlite::{Connection, Row};

use crate::error::DevError;

use super::status::Status;

#[derive(Clone, Copy)]
pub(crate) enum DbSubject {
    Litehtml,
    Sluggrs,
}

struct Layout {
    runs: &'static str,
    results: &'static str,
    approvals: &'static str,
    id: &'static str,
    element: bool,
    adapter: bool,
}

impl Layout {
    fn for_subject(subject: DbSubject) -> Self {
        match subject {
            DbSubject::Litehtml => Self {
                runs: "mechanical_runs",
                results: "mechanical_results",
                approvals: "mechanical_approvals",
                id: "fixture_id",
                element: true,
                adapter: false,
            },
            DbSubject::Sluggrs => Self {
                runs: "snapshot_runs",
                results: "snapshot_results",
                approvals: "snapshot_approvals",
                id: "snapshot_id",
                element: false,
                adapter: true,
            },
        }
    }

    fn element_column(&self) -> &'static str {
        if self.element {
            "element_match_pct"
        } else {
            "NULL"
        }
    }

    fn adapter_column(&self) -> &'static str {
        if self.adapter { "adapter_name" } else { "NULL" }
    }
}

#[allow(dead_code)]
pub(crate) struct Approval {
    pub(crate) subject_id: String,
    pub(crate) approved_at: String,
    pub(crate) commit: String,
    pub(crate) pixel_diff_pct: f64,
    pub(crate) element_match_pct: Option<f64>,
}

#[allow(dead_code)]
pub(crate) struct RunSummary {
    pub(crate) run_id: String,
    pub(crate) timestamp: String,
    pub(crate) commit: String,
    pub(crate) dirty: bool,
    pub(crate) adapter_name: Option<String>,
}

#[allow(dead_code)]
pub(crate) struct ResultRow {
    pub(crate) run_id: String,
    pub(crate) subject_id: String,
    pub(crate) pixel_diff_pct: Option<f64>,
    pub(crate) element_match_pct: Option<f64>,
    pub(crate) status: Status,
}

pub(crate) struct VisualDb {
    conn: Connection,
    layout: Layout,
}

impl VisualDb {
    pub(crate) fn open(path: &Path, subject: DbSubject) -> Result<Self, DevError> {
        let layout = Layout::for_subject(subject);
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let adapter_column = if layout.adapter {
            ", adapter_name TEXT"
        } else {
            ""
        };
        let result_element = if layout.element {
            ", element_match_pct REAL"
        } else {
            ""
        };
        let approval_element = if layout.element {
            ", element_match_pct REAL"
        } else {
            ""
        };
        conn.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                    run_id TEXT PRIMARY KEY, \
                    timestamp TEXT NOT NULL DEFAULT (datetime('now')), \
                    [commit] TEXT NOT NULL, \
                    dirty INTEGER NOT NULL DEFAULT 0{adapter_column}\
                )",
                layout.runs,
            ),
            [],
        )?;
        conn.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                    run_id TEXT NOT NULL REFERENCES {}(run_id), \
                    {} TEXT NOT NULL, \
                    pixel_diff_pct REAL{result_element}, \
                    status TEXT NOT NULL, \
                    PRIMARY KEY (run_id, {})\
                )",
                layout.results, layout.runs, layout.id, layout.id,
            ),
            [],
        )?;
        conn.execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {} (\
                    {} TEXT PRIMARY KEY, \
                    approved_at TEXT NOT NULL DEFAULT (datetime('now')), \
                    [commit] TEXT NOT NULL, \
                    pixel_diff_pct REAL NOT NULL{approval_element}\
                )",
                layout.approvals, layout.id,
            ),
            [],
        )?;
        Ok(Self { conn, layout })
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn insert_run(
        &self,
        run_id: &str,
        commit: &str,
        dirty: bool,
        adapter_name: Option<&str>,
    ) -> Result<(), DevError> {
        if self.layout.adapter {
            self.conn.execute(
                &format!(
                    "INSERT INTO {} (run_id, [commit], dirty, adapter_name) \
                     VALUES (?1, ?2, ?3, ?4)",
                    self.layout.runs,
                ),
                rusqlite::params![run_id, commit, dirty as i32, adapter_name],
            )?;
        } else {
            self.conn.execute(
                &format!(
                    "INSERT INTO {} (run_id, [commit], dirty) VALUES (?1, ?2, ?3)",
                    self.layout.runs,
                ),
                rusqlite::params![run_id, commit, dirty as i32],
            )?;
        }
        Ok(())
    }

    pub(crate) fn insert_result(
        &self,
        run_id: &str,
        subject_id: &str,
        pixel_diff_pct: Option<f64>,
        element_match_pct: Option<f64>,
        status: Status,
    ) -> Result<(), DevError> {
        if self.layout.element {
            self.conn.execute(
                &format!(
                    "INSERT INTO {} \
                     (run_id, {}, pixel_diff_pct, element_match_pct, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    self.layout.results, self.layout.id,
                ),
                rusqlite::params![
                    run_id,
                    subject_id,
                    pixel_diff_pct,
                    element_match_pct,
                    status.as_str()
                ],
            )?;
        } else {
            self.conn.execute(
                &format!(
                    "INSERT INTO {} (run_id, {}, pixel_diff_pct, status) \
                     VALUES (?1, ?2, ?3, ?4)",
                    self.layout.results, self.layout.id,
                ),
                rusqlite::params![run_id, subject_id, pixel_diff_pct, status.as_str()],
            )?;
        }
        Ok(())
    }

    pub(crate) fn get_approval(&self, subject_id: &str) -> Result<Option<Approval>, DevError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}, approved_at, [commit], pixel_diff_pct, {} \
             FROM {} WHERE {} = ?1",
            self.layout.id,
            self.layout.element_column(),
            self.layout.approvals,
            self.layout.id,
        ))?;
        let mut rows = stmt.query_map(rusqlite::params![subject_id], map_approval)?;
        match rows.next() {
            Some(Ok(approval)) => Ok(Some(approval)),
            Some(Err(error)) => Err(error.into()),
            None => Ok(None),
        }
    }

    pub(crate) fn set_approval(
        &self,
        subject_id: &str,
        commit: &str,
        pixel_diff_pct: f64,
        element_match_pct: Option<f64>,
    ) -> Result<(), DevError> {
        if self.layout.element {
            self.conn.execute(
                &format!(
                    "INSERT OR REPLACE INTO {} \
                     ({}, [commit], pixel_diff_pct, element_match_pct) \
                     VALUES (?1, ?2, ?3, ?4)",
                    self.layout.approvals, self.layout.id,
                ),
                rusqlite::params![subject_id, commit, pixel_diff_pct, element_match_pct],
            )?;
        } else {
            self.conn.execute(
                &format!(
                    "INSERT OR REPLACE INTO {} ({}, [commit], pixel_diff_pct) \
                     VALUES (?1, ?2, ?3)",
                    self.layout.approvals, self.layout.id,
                ),
                rusqlite::params![subject_id, commit, pixel_diff_pct],
            )?;
        }
        Ok(())
    }

    pub(crate) fn latest_run(&self) -> Result<Option<RunSummary>, DevError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT run_id, timestamp, [commit], dirty, {} \
             FROM {} ORDER BY timestamp DESC, rowid DESC LIMIT 1",
            self.layout.adapter_column(),
            self.layout.runs,
        ))?;
        let mut rows = stmt.query_map([], map_run_summary)?;
        match rows.next() {
            Some(Ok(summary)) => Ok(Some(summary)),
            Some(Err(error)) => Err(error.into()),
            None => Ok(None),
        }
    }

    pub(crate) fn run_summary(&self, run_id: &str) -> Result<Option<RunSummary>, DevError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT run_id, timestamp, [commit], dirty, {} FROM {} WHERE run_id = ?1",
            self.layout.adapter_column(),
            self.layout.runs,
        ))?;
        let mut rows = stmt.query_map(rusqlite::params![run_id], map_run_summary)?;
        match rows.next() {
            Some(Ok(summary)) => Ok(Some(summary)),
            Some(Err(error)) => Err(error.into()),
            None => Ok(None),
        }
    }

    pub(crate) fn run_results(&self, run_id: &str) -> Result<Vec<ResultRow>, DevError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT run_id, {}, pixel_diff_pct, {}, status \
             FROM {} WHERE run_id = ?1",
            self.layout.id,
            self.layout.element_column(),
            self.layout.results,
        ))?;
        let rows = stmt.query_map(rusqlite::params![run_id], map_result_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn all_approvals(&self) -> Result<Vec<Approval>, DevError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}, approved_at, [commit], pixel_diff_pct, {} \
             FROM {} ORDER BY {}",
            self.layout.id,
            self.layout.element_column(),
            self.layout.approvals,
            self.layout.id,
        ))?;
        let rows = stmt.query_map([], map_approval)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn update_run_adapter(
        &self,
        run_id: &str,
        adapter_name: &str,
    ) -> Result<(), DevError> {
        if self.layout.adapter {
            self.conn.execute(
                &format!(
                    "UPDATE {} SET adapter_name = ?1 WHERE run_id = ?2",
                    self.layout.runs,
                ),
                rusqlite::params![adapter_name, run_id],
            )?;
        }
        Ok(())
    }

    pub(crate) fn latest_result_for_subject(
        &self,
        subject_id: &str,
    ) -> Result<Option<ResultRow>, DevError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT r.run_id, r.{}, r.pixel_diff_pct, r.{}, r.status \
             FROM {} r JOIN {} runs ON runs.run_id = r.run_id \
             WHERE r.{} = ?1 \
             ORDER BY runs.timestamp DESC, runs.rowid DESC LIMIT 1",
            self.layout.id,
            self.layout.element_column(),
            self.layout.results,
            self.layout.runs,
            self.layout.id,
        ))?;
        let mut rows = stmt.query_map(rusqlite::params![subject_id], map_result_row)?;
        match rows.next() {
            Some(Ok(row)) => Ok(Some(row)),
            Some(Err(error)) => Err(error.into()),
            None => Ok(None),
        }
    }
}

fn map_approval(row: &Row<'_>) -> rusqlite::Result<Approval> {
    Ok(Approval {
        subject_id: row.get(0)?,
        approved_at: row.get(1)?,
        commit: row.get(2)?,
        pixel_diff_pct: row.get(3)?,
        element_match_pct: row.get(4)?,
    })
}

fn map_run_summary(row: &Row<'_>) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        run_id: row.get(0)?,
        timestamp: row.get(1)?,
        commit: row.get(2)?,
        dirty: row.get::<_, i32>(3)? != 0,
        adapter_name: row.get(4)?,
    })
}

fn map_result_row(row: &Row<'_>) -> rusqlite::Result<ResultRow> {
    Ok(ResultRow {
        run_id: row.get(0)?,
        subject_id: row.get(1)?,
        pixel_diff_pct: row.get(2)?,
        element_match_pct: row.get(3)?,
        status: row.get(4)?,
    })
}
