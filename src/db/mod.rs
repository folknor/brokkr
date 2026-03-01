mod format;
mod migrate;

pub use format::{format_compare, format_details, format_table};

use std::io::Read;
use std::path::Path;

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// Handle to the results database.
pub struct ResultsDb {
    pub(super) conn: rusqlite::Connection,
}

/// A typed key-value pair for benchmark metadata and subprocess metrics.
#[derive(Clone)]
pub struct KvPair {
    pub key: String,
    pub value: KvValue,
}

/// Typed value for a key-value pair.
#[derive(Clone)]
pub enum KvValue {
    Int(i64),
    Real(f64),
    Text(String),
}

impl KvPair {
    pub fn int(key: impl Into<String>, value: i64) -> Self {
        Self { key: key.into(), value: KvValue::Int(value) }
    }
    pub fn real(key: impl Into<String>, value: f64) -> Self {
        Self { key: key.into(), value: KvValue::Real(value) }
    }
    pub fn text(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self { key: key.into(), value: KvValue::Text(value.into()) }
    }
}

impl std::fmt::Display for KvValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Int(v) => write!(f, "{v}"),
            Self::Real(v) => write!(f, "{v}"),
            Self::Text(v) => write!(f, "{v}"),
        }
    }
}

/// Distribution statistics from `harness.run_distribution()`.
pub struct Distribution {
    pub samples: i64,
    pub min_ms: i64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub max_ms: i64,
}

/// A single function row from hotpath profiling.
pub struct HotpathFunction {
    pub section: String,
    pub description: Option<String>,
    pub ordinal: i64,
    pub name: String,
    pub calls: Option<i64>,
    pub avg: Option<String>,
    pub total: Option<String>,
    pub percent_total: Option<String>,
    pub p50: Option<String>,
    pub p95: Option<String>,
    pub p99: Option<String>,
}

/// A single thread row from hotpath profiling.
pub struct HotpathThread {
    pub name: String,
    pub status: Option<String>,
    pub cpu_percent: Option<String>,
    pub cpu_percent_max: Option<String>,
    pub cpu_user: Option<String>,
    pub cpu_sys: Option<String>,
    pub cpu_total: Option<String>,
    pub alloc_bytes: Option<String>,
    pub dealloc_bytes: Option<String>,
    pub mem_diff: Option<String>,
}

/// Structured hotpath profiling data (functions + threads).
pub struct HotpathData {
    pub functions: Vec<HotpathFunction>,
    pub threads: Vec<HotpathThread>,
    pub thread_summary: Vec<KvPair>,
}

/// Convert a hotpath JSON report (from the hotpath crate) into a `HotpathData` struct.
///
/// Used by `run_hotpath_capture()` and the v2→v3 migration.
pub fn hotpath_data_from_json(extra: &serde_json::Value) -> Option<HotpathData> {
    let obj = extra.as_object()?;

    let has_timing = obj.contains_key("functions_timing");
    let has_alloc = obj.contains_key("functions_alloc");
    let has_threads = obj.contains_key("threads");

    if !has_timing && !has_alloc && !has_threads {
        return None;
    }

    let mut functions = Vec::new();

    if let Some(timing) = obj.get("functions_timing") {
        parse_functions_section(timing, "timing", &mut functions);
    }
    if let Some(alloc) = obj.get("functions_alloc") {
        parse_functions_section(alloc, "alloc", &mut functions);
    }

    let mut threads = Vec::new();
    let mut thread_summary = Vec::new();

    if let Some(threads_val) = obj.get("threads")
        && let Some(threads_obj) = threads_val.as_object()
    {
        for key in &["rss_bytes", "total_alloc_bytes", "total_dealloc_bytes", "alloc_dealloc_diff"] {
            if let Some(v) = threads_obj.get(*key).and_then(|v| v.as_str()) {
                thread_summary.push(KvPair::text(format!("threads.{key}"), v));
            }
        }
        if let Some(data) = threads_obj.get("data").and_then(|v| v.as_array()) {
            for entry in data {
                let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
                threads.push(HotpathThread {
                    name: s("name").unwrap_or_default(),
                    status: s("status"),
                    cpu_percent: s("cpu_percent"),
                    cpu_percent_max: s("cpu_percent_max"),
                    cpu_user: s("cpu_user"),
                    cpu_sys: s("cpu_sys"),
                    cpu_total: s("cpu_total"),
                    alloc_bytes: s("alloc_bytes"),
                    dealloc_bytes: s("dealloc_bytes"),
                    mem_diff: s("mem_diff"),
                });
            }
        }
    }

    if functions.is_empty() && threads.is_empty() {
        return None;
    }

    Some(HotpathData { functions, threads, thread_summary })
}

fn parse_functions_section(value: &serde_json::Value, section: &str, out: &mut Vec<HotpathFunction>) {
    let Some(obj) = value.as_object() else { return };
    let description = obj.get("description").and_then(|v| v.as_str()).map(String::from);
    let Some(data) = obj.get("data").and_then(|v| v.as_array()) else { return };

    for (ordinal, entry) in data.iter().enumerate() {
        let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
        #[allow(clippy::cast_possible_wrap)]
        let ord = ordinal as i64;
        out.push(HotpathFunction {
            section: section.to_owned(),
            description: description.clone(),
            ordinal: ord,
            name: s("name").unwrap_or_default(),
            calls: entry.get("calls").and_then(serde_json::Value::as_i64),
            avg: s("avg"),
            total: s("total"),
            percent_total: s("percent_total"),
            p50: s("p50"),
            p95: s("p95"),
            p99: s("p99"),
        });
    }
}

/// A benchmark result row to insert.
pub struct RunRow {
    pub hostname: String,
    pub commit: String,
    pub subject: String,
    pub command: String,
    pub variant: Option<String>,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub elapsed_ms: i64,
    pub peak_rss_mb: Option<f64>,
    pub cargo_features: Option<String>,
    pub cargo_profile: String,
    pub kernel: Option<String>,
    pub cpu_governor: Option<String>,
    pub avail_memory_mb: Option<i64>,
    pub storage_notes: Option<String>,
    pub cli_args: Option<String>,
    pub project: String,
    pub kv: Vec<KvPair>,
    pub distribution: Option<Distribution>,
    pub hotpath: Option<HotpathData>,
}

/// A row read back from the database.
///
/// Nullable columns (`variant`, `input_file`, `cargo_features`, etc.) are
/// mapped to `String` via `unwrap_or_default()` — `NULL` becomes `""`.
/// This is intentional: all consumers use `.is_empty()` checks, so the
/// distinction between NULL and empty string is not needed.
#[allow(dead_code)]
pub struct StoredRow {
    pub id: i64,
    pub timestamp: String,
    pub hostname: String,
    pub commit: String,
    pub subject: String,
    pub command: String,
    pub variant: String,
    pub input_file: String,
    pub input_mb: Option<f64>,
    pub elapsed_ms: i64,
    pub peak_rss_mb: Option<f64>,
    pub cargo_features: String,
    pub cargo_profile: String,
    pub kernel: String,
    pub cpu_governor: String,
    pub avail_memory_mb: Option<i64>,
    pub storage_notes: String,
    pub uuid: String,
    pub cli_args: String,
    pub project: String,
    pub kv: Vec<KvPair>,
    pub distribution: Option<Distribution>,
    pub hotpath: Option<HotpathData>,
}

/// Two-commit comparison: (commit_a, rows_a, commit_b, rows_b).
pub type CompareResult = (String, Vec<StoredRow>, String, Vec<StoredRow>);

/// Filters for querying stored rows.
pub struct QueryFilter {
    pub commit: Option<String>,
    pub command: Option<String>,
    pub variant: Option<String>,
    pub limit: usize,
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS runs (
    id              INTEGER PRIMARY KEY,
    timestamp       TEXT NOT NULL,
    hostname        TEXT NOT NULL,
    [commit]        TEXT NOT NULL,
    subject         TEXT NOT NULL,
    command         TEXT NOT NULL,
    variant         TEXT,
    input_file      TEXT,
    input_mb        REAL,
    elapsed_ms      INTEGER NOT NULL,
    peak_rss_mb     REAL,
    cargo_features  TEXT,
    cargo_profile   TEXT DEFAULT 'release',
    kernel          TEXT,
    cpu_governor    TEXT,
    avail_memory_mb INTEGER,
    storage_notes   TEXT,
    extra           TEXT,
    uuid            TEXT,
    cli_args        TEXT,
    metadata        TEXT,
    project         TEXT NOT NULL DEFAULT 'pbfhogg'
);
CREATE INDEX IF NOT EXISTS idx_runs_commit ON runs([commit]);
CREATE INDEX IF NOT EXISTS idx_runs_command ON runs(command);
CREATE INDEX IF NOT EXISTS idx_runs_timestamp ON runs(timestamp);
CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project);

CREATE TABLE IF NOT EXISTS run_distribution (
    run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    samples     INTEGER NOT NULL,
    min_ms      INTEGER NOT NULL,
    p50_ms      INTEGER NOT NULL,
    p95_ms      INTEGER NOT NULL,
    max_ms      INTEGER NOT NULL,
    PRIMARY KEY (run_id)
);

CREATE TABLE IF NOT EXISTS run_kv (
    run_id      INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    value_int   INTEGER,
    value_real  REAL,
    value_text  TEXT,
    PRIMARY KEY (run_id, key)
);
CREATE INDEX IF NOT EXISTS idx_run_kv_key ON run_kv(key, run_id);

CREATE TABLE IF NOT EXISTS hotpath_functions (
    id              INTEGER PRIMARY KEY,
    run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    section         TEXT NOT NULL,
    description     TEXT,
    ordinal         INTEGER NOT NULL,
    name            TEXT NOT NULL,
    calls           INTEGER,
    avg             TEXT,
    total           TEXT,
    percent_total   TEXT,
    p50             TEXT,
    p95             TEXT,
    p99             TEXT
);
CREATE INDEX IF NOT EXISTS idx_hotpath_functions_run_id ON hotpath_functions(run_id);

CREATE TABLE IF NOT EXISTS hotpath_threads (
    id              INTEGER PRIMARY KEY,
    run_id          INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    status          TEXT,
    cpu_percent     TEXT,
    cpu_percent_max TEXT,
    cpu_user        TEXT,
    cpu_sys         TEXT,
    cpu_total       TEXT,
    alloc_bytes     TEXT,
    dealloc_bytes   TEXT,
    mem_diff        TEXT
);
CREATE INDEX IF NOT EXISTS idx_hotpath_threads_run_id ON hotpath_threads(run_id);
";

const SELECT_COLS: &str = "\
id, timestamp, hostname, [commit], subject, command, variant, \
input_file, input_mb, elapsed_ms, peak_rss_mb, cargo_features, cargo_profile, \
kernel, cpu_governor, avail_memory_mb, storage_notes, uuid, \
cli_args, project";

const INSERT_SQL: &str = "\
INSERT INTO runs (\
    timestamp, hostname, [commit], subject, command, variant, \
    input_file, input_mb, elapsed_ms, peak_rss_mb, cargo_features, cargo_profile, \
    kernel, cpu_governor, avail_memory_mb, storage_notes, uuid, \
    cli_args, project\
) VALUES (\
    datetime('now'), ?1, ?2, ?3, ?4, ?5, \
    ?6, ?7, ?8, ?9, ?10, ?11, \
    ?12, ?13, ?14, ?15, ?16, \
    ?17, ?18\
)";

impl ResultsDb {
    /// Open (or create) the database at `path`. Creates schema and enables WAL
    /// mode. Runs any pending migrations based on `PRAGMA user_version`.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Migrate existing databases *before* applying SCHEMA so that
        // indexes on columns added by migrations (e.g. `project`) exist
        // by the time CREATE INDEX runs.
        migrate::run_migrations(&conn)?;
        conn.execute_batch(SCHEMA)?;
        // For fresh databases run_migrations was a no-op, so ensure
        // user_version reflects the schema we just created.
        conn.pragma_update(None, "user_version", migrate::SCHEMA_VERSION)?;
        Ok(Self { conn })
    }

    /// Insert a benchmark result row. Timestamp generated by SQLite
    /// `datetime('now')`. Returns the short UUID prefix (8 hex chars).
    pub fn insert(&self, row: &RunRow) -> Result<String, DevError> {
        let uuid = generate_uuid()?;
        self.conn.execute("BEGIN", [])?;

        let result = self.insert_inner(row, &uuid);
        if result.is_err() {
            self.conn.execute("ROLLBACK", []).ok();
            return result;
        }

        self.conn.execute("COMMIT", [])?;
        Ok(short_uuid(&uuid))
    }

    fn insert_inner(&self, row: &RunRow, uuid: &str) -> Result<String, DevError> {
        // Envelope row.
        self.conn.execute(
            INSERT_SQL,
            rusqlite::params![
                row.hostname,
                row.commit,
                row.subject,
                row.command,
                row.variant,
                row.input_file,
                row.input_mb,
                row.elapsed_ms,
                row.peak_rss_mb,
                row.cargo_features,
                row.cargo_profile,
                row.kernel,
                row.cpu_governor,
                row.avail_memory_mb,
                row.storage_notes,
                uuid,
                row.cli_args,
                row.project,
            ],
        )?;
        let run_id = self.conn.last_insert_rowid();

        // Distribution child row.
        if let Some(ref dist) = row.distribution {
            self.conn.execute(
                "INSERT INTO run_distribution (run_id, samples, min_ms, p50_ms, p95_ms, max_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![run_id, dist.samples, dist.min_ms, dist.p50_ms, dist.p95_ms, dist.max_ms],
            )?;
        }

        // Key-value pairs.
        for kv in &row.kv {
            insert_kv_row(&self.conn, run_id, kv)?;
        }

        // Hotpath child rows.
        if let Some(ref hp) = row.hotpath {
            for func in &hp.functions {
                self.conn.execute(
                    "INSERT INTO hotpath_functions \
                     (run_id, section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    rusqlite::params![
                        run_id, func.section, func.description, func.ordinal, func.name,
                        func.calls, func.avg, func.total, func.percent_total,
                        func.p50, func.p95, func.p99,
                    ],
                )?;
            }
            for thread in &hp.threads {
                self.conn.execute(
                    "INSERT INTO hotpath_threads \
                     (run_id, name, status, cpu_percent, cpu_percent_max, cpu_user, cpu_sys, cpu_total, \
                      alloc_bytes, dealloc_bytes, mem_diff) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    rusqlite::params![
                        run_id, thread.name, thread.status, thread.cpu_percent,
                        thread.cpu_percent_max, thread.cpu_user, thread.cpu_sys, thread.cpu_total,
                        thread.alloc_bytes, thread.dealloc_bytes, thread.mem_diff,
                    ],
                )?;
            }
            // Thread summary stats into run_kv.
            for kv in &hp.thread_summary {
                insert_kv_row(&self.conn, run_id, kv)?;
            }
        }

        Ok(short_uuid(uuid))
    }
}

fn insert_kv_row(conn: &rusqlite::Connection, run_id: i64, kv: &KvPair) -> Result<(), DevError> {
    // OR IGNORE: row.kv (stderr) is inserted first, thread_summary second.
    // Duplicates (same run_id+key) are silently skipped, keeping the stderr value.
    match &kv.value {
        KvValue::Int(v) => conn.execute(
            "INSERT OR IGNORE INTO run_kv (run_id, key, value_int) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id, kv.key, v],
        )?,
        KvValue::Real(v) => conn.execute(
            "INSERT OR IGNORE INTO run_kv (run_id, key, value_real) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id, kv.key, v],
        )?,
        KvValue::Text(v) => conn.execute(
            "INSERT OR IGNORE INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id, kv.key, v],
        )?,
    };
    Ok(())
}

impl ResultsDb {
    /// Query rows by UUID prefix. Loads all child data (kv, distribution, hotpath).
    pub fn query_by_uuid(&self, prefix: &str) -> Result<Vec<StoredRow>, DevError> {
        let sql = format!("SELECT {SELECT_COLS} FROM runs WHERE uuid LIKE ?1||'%' ORDER BY id DESC");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![prefix], map_stored_row)?;
        let mut result = collect_rows(rows)?;
        for row in &mut result {
            load_children(&self.conn, row)?;
        }
        Ok(result)
    }

    /// Query rows with optional filters. Commit matches by prefix (LIKE).
    /// Results ordered by id descending, limited to `filter.limit`.
    pub fn query(&self, filter: &QueryFilter) -> Result<Vec<StoredRow>, DevError> {
        let (sql, params) = build_query_sql(filter);
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), map_stored_row)?;
        collect_rows(rows)
    }

    /// Query two commits for side-by-side comparison. Each commit is matched
    /// by prefix. Optional command/variant filters narrow the results.
    /// Loads kv + hotpath children for each row (needed for output_bytes and diffs).
    pub fn query_compare(
        &self,
        a: &str,
        b: &str,
        command: Option<&str>,
        variant: Option<&str>,
    ) -> Result<(Vec<StoredRow>, Vec<StoredRow>), DevError> {
        let mut clauses = vec!["[commit] LIKE ?1||'%'".to_owned()];
        let mut params: Vec<String> = Vec::new();
        // ?1 is the commit, filled per-call below.
        params.push(String::new());
        if let Some(cmd) = command {
            params.push(cmd.to_owned());
            clauses.push(format!("command LIKE '%'||?{}||'%'", params.len()));
        }
        if let Some(v) = variant {
            params.push(v.to_owned());
            clauses.push(format!("variant LIKE '%'||?{}||'%'", params.len()));
        }
        let sql = format!(
            "SELECT {SELECT_COLS} FROM runs WHERE {} ORDER BY command, variant, id DESC",
            clauses.join(" AND ")
        );
        let mut rows_a = query_commit_filtered(&self.conn, &sql, a, &params)?;
        let mut rows_b = query_commit_filtered(&self.conn, &sql, b, &params)?;
        for row in rows_a.iter_mut().chain(rows_b.iter_mut()) {
            load_children(&self.conn, row)?;
        }
        Ok((rows_a, rows_b))
    }

    /// Find the two most recent distinct commits and compare them.
    /// Optional command/variant filters narrow the search (variant uses prefix match).
    pub fn query_compare_last(
        &self,
        command: Option<&str>,
        variant: Option<&str>,
    ) -> Result<Option<CompareResult>, DevError> {
        // Find two most recent distinct commits matching the filters.
        let mut clauses = Vec::new();
        let mut params: Vec<String> = Vec::new();
        if let Some(cmd) = command {
            params.push(cmd.to_owned());
            clauses.push(format!("command LIKE '%'||?{}||'%'", params.len()));
        }
        if let Some(v) = variant {
            params.push(v.to_owned());
            clauses.push(format!("variant LIKE '%'||?{}||'%'", params.len()));
        }
        let mut sql = "SELECT DISTINCT [commit] FROM runs".to_owned();
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY id DESC LIMIT 2");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let commits: Vec<String> = stmt
            .query_map(param_refs.as_slice(), |row| row.get(0))?
            .filter_map(Result::ok)
            .collect();

        if commits.len() < 2 {
            return Ok(None);
        }

        let (rows_a, rows_b) = self.query_compare(&commits[1], &commits[0], command, variant)?;
        Ok(Some((commits[1].clone(), rows_a, commits[0].clone(), rows_b)))
    }
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

fn query_commit_filtered(
    conn: &rusqlite::Connection,
    sql: &str,
    commit: &str,
    params: &[String],
) -> Result<Vec<StoredRow>, DevError> {
    let mut bound = params.to_vec();
    bound[0] = commit.to_owned();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        bound.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), map_stored_row)?;
    collect_rows(rows)
}

fn collect_rows(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<StoredRow>>,
) -> Result<Vec<StoredRow>, DevError> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn map_stored_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRow> {
    Ok(StoredRow {
        id: row.get("id")?,
        timestamp: row.get("timestamp")?,
        hostname: row.get("hostname")?,
        commit: row.get("commit")?,
        subject: row.get("subject")?,
        command: row.get("command")?,
        variant: row.get::<_, Option<String>>("variant")?.unwrap_or_default(),
        input_file: row.get::<_, Option<String>>("input_file")?.unwrap_or_default(),
        input_mb: row.get("input_mb")?,
        elapsed_ms: row.get("elapsed_ms")?,
        peak_rss_mb: row.get("peak_rss_mb")?,
        cargo_features: row.get::<_, Option<String>>("cargo_features")?.unwrap_or_default(),
        cargo_profile: row.get::<_, Option<String>>("cargo_profile")?.unwrap_or_default(),
        kernel: row.get::<_, Option<String>>("kernel")?.unwrap_or_default(),
        cpu_governor: row.get::<_, Option<String>>("cpu_governor")?.unwrap_or_default(),
        avail_memory_mb: row.get("avail_memory_mb")?,
        storage_notes: row.get::<_, Option<String>>("storage_notes")?.unwrap_or_default(),
        uuid: row.get::<_, Option<String>>("uuid")?.unwrap_or_default(),
        cli_args: row.get::<_, Option<String>>("cli_args")?.unwrap_or_default(),
        project: row.get::<_, Option<String>>("project")?.unwrap_or_else(|| "pbfhogg".to_owned()),
        kv: Vec::new(),
        distribution: None,
        hotpath: None,
    })
}

/// Load all child data (distribution, kv, hotpath) for a stored row.
fn load_children(conn: &rusqlite::Connection, row: &mut StoredRow) -> Result<(), DevError> {
    row.distribution = load_distribution(conn, row.id)?;
    row.kv = load_kv(conn, row.id)?;
    row.hotpath = load_hotpath(conn, row.id)?;
    Ok(())
}

fn load_distribution(conn: &rusqlite::Connection, run_id: i64) -> Result<Option<Distribution>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT samples, min_ms, p50_ms, p95_ms, max_ms FROM run_distribution WHERE run_id = ?1"
    )?;
    let mut rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok(Distribution {
            samples: row.get(0)?,
            min_ms: row.get(1)?,
            p50_ms: row.get(2)?,
            p95_ms: row.get(3)?,
            max_ms: row.get(4)?,
        })
    })?;
    match rows.next() {
        Some(Ok(dist)) => Ok(Some(dist)),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

fn load_kv(conn: &rusqlite::Connection, run_id: i64) -> Result<Vec<KvPair>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT key, value_int, value_real, value_text FROM run_kv WHERE run_id = ?1 ORDER BY key"
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        let key: String = row.get(0)?;
        let vi: Option<i64> = row.get(1)?;
        let vr: Option<f64> = row.get(2)?;
        let vt: Option<String> = row.get(3)?;
        let value = if let Some(v) = vi {
            KvValue::Int(v)
        } else if let Some(v) = vr {
            KvValue::Real(v)
        } else {
            KvValue::Text(vt.unwrap_or_default())
        };
        Ok(KvPair { key, value })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

fn load_hotpath(conn: &rusqlite::Connection, run_id: i64) -> Result<Option<HotpathData>, DevError> {
    let functions = load_hotpath_functions(conn, run_id)?;
    let threads = load_hotpath_threads(conn, run_id)?;
    if functions.is_empty() && threads.is_empty() {
        return Ok(None);
    }
    // Thread summary stats are stored in run_kv with "threads." prefix.
    let mut thread_summary = Vec::new();
    let kv = load_kv(conn, run_id)?;
    for pair in kv {
        if pair.key.starts_with("threads.") {
            thread_summary.push(pair);
        }
    }
    Ok(Some(HotpathData { functions, threads, thread_summary }))
}

fn load_hotpath_functions(conn: &rusqlite::Connection, run_id: i64) -> Result<Vec<HotpathFunction>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99 \
         FROM hotpath_functions WHERE run_id = ?1 ORDER BY section, ordinal"
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok(HotpathFunction {
            section: row.get(0)?,
            description: row.get(1)?,
            ordinal: row.get(2)?,
            name: row.get(3)?,
            calls: row.get(4)?,
            avg: row.get(5)?,
            total: row.get(6)?,
            percent_total: row.get(7)?,
            p50: row.get(8)?,
            p95: row.get(9)?,
            p99: row.get(10)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

fn load_hotpath_threads(conn: &rusqlite::Connection, run_id: i64) -> Result<Vec<HotpathThread>, DevError> {
    let mut stmt = conn.prepare(
        "SELECT name, status, cpu_percent, cpu_percent_max, cpu_user, cpu_sys, cpu_total, \
         alloc_bytes, dealloc_bytes, mem_diff \
         FROM hotpath_threads WHERE run_id = ?1"
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| {
        Ok(HotpathThread {
            name: row.get(0)?,
            status: row.get(1)?,
            cpu_percent: row.get(2)?,
            cpu_percent_max: row.get(3)?,
            cpu_user: row.get(4)?,
            cpu_sys: row.get(5)?,
            cpu_total: row.get(6)?,
            alloc_bytes: row.get(7)?,
            dealloc_bytes: row.get(8)?,
            mem_diff: row.get(9)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

fn build_query_sql(filter: &QueryFilter) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut params: Vec<String> = Vec::new();

    if let Some(ref c) = filter.commit {
        params.push(c.clone());
        clauses.push(format!("[commit] LIKE ?{}||'%'", params.len()));
    }
    if let Some(ref cmd) = filter.command {
        params.push(cmd.clone());
        clauses.push(format!("command LIKE '%'||?{}||'%'", params.len()));
    }
    if let Some(ref v) = filter.variant {
        params.push(v.clone());
        clauses.push(format!("variant LIKE '%'||?{}||'%'", params.len()));
    }

    let mut sql = format!("SELECT {SELECT_COLS} FROM runs");
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }

    // Limit param is appended as the next positional parameter.
    params.push(filter.limit.to_string());
    sql.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", params.len()));

    (sql, params)
}

// ---------------------------------------------------------------------------
// UUID
// ---------------------------------------------------------------------------

/// Generate a UUIDv4 as 32 hex chars (no dashes).
pub(super) fn generate_uuid() -> Result<String, DevError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    // Set version 4.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // Set variant 1.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(hex)
}

/// Return the first 8 hex chars of a UUID.
pub fn short_uuid(uuid: &str) -> String {
    uuid[..8.min(uuid.len())].to_owned()
}


#[cfg(test)]
mod tests {
    use super::*;

    fn test_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("brokkr_test_{name}"));
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        drop(std::fs::remove_file(&db_path));
        (dir, db_path)
    }

    fn cleanup(dir: &std::path::Path, db_path: &std::path::Path) {
        drop(std::fs::remove_file(db_path));
        drop(std::fs::remove_file(db_path.with_extension("db-wal")));
        drop(std::fs::remove_file(db_path.with_extension("db-shm")));
        drop(std::fs::remove_dir(dir));
    }

    // -----------------------------------------------------------------------
    // short_uuid
    // -----------------------------------------------------------------------

    #[test]
    fn short_uuid_normal() {
        let result = short_uuid("abcdef1234567890abcdef1234567890");
        assert_eq!(result, "abcdef12");
    }

    #[test]
    fn short_uuid_exactly_8() {
        let result = short_uuid("12345678");
        assert_eq!(result, "12345678");
    }

    #[test]
    fn short_uuid_shorter_than_8() {
        let result = short_uuid("abc");
        assert_eq!(result, "abc");
    }

    #[test]
    fn short_uuid_empty() {
        let result = short_uuid("");
        assert_eq!(result, "");
    }

    // -----------------------------------------------------------------------
    // build_query_sql
    // -----------------------------------------------------------------------

    #[test]
    fn build_query_sql_no_filters() {
        let filter = QueryFilter {
            commit: None,
            command: None,
            variant: None,
            limit: 50,
        };
        let (sql, params) = build_query_sql(&filter);

        // No WHERE clause, just ORDER BY + LIMIT
        assert!(!sql.contains("WHERE"), "should have no WHERE clause");
        assert!(sql.contains("ORDER BY id DESC LIMIT ?1"));
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], "50");
    }

    #[test]
    fn build_query_sql_all_filters() {
        let filter = QueryFilter {
            commit: Some(String::from("abc123")),
            command: Some(String::from("read")),
            variant: Some(String::from("mmap")),
            limit: 10,
        };
        let (sql, params) = build_query_sql(&filter);

        assert!(sql.contains("WHERE"));
        assert!(sql.contains("[commit] LIKE ?1||'%'"), "commit should be ?1");
        assert!(sql.contains("command LIKE '%'||?2||'%'"), "command should be ?2 contains");
        assert!(sql.contains("variant LIKE '%'||?3||'%'"), "variant should be ?3 contains");
        assert!(sql.contains("LIMIT ?4"), "limit should be ?4");
        assert_eq!(params.len(), 4);
        assert_eq!(params[0], "abc123");
        assert_eq!(params[1], "read");
        assert_eq!(params[2], "mmap");
        assert_eq!(params[3], "10");
    }

    #[test]
    fn build_query_sql_commit_only() {
        let filter = QueryFilter {
            commit: Some(String::from("deadbeef")),
            command: None,
            variant: None,
            limit: 25,
        };
        let (sql, params) = build_query_sql(&filter);

        assert!(sql.contains("[commit] LIKE ?1||'%'"));
        assert!(sql.contains("LIMIT ?2"));
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], "deadbeef");
        assert_eq!(params[1], "25");
    }

    #[test]
    fn build_query_sql_command_and_variant_no_commit() {
        let filter = QueryFilter {
            commit: None,
            command: Some(String::from("write")),
            variant: Some(String::from("direct")),
            limit: 5,
        };
        let (sql, params) = build_query_sql(&filter);

        // Without commit, command becomes ?1, variant ?2, limit ?3
        assert!(sql.contains("command LIKE '%'||?1||'%'"));
        assert!(sql.contains("variant LIKE '%'||?2||'%'"));
        assert!(sql.contains("LIMIT ?3"));
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], "write");
        assert_eq!(params[1], "direct");
        assert_eq!(params[2], "5");
    }

    #[test]
    fn build_query_sql_selects_correct_columns() {
        let filter = QueryFilter {
            commit: None,
            command: None,
            variant: None,
            limit: 1,
        };
        let (sql, _) = build_query_sql(&filter);
        assert!(sql.starts_with(&format!("SELECT {SELECT_COLS} FROM runs")));
    }

    // -----------------------------------------------------------------------
    // Integration: ResultsDb open + insert + query (in-memory via tempfile)
    // -----------------------------------------------------------------------

    #[test]
    fn db_open_and_insert_roundtrip() {
        let dir = std::env::temp_dir().join("brokkr_test_db_roundtrip");
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        // Clean up from previous runs.
        drop(std::fs::remove_file(&db_path));

        let db = ResultsDb::open(&db_path).expect("open db");
        let run = RunRow {
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test subject"),
            command: String::from("read"),
            variant: Some(String::from("mmap")),
            input_file: Some(String::from("denmark.osm.pbf")),
            input_mb: Some(42.5),
            elapsed_ms: 1234,
            peak_rss_mb: None,
            cargo_features: None,
            cargo_profile: String::from("release"),
            kernel: None,
            cpu_governor: None,
            avail_memory_mb: None,
            storage_notes: None,
            cli_args: Some(String::from("--fast")),
            project: String::from("test"),
            kv: vec![],
            distribution: None,
            hotpath: None,
        };
        let short = db.insert(&run).expect("insert");
        assert_eq!(short.len(), 8, "short uuid should be 8 chars");

        let rows = db
            .query(&QueryFilter {
                commit: Some(String::from("aabbccdd")),
                command: None,
                variant: None,
                limit: 10,
            })
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "read");
        assert_eq!(rows[0].variant, "mmap");
        assert_eq!(rows[0].input_file, "denmark.osm.pbf");
        assert_eq!(rows[0].elapsed_ms, 1234);
        assert_eq!(rows[0].cli_args, "--fast");

        // Clean up.
        drop(std::fs::remove_file(&db_path));
        drop(std::fs::remove_dir(&dir));
    }

    #[test]
    fn db_migrations_are_idempotent() {
        let dir = std::env::temp_dir().join("brokkr_test_db_migrations");
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        drop(std::fs::remove_file(&db_path));

        // Open twice -- second open should not fail on migrations.
        {
            let _db = ResultsDb::open(&db_path).expect("first open");
        }
        {
            let _db = ResultsDb::open(&db_path).expect("second open");
        }

        drop(std::fs::remove_file(&db_path));
        drop(std::fs::remove_dir(&dir));
    }

    // -----------------------------------------------------------------------
    // Query: command and variant contains match
    // -----------------------------------------------------------------------

    #[test]
    fn query_command_contains_match() {
        let (dir, db_path) = test_db("query_cmd_contains");

        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |cmd: &str, variant: &str| RunRow {
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test"),
            command: String::from(cmd),
            variant: Some(String::from(variant)),
            input_file: None, input_mb: None,
            elapsed_ms: 100,
            peak_rss_mb: None, cargo_features: None,
            cargo_profile: String::from("release"),
            kernel: None, cpu_governor: None, avail_memory_mb: None,
            storage_notes: None, cli_args: None,
            project: String::from("test"),
            kv: vec![], distribution: None, hotpath: None,
        };

        db.insert(&make_row("bench merge", "buffered+zlib")).unwrap();
        db.insert(&make_row("bench merge", "buffered+none")).unwrap();
        db.insert(&make_row("bench read", "mmap")).unwrap();

        // "merge" matches "bench merge" rows only.
        let rows = db.query(&QueryFilter {
            commit: None,
            command: Some(String::from("merge")),
            variant: None,
            limit: 50,
        }).unwrap();
        assert_eq!(rows.len(), 2, "merge should match 2 bench merge rows");

        // "bench" matches all 3 rows.
        let rows = db.query(&QueryFilter {
            commit: None,
            command: Some(String::from("bench")),
            variant: None,
            limit: 50,
        }).unwrap();
        assert_eq!(rows.len(), 3, "bench should match all rows");

        // "read" matches only bench read.
        let rows = db.query(&QueryFilter {
            commit: None,
            command: Some(String::from("read")),
            variant: None,
            limit: 50,
        }).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "bench read");

        // Full exact value still works.
        let rows = db.query(&QueryFilter {
            commit: None,
            command: Some(String::from("bench merge")),
            variant: None,
            limit: 50,
        }).unwrap();
        assert_eq!(rows.len(), 2);

        drop(db);
        cleanup(&dir, &db_path);
    }

    #[test]
    fn query_variant_contains_match() {
        let (dir, db_path) = test_db("query_var_contains");

        let db = ResultsDb::open(&db_path).expect("open");

        let make_row = |variant: &str| RunRow {
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test"),
            command: String::from("bench merge"),
            variant: Some(String::from(variant)),
            input_file: None, input_mb: None,
            elapsed_ms: 100,
            peak_rss_mb: None, cargo_features: None,
            cargo_profile: String::from("release"),
            kernel: None, cpu_governor: None, avail_memory_mb: None,
            storage_notes: None, cli_args: None,
            project: String::from("test"),
            kv: vec![], distribution: None, hotpath: None,
        };

        db.insert(&make_row("buffered+zlib")).unwrap();
        db.insert(&make_row("buffered+none")).unwrap();
        db.insert(&make_row("direct+zlib")).unwrap();

        // "zlib" matches buffered+zlib and direct+zlib.
        let rows = db.query(&QueryFilter {
            commit: None, command: None,
            variant: Some(String::from("zlib")),
            limit: 50,
        }).unwrap();
        assert_eq!(rows.len(), 2, "zlib should match 2 rows");

        // "buffered" matches both buffered variants.
        let rows = db.query(&QueryFilter {
            commit: None, command: None,
            variant: Some(String::from("buffered")),
            limit: 50,
        }).unwrap();
        assert_eq!(rows.len(), 2, "buffered should match 2 rows");

        // "none" matches only buffered+none.
        let rows = db.query(&QueryFilter {
            commit: None, command: None,
            variant: Some(String::from("none")),
            limit: 50,
        }).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].variant, "buffered+none");

        drop(db);
        cleanup(&dir, &db_path);
    }
}
