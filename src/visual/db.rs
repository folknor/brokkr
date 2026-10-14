//! Shared SQLite operations for the two visual result stores.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, Row};

use crate::error::DevError;

use super::status::Status;

#[derive(Clone, Copy)]
pub(crate) enum DbSubject {
    Litehtml,
    Sluggrs,
}

struct Layout {
    store_name: &'static str,
    file_name: &'static str,
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
                store_name: "litehtml visual store",
                file_name: "litehtml-visual.db",
                runs: "mechanical_runs",
                results: "mechanical_results",
                approvals: "mechanical_approvals",
                id: "fixture_id",
                element: true,
                adapter: false,
            },
            DbSubject::Sluggrs => Self {
                store_name: "sluggrs visual store",
                file_name: "sluggrs-visual.db",
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

    /// The element column read through a table alias; the `NULL` stand-in
    /// for a store without one is a literal, so it takes no qualifier.
    fn qualified_element_column(&self, alias: &str) -> String {
        if self.element {
            format!("{alias}.element_match_pct")
        } else {
            "NULL".to_owned()
        }
    }

    fn adapter_column(&self) -> &'static str {
        if self.adapter { "adapter_name" } else { "NULL" }
    }
}

pub(crate) struct Approval {
    pub(crate) subject_id: String,
    pub(crate) pixel_diff_pct: f64,
    pub(crate) element_match_pct: Option<f64>,
}

pub(crate) struct RunSummary {
    pub(crate) run_id: String,
    pub(crate) commit: String,
    pub(crate) dirty: bool,
    pub(crate) adapter_name: Option<String>,
}

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
    /// Open the subject's versioned visual store. `legacy_results_path` is
    /// retained as the source for a one-time import of the former shared
    /// `results.db` tables.
    pub(crate) fn open(legacy_results_path: &Path, subject: DbSubject) -> Result<Self, DevError> {
        let layout = Layout::for_subject(subject);
        let path = store_path(legacy_results_path, &layout);
        let conn = open_writable(&path, &layout)?;
        import_legacy(&conn, legacy_results_path, &layout)?;
        Ok(Self { conn, layout })
    }

    /// Open a query-only connection, upgrading an older visual file first.
    /// When neither a visual file nor legacy tables exist, query an empty
    /// in-memory schema without creating a project file.
    pub(crate) fn open_read_only(
        legacy_results_path: &Path,
        subject: DbSubject,
    ) -> Result<Self, DevError> {
        let layout = Layout::for_subject(subject);
        let path = store_path(legacy_results_path, &layout);
        let conn = if path.exists() {
            crate::db::store::open_read_only_migrating(
                &path,
                layout.store_name,
                SCHEMA_VERSION,
                || {
                    let conn = open_writable(&path, &layout)?;
                    import_legacy(&conn, legacy_results_path, &layout)?;
                    Ok(())
                },
            )?
        } else if let Some(data) = read_legacy(legacy_results_path, &layout)? {
            let conn = open_writable(&path, &layout)?;
            import_legacy_data(&conn, &layout, &data)?;
            drop(conn);
            crate::db::store::open_read_only(&path)?
        } else {
            let conn = Connection::open_in_memory()?;
            initialize(&conn, &layout)?;
            conn.pragma_update(None, "query_only", true)?;
            conn
        };

        // A current-version destination may predate the import marker if an
        // earlier process stopped between schema setup and row transfer.
        if !legacy_imported(&conn)? && has_legacy_tables(legacy_results_path, &layout)? {
            drop(conn);
            let writable = open_writable(&path, &layout)?;
            import_legacy(&writable, legacy_results_path, &layout)?;
            drop(writable);
            return Ok(Self {
                conn: crate::db::store::open_read_only(&path)?,
                layout,
            });
        }
        Ok(Self { conn, layout })
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
            "SELECT {}, pixel_diff_pct, {} \
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

    pub(crate) fn run_summary(&self, run_id: &str) -> Result<Option<RunSummary>, DevError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT run_id, [commit], dirty, {} FROM {} WHERE run_id = ?1",
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
            "SELECT {}, pixel_diff_pct, {} \
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
            "SELECT r.run_id, r.{}, r.pixel_diff_pct, {}, r.status \
             FROM {} r JOIN {} runs ON runs.run_id = r.run_id \
             WHERE r.{} = ?1 \
             ORDER BY runs.timestamp DESC, runs.rowid DESC LIMIT 1",
            self.layout.id,
            self.layout.qualified_element_column("r"),
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

const SCHEMA_VERSION: i64 = 1;
const LEGACY_IMPORT: &str = "results-db-visual-v1";

struct LegacyRun {
    run_id: String,
    timestamp: String,
    commit: String,
    dirty: i64,
    adapter_name: Option<String>,
}

struct LegacyResult {
    run_id: String,
    subject_id: String,
    pixel_diff_pct: Option<f64>,
    element_match_pct: Option<f64>,
    status: String,
}

struct LegacyApproval {
    subject_id: String,
    approved_at: String,
    commit: String,
    pixel_diff_pct: f64,
    element_match_pct: Option<f64>,
}

#[derive(Default)]
struct LegacyData {
    has_tables: bool,
    runs: Vec<LegacyRun>,
    results: Vec<LegacyResult>,
    approvals: Vec<LegacyApproval>,
}

fn store_path(legacy_results_path: &Path, layout: &Layout) -> PathBuf {
    legacy_results_path.with_file_name(layout.file_name)
}

fn open_writable(path: &Path, layout: &Layout) -> Result<Connection, DevError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = crate::db::store::open_read_write(path, layout.store_name, SCHEMA_VERSION)?;
    crate::db::store::migrate(
        &conn,
        layout.store_name,
        SCHEMA_VERSION,
        |conn, _current| initialize(conn, layout),
    )?;
    // A current-version store may be missing an index or a table after manual
    // repair. The idempotent DDL restores those without changing its stamp.
    initialize(&conn, layout)?;
    Ok(conn)
}

fn initialize(conn: &Connection, layout: &Layout) -> Result<(), DevError> {
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
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {} (\
            run_id TEXT PRIMARY KEY, \
            timestamp TEXT NOT NULL DEFAULT (datetime('now')), \
            [commit] TEXT NOT NULL, \
            dirty INTEGER NOT NULL DEFAULT 0{adapter_column}\
        );\
        CREATE TABLE IF NOT EXISTS {} (\
            run_id TEXT NOT NULL REFERENCES {}(run_id), \
            {} TEXT NOT NULL, \
            pixel_diff_pct REAL{result_element}, \
            status TEXT NOT NULL, \
            PRIMARY KEY (run_id, {})\
        );\
        CREATE TABLE IF NOT EXISTS {} (\
            {} TEXT PRIMARY KEY, \
            approved_at TEXT NOT NULL DEFAULT (datetime('now')), \
            [commit] TEXT NOT NULL, \
            pixel_diff_pct REAL NOT NULL{approval_element}\
        );\
        CREATE TABLE IF NOT EXISTS brokkr_imports (migration TEXT PRIMARY KEY);",
        layout.runs, layout.results, layout.runs, layout.id, layout.id, layout.approvals, layout.id,
    ))?;

    if layout.adapter && !has_column(conn, layout.runs, "adapter_name")? {
        conn.execute_batch(&format!(
            "ALTER TABLE {} ADD COLUMN adapter_name TEXT",
            layout.runs
        ))?;
    }
    if layout.element && !has_column(conn, layout.results, "element_match_pct")? {
        conn.execute_batch(&format!(
            "ALTER TABLE {} ADD COLUMN element_match_pct REAL",
            layout.results
        ))?;
    }
    if layout.element && !has_column(conn, layout.approvals, "element_match_pct")? {
        conn.execute_batch(&format!(
            "ALTER TABLE {} ADD COLUMN element_match_pct REAL",
            layout.approvals
        ))?;
    }
    Ok(())
}

fn has_table(conn: &Connection, table: &str) -> Result<bool, DevError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, DevError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let columns = stmt
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|name| name == column))
}

fn legacy_imported(conn: &Connection) -> rusqlite::Result<bool> {
    let has_marker: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' \
         AND name='brokkr_imports')",
        [],
        |row| row.get(0),
    )?;
    if !has_marker {
        return Ok(false);
    }
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM brokkr_imports WHERE migration = ?1)",
        [LEGACY_IMPORT],
        |row| row.get(0),
    )
}

fn has_legacy_tables(path: &Path, layout: &Layout) -> Result<bool, DevError> {
    if !path.is_file() {
        return Ok(false);
    }
    let conn = crate::db::store::open_read_only(path)?;
    Ok(has_table(&conn, layout.runs)?
        || has_table(&conn, layout.results)?
        || has_table(&conn, layout.approvals)?)
}

fn read_legacy(path: &Path, layout: &Layout) -> Result<Option<LegacyData>, DevError> {
    if !path.is_file() {
        return Ok(None);
    }
    let conn = crate::db::store::open_read_only(path)?;
    let has_runs = has_table(&conn, layout.runs)?;
    let has_results = has_table(&conn, layout.results)?;
    let has_approvals = has_table(&conn, layout.approvals)?;
    if !has_runs && !has_results && !has_approvals {
        return Ok(None);
    }

    let mut data = LegacyData {
        has_tables: true,
        ..LegacyData::default()
    };
    if has_runs {
        let adapter = if layout.adapter && has_column(&conn, layout.runs, "adapter_name")? {
            "adapter_name"
        } else {
            "NULL"
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT run_id, timestamp, [commit], dirty, {adapter} FROM {} ORDER BY rowid",
            layout.runs
        ))?;
        data.runs = stmt
            .query_map([], |row| {
                Ok(LegacyRun {
                    run_id: row.get(0)?,
                    timestamp: row.get(1)?,
                    commit: row.get(2)?,
                    dirty: row.get(3)?,
                    adapter_name: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    if has_results {
        let element = if layout.element && has_column(&conn, layout.results, "element_match_pct")? {
            "element_match_pct"
        } else {
            "NULL"
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT run_id, {}, pixel_diff_pct, {element}, status FROM {} ORDER BY rowid",
            layout.id, layout.results
        ))?;
        data.results = stmt
            .query_map([], |row| {
                Ok(LegacyResult {
                    run_id: row.get(0)?,
                    subject_id: row.get(1)?,
                    pixel_diff_pct: row.get(2)?,
                    element_match_pct: row.get(3)?,
                    status: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    if has_approvals {
        let element = if layout.element && has_column(&conn, layout.approvals, "element_match_pct")?
        {
            "element_match_pct"
        } else {
            "NULL"
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT {}, approved_at, [commit], pixel_diff_pct, {element} \
             FROM {} ORDER BY rowid",
            layout.id, layout.approvals
        ))?;
        data.approvals = stmt
            .query_map([], |row| {
                Ok(LegacyApproval {
                    subject_id: row.get(0)?,
                    approved_at: row.get(1)?,
                    commit: row.get(2)?,
                    pixel_diff_pct: row.get(3)?,
                    element_match_pct: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(Some(data))
}

fn import_legacy(conn: &Connection, legacy_path: &Path, layout: &Layout) -> Result<(), DevError> {
    if legacy_imported(conn)? {
        return Ok(());
    }
    let Some(data) = read_legacy(legacy_path, layout)? else {
        return Ok(());
    };
    import_legacy_data(conn, layout, &data)
}

fn import_legacy_data(
    conn: &Connection,
    layout: &Layout,
    data: &LegacyData,
) -> Result<(), DevError> {
    if !data.has_tables || legacy_imported(conn)? {
        return Ok(());
    }
    let foreign_keys: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let import = (|| {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> rusqlite::Result<()> {
            if legacy_imported(conn)? {
                return Ok(());
            }
            insert_legacy_runs(conn, layout, &data.runs)?;
            insert_legacy_results(conn, layout, &data.results)?;
            insert_legacy_approvals(conn, layout, &data.approvals)?;
            conn.execute(
                "INSERT INTO brokkr_imports (migration) VALUES (?1)",
                [LEGACY_IMPORT],
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            conn.execute_batch("ROLLBACK").ok();
            return Err(DevError::from(error));
        }
        if let Err(error) = conn.execute_batch("COMMIT") {
            conn.execute_batch("ROLLBACK").ok();
            return Err(DevError::from(error));
        }
        Ok(())
    })();
    let restored = conn.pragma_update(None, "foreign_keys", foreign_keys);
    import?;
    restored?;
    Ok(())
}

fn insert_legacy_runs(
    conn: &Connection,
    layout: &Layout,
    rows: &[LegacyRun],
) -> rusqlite::Result<()> {
    for run in rows {
        if layout.adapter {
            conn.execute(
                &format!(
                    "INSERT INTO {} (run_id, timestamp, [commit], dirty, adapter_name) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    layout.runs
                ),
                rusqlite::params![
                    run.run_id,
                    run.timestamp,
                    run.commit,
                    run.dirty,
                    run.adapter_name
                ],
            )?;
        } else {
            conn.execute(
                &format!(
                    "INSERT INTO {} (run_id, timestamp, [commit], dirty) \
                     VALUES (?1, ?2, ?3, ?4)",
                    layout.runs
                ),
                rusqlite::params![run.run_id, run.timestamp, run.commit, run.dirty],
            )?;
        }
    }
    Ok(())
}

fn insert_legacy_results(
    conn: &Connection,
    layout: &Layout,
    rows: &[LegacyResult],
) -> rusqlite::Result<()> {
    for result in rows {
        if layout.element {
            conn.execute(
                &format!(
                    "INSERT INTO {} \
                     (run_id, {}, pixel_diff_pct, element_match_pct, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    layout.results, layout.id
                ),
                rusqlite::params![
                    result.run_id,
                    result.subject_id,
                    result.pixel_diff_pct,
                    result.element_match_pct,
                    result.status
                ],
            )?;
        } else {
            conn.execute(
                &format!(
                    "INSERT INTO {} (run_id, {}, pixel_diff_pct, status) \
                     VALUES (?1, ?2, ?3, ?4)",
                    layout.results, layout.id
                ),
                rusqlite::params![
                    result.run_id,
                    result.subject_id,
                    result.pixel_diff_pct,
                    result.status
                ],
            )?;
        }
    }
    Ok(())
}

fn insert_legacy_approvals(
    conn: &Connection,
    layout: &Layout,
    rows: &[LegacyApproval],
) -> rusqlite::Result<()> {
    for approval in rows {
        if layout.element {
            conn.execute(
                &format!(
                    "INSERT INTO {} \
                     ({}, approved_at, [commit], pixel_diff_pct, element_match_pct) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    layout.approvals, layout.id
                ),
                rusqlite::params![
                    approval.subject_id,
                    approval.approved_at,
                    approval.commit,
                    approval.pixel_diff_pct,
                    approval.element_match_pct
                ],
            )?;
        } else {
            conn.execute(
                &format!(
                    "INSERT INTO {} ({}, approved_at, [commit], pixel_diff_pct) \
                     VALUES (?1, ?2, ?3, ?4)",
                    layout.approvals, layout.id
                ),
                rusqlite::params![
                    approval.subject_id,
                    approval.approved_at,
                    approval.commit,
                    approval.pixel_diff_pct
                ],
            )?;
        }
    }
    Ok(())
}

fn map_approval(row: &Row<'_>) -> rusqlite::Result<Approval> {
    Ok(Approval {
        subject_id: row.get(0)?,
        pixel_diff_pct: row.get(1)?,
        element_match_pct: row.get(2)?,
    })
}

fn map_run_summary(row: &Row<'_>) -> rusqlite::Result<RunSummary> {
    Ok(RunSummary {
        run_id: row.get(0)?,
        commit: row.get(1)?,
        dirty: row.get::<_, i32>(2)? != 0,
        adapter_name: row.get(3)?,
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
