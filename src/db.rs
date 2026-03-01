use std::io::Read;
use std::path::Path;

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// Handle to the results database.
pub struct ResultsDb {
    conn: rusqlite::Connection,
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
        run_migrations(&conn)?;
        conn.execute_batch(SCHEMA)?;
        // For fresh databases run_migrations was a no-op, so ensure
        // user_version reflects the schema we just created.
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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
    match &kv.value {
        KvValue::Int(v) => conn.execute(
            "INSERT INTO run_kv (run_id, key, value_int) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id, kv.key, v],
        )?,
        KvValue::Real(v) => conn.execute(
            "INSERT INTO run_kv (run_id, key, value_real) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id, kv.key, v],
        )?,
        KvValue::Text(v) => conn.execute(
            "INSERT INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
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
fn generate_uuid() -> Result<String, DevError> {
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

/// Current schema version. Increment when adding new migrations.
const SCHEMA_VERSION: i64 = 3;

/// Run all pending migrations based on `PRAGMA user_version`.
fn run_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Fresh database — no tables yet, nothing to migrate.
    if !has_table(conn, "runs") {
        return Ok(());
    }

    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    if current < 1 {
        migrate_uuid(conn)?;
    }
    if current < 2 {
        migrate_cli_args_metadata(conn)?;
    }
    if current < 3 {
        migrate_v2_to_v3(conn)?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Check whether a table exists in the database.
fn has_table(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

/// Check whether a column exists on a table.
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| {
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(Result::ok)
                .collect();
            Ok(names.contains(&column.to_owned()))
        })
        .unwrap_or(false)
}

/// Migration v0 -> v1: add uuid column and backfill.
fn migrate_uuid(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_column(conn, "runs", "uuid") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN uuid TEXT")?;
    }

    // Backfill existing rows with generated UUIDs.
    let mut stmt = conn.prepare("SELECT id FROM runs WHERE uuid IS NULL")?;
    let ids: Vec<i64> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(Result::ok)
        .collect();

    let mut update = conn.prepare("UPDATE runs SET uuid = ?1 WHERE id = ?2")?;
    for id in ids {
        let uuid = generate_uuid()?;
        update.execute(rusqlite::params![uuid, id])?;
    }

    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_runs_uuid ON runs(uuid)")?;
    Ok(())
}

/// Migration v1 -> v2: add cli_args and metadata columns.
fn migrate_cli_args_metadata(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_column(conn, "runs", "cli_args") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN cli_args TEXT")?;
    }
    if !has_column(conn, "runs", "metadata") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN metadata TEXT")?;
    }
    Ok(())
}

/// Migration v2 -> v3: add peak_rss_mb, project columns, create child tables,
/// migrate extra/metadata JSON into child tables.
fn migrate_v2_to_v3(conn: &rusqlite::Connection) -> Result<(), DevError> {
    // Phase 1: DDL additions.
    if !has_column(conn, "runs", "peak_rss_mb") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN peak_rss_mb REAL")?;
    }
    if !has_column(conn, "runs", "project") {
        conn.execute_batch("ALTER TABLE runs ADD COLUMN project TEXT NOT NULL DEFAULT 'pbfhogg'")?;
    }

    // Child tables + indexes (all idempotent via IF NOT EXISTS).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS run_distribution (
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            samples INTEGER NOT NULL, min_ms INTEGER NOT NULL,
            p50_ms INTEGER NOT NULL, p95_ms INTEGER NOT NULL, max_ms INTEGER NOT NULL,
            PRIMARY KEY (run_id));

        CREATE TABLE IF NOT EXISTS run_kv (
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            key TEXT NOT NULL, value_int INTEGER, value_real REAL, value_text TEXT,
            PRIMARY KEY (run_id, key));
        CREATE INDEX IF NOT EXISTS idx_run_kv_key ON run_kv(key, run_id);

        CREATE TABLE IF NOT EXISTS hotpath_functions (
            id INTEGER PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            section TEXT NOT NULL, description TEXT, ordinal INTEGER NOT NULL,
            name TEXT NOT NULL, calls INTEGER, avg TEXT, total TEXT,
            percent_total TEXT, p50 TEXT, p95 TEXT, p99 TEXT);
        CREATE INDEX IF NOT EXISTS idx_hotpath_functions_run_id ON hotpath_functions(run_id);

        CREATE TABLE IF NOT EXISTS hotpath_threads (
            id INTEGER PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
            name TEXT NOT NULL, status TEXT, cpu_percent TEXT, cpu_percent_max TEXT,
            cpu_user TEXT, cpu_sys TEXT, cpu_total TEXT,
            alloc_bytes TEXT, dealloc_bytes TEXT, mem_diff TEXT);
        CREATE INDEX IF NOT EXISTS idx_hotpath_threads_run_id ON hotpath_threads(run_id);

        CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project);"
    )?;

    // Phase 2: Migrate existing extra/metadata JSON to child tables.
    migrate_json_to_children(conn)?;

    Ok(())
}

/// Parse existing extra/metadata JSON and insert into child tables.
fn migrate_json_to_children(conn: &rusqlite::Connection) -> Result<(), DevError> {
    let mut stmt = conn.prepare(
        "SELECT id, extra, metadata FROM runs WHERE extra IS NOT NULL OR metadata IS NOT NULL"
    )?;
    let rows: Vec<(i64, Option<String>, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .filter_map(Result::ok)
        .collect();

    for (run_id, extra_json, metadata_json) in &rows {
        // Migrate extra JSON.
        if let Some(json_str) = extra_json
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(obj) = val.as_object()
        {
            migrate_extra_object(conn, *run_id, obj)?;
        }
        // Migrate metadata JSON -> run_kv with meta. prefix.
        if let Some(json_str) = metadata_json
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(obj) = val.as_object()
        {
            for (key, value) in obj {
                let prefixed = format!("meta.{key}");
                insert_kv_from_json(conn, *run_id, &prefixed, value)?;
            }
        }
    }

    Ok(())
}

/// Migrate a single extra JSON object into the appropriate child tables.
fn migrate_extra_object(
    conn: &rusqlite::Connection,
    run_id: i64,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), DevError> {
    // Case 1: Distribution stats.
    let is_distribution = obj.contains_key("min_ms")
        && obj.contains_key("p50_ms")
        && obj.contains_key("p95_ms")
        && obj.contains_key("max_ms")
        && obj.contains_key("samples");

    if is_distribution {
        let get_i64 = |k: &str| obj.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        conn.execute(
            "INSERT OR IGNORE INTO run_distribution (run_id, samples, min_ms, p50_ms, p95_ms, max_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![run_id, get_i64("samples"), get_i64("min_ms"), get_i64("p50_ms"), get_i64("p95_ms"), get_i64("max_ms")],
        )?;
        // Remaining keys (beyond the 5 distribution keys) go to run_kv.
        let dist_keys = ["min_ms", "p50_ms", "p95_ms", "max_ms", "samples"];
        for (key, value) in obj {
            if !dist_keys.contains(&key.as_str()) {
                insert_kv_from_json(conn, run_id, key, value)?;
            }
        }
        return Ok(());
    }

    // Case 2: Hotpath data.
    let is_hotpath = obj.contains_key("functions_timing") || obj.contains_key("functions_alloc");

    if is_hotpath {
        migrate_hotpath_section(conn, run_id, obj, "functions_timing", "timing")?;
        migrate_hotpath_section(conn, run_id, obj, "functions_alloc", "alloc")?;

        if let Some(threads_val) = obj.get("threads")
            && let Some(threads_obj) = threads_val.as_object()
        {
            // Thread summary stats -> run_kv with threads. prefix.
            for key in &["rss_bytes", "total_alloc_bytes", "total_dealloc_bytes", "alloc_dealloc_diff"] {
                if let Some(v) = threads_obj.get(*key) {
                    let prefixed = format!("threads.{key}");
                    insert_kv_from_json(conn, run_id, &prefixed, v)?;
                }
            }
            // Thread data rows.
            if let Some(data) = threads_obj.get("data").and_then(|v| v.as_array()) {
                for entry in data {
                    let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
                    conn.execute(
                        "INSERT INTO hotpath_threads \
                         (run_id, name, status, cpu_percent, cpu_percent_max, cpu_user, cpu_sys, cpu_total, \
                          alloc_bytes, dealloc_bytes, mem_diff) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        rusqlite::params![
                            run_id,
                            s("name").unwrap_or_default(),
                            s("status"), s("cpu_percent"), s("cpu_percent_max"),
                            s("cpu_user"), s("cpu_sys"), s("cpu_total"),
                            s("alloc_bytes"), s("dealloc_bytes"), s("mem_diff"),
                        ],
                    )?;
                }
            }
        }
        return Ok(());
    }

    // Case 3: Plain kv pairs.
    for (key, value) in obj {
        insert_kv_from_json(conn, run_id, key, value)?;
    }

    Ok(())
}

/// Migrate a hotpath functions section (timing or alloc) from JSON to the child table.
fn migrate_hotpath_section(
    conn: &rusqlite::Connection,
    run_id: i64,
    obj: &serde_json::Map<String, serde_json::Value>,
    json_key: &str,
    section_name: &str,
) -> Result<(), DevError> {
    let Some(section_val) = obj.get(json_key) else { return Ok(()) };
    let Some(section_obj) = section_val.as_object() else { return Ok(()) };
    let description = section_obj.get("description").and_then(|v| v.as_str());
    let Some(data) = section_obj.get("data").and_then(|v| v.as_array()) else { return Ok(()) };

    for (ordinal, entry) in data.iter().enumerate() {
        let s = |k: &str| entry.get(k).and_then(|v| v.as_str()).map(String::from);
        let calls = entry.get("calls").and_then(serde_json::Value::as_i64);
        #[allow(clippy::cast_possible_wrap)]
        let ord = ordinal as i64;
        conn.execute(
            "INSERT INTO hotpath_functions \
             (run_id, section, description, ordinal, name, calls, avg, total, percent_total, p50, p95, p99) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                run_id, section_name, description,
                ord,
                s("name").unwrap_or_default(),
                calls, s("avg"), s("total"), s("percent_total"),
                s("p50"), s("p95"), s("p99"),
            ],
        )?;
    }

    Ok(())
}

/// Insert a single JSON value into run_kv, auto-detecting type.
fn insert_kv_from_json(
    conn: &rusqlite::Connection,
    run_id: i64,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), DevError> {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                conn.execute(
                    "INSERT OR IGNORE INTO run_kv (run_id, key, value_int) VALUES (?1, ?2, ?3)",
                    rusqlite::params![run_id, key, v],
                )?;
            } else if let Some(v) = n.as_f64() {
                conn.execute(
                    "INSERT OR IGNORE INTO run_kv (run_id, key, value_real) VALUES (?1, ?2, ?3)",
                    rusqlite::params![run_id, key, v],
                )?;
            }
        }
        serde_json::Value::String(s) => {
            conn.execute(
                "INSERT OR IGNORE INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
                rusqlite::params![run_id, key, s],
            )?;
        }
        serde_json::Value::Bool(b) => {
            conn.execute(
                "INSERT OR IGNORE INTO run_kv (run_id, key, value_text) VALUES (?1, ?2, ?3)",
                rusqlite::params![run_id, key, b.to_string()],
            )?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format rows as a column-aligned table for stdout.
pub fn format_table(rows: &[StoredRow]) -> String {
    if rows.is_empty() {
        return String::from("(no results)");
    }

    let widths = compute_table_widths(rows);
    let mut out = String::new();

    // Header line.
    append_table_header(&mut out, &widths);
    out.push('\n');

    // Data lines.
    for row in rows {
        append_table_row(&mut out, row, &widths);
        out.push('\n');
    }

    // Remove trailing newline.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Format the detail fields that aren't shown in the summary table.
///
/// Shows hostname, subject, cargo features/profile, kernel, cpu governor,
/// available memory, storage notes, kv pairs, and distribution stats.
pub fn format_details(row: &StoredRow) -> String {
    let mut out = String::new();
    let mut fields: Vec<(String, String)> = Vec::new();

    if !row.hostname.is_empty() {
        fields.push(("hostname".into(), row.hostname.clone()));
    }
    if !row.subject.is_empty() {
        fields.push(("subject".into(), row.subject.clone()));
    }
    if !row.cargo_features.is_empty() {
        fields.push(("cargo features".into(), row.cargo_features.clone()));
    }
    if !row.cargo_profile.is_empty() {
        fields.push(("cargo profile".into(), row.cargo_profile.clone()));
    }
    if !row.kernel.is_empty() {
        fields.push(("kernel".into(), row.kernel.clone()));
    }
    if !row.cpu_governor.is_empty() {
        fields.push(("cpu governor".into(), row.cpu_governor.clone()));
    }
    if let Some(mb) = row.avail_memory_mb {
        fields.push(("avail memory".into(), format!("{mb} MB")));
    }
    if let Some(mb) = row.peak_rss_mb {
        fields.push(("peak rss".into(), format!("{mb:.1} MB")));
    }
    if !row.storage_notes.is_empty() {
        fields.push(("storage".into(), row.storage_notes.clone()));
    }
    if !row.cli_args.is_empty() {
        fields.push(("cli args".into(), row.cli_args.clone()));
    }
    if !row.project.is_empty() && row.project != "pbfhogg" {
        fields.push(("project".into(), row.project.clone()));
    }

    // Distribution stats.
    if let Some(ref dist) = row.distribution {
        fields.push(("samples".into(), dist.samples.to_string()));
        fields.push(("min".into(), format!("{} ms", dist.min_ms)));
        fields.push(("p50".into(), format!("{} ms", dist.p50_ms)));
        fields.push(("p95".into(), format!("{} ms", dist.p95_ms)));
        fields.push(("max".into(), format!("{} ms", dist.max_ms)));
    }

    // Metadata kv pairs (meta. prefix).
    let mut meta_kv: Vec<&KvPair> = row.kv.iter().filter(|kv| kv.key.starts_with("meta.")).collect();
    meta_kv.sort_by_key(|kv| &kv.key);
    for kv in &meta_kv {
        let label = kv.key.strip_prefix("meta.").unwrap_or(&kv.key).replace('_', " ");
        fields.push((label, kv.value.to_string()));
    }

    // Runtime kv pairs (non-meta, non-threads).
    let mut runtime_kv: Vec<&KvPair> = row.kv.iter()
        .filter(|kv| !kv.key.starts_with("meta.") && !kv.key.starts_with("threads."))
        .collect();
    runtime_kv.sort_by_key(|kv| &kv.key);
    for kv in &runtime_kv {
        let label = kv.key.replace('_', " ");
        fields.push((label, kv.value.to_string()));
    }

    if fields.is_empty() {
        return out;
    }

    let label_width = fields.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
    for (label, value) in &fields {
        use std::fmt::Write;
        writeln!(out, "  {label:<label_width$}  {value}").expect("write to String is infallible");
    }

    // Remove trailing newline.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Format side-by-side comparison of two commits.
pub fn format_compare(
    commit_a: &str,
    rows_a: &[StoredRow],
    commit_b: &str,
    rows_b: &[StoredRow],
    top: usize,
) -> String {
    let pairs = build_comparison_pairs(rows_a, rows_b);
    if pairs.is_empty() {
        return String::from("(no results)");
    }

    let widths = compute_compare_widths(commit_a, commit_b, &pairs);
    let mut out = String::new();

    append_compare_header(&mut out, commit_a, commit_b, &widths);
    out.push('\n');

    for pair in &pairs {
        append_compare_row(&mut out, pair, &widths);
        out.push('\n');
    }

    // Append hotpath diff tables for pairs that have hotpath data on both sides.
    for pair in &pairs {
        if let (Some(ha), Some(hb)) = (&pair.a_hotpath, &pair.b_hotpath)
            && let Some(diff) = crate::hotpath_fmt::format_hotpath_diff(ha, hb, top)
        {
            let (cmd, var, _) = split_pair_key(&pair.key);
            let label = if var.is_empty() { cmd.to_owned() } else { format!("{cmd} {var}") };
            let heading = if pair.input_display.is_empty() {
                format!("\n{label} — {commit_a} vs {commit_b}")
            } else {
                format!("\n{label} - {} — {commit_a} vs {commit_b}", pair.input_display)
            };
            out.push_str(&heading);
            out.push('\n');
            out.push_str(&diff);
        }
    }

    if out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Table formatting internals
// ---------------------------------------------------------------------------

struct TableWidths {
    uuid: usize,
    timestamp: usize,
    commit: usize,
    command: usize,
    variant: usize,
    elapsed: usize,
    input: usize,
}

fn compute_table_widths(rows: &[StoredRow]) -> TableWidths {
    let mut w = TableWidths {
        uuid: 4,
        timestamp: 9,
        commit: 6,
        command: 7,
        variant: 7,
        elapsed: 7,
        input: 5,
    };
    for row in rows {
        let uuid_short = short_uuid(&row.uuid);
        if uuid_short.len() > w.uuid {
            w.uuid = uuid_short.len();
        }
        if row.timestamp.len() > w.timestamp {
            w.timestamp = row.timestamp.len();
        }
        if row.commit.len() > w.commit {
            w.commit = row.commit.len();
        }
        if row.command.len() > w.command {
            w.command = row.command.len();
        }
        if row.variant.len() > w.variant {
            w.variant = row.variant.len();
        }
        let elapsed_str = format_elapsed(row.elapsed_ms);
        if elapsed_str.len() > w.elapsed {
            w.elapsed = elapsed_str.len();
        }
        let input_str = format_input(&row.input_file, row.input_mb);
        if input_str.len() > w.input {
            w.input = input_str.len();
        }
    }
    w
}

fn append_table_header(out: &mut String, w: &TableWidths) {
    use std::fmt::Write;
    write!(
        out,
        "{:<uuid_w$}  {:<ts_w$}  {:<cm_w$}  {:<cmd_w$}  {:<var_w$}  {:>el_w$}  {:<in_w$}",
        "uuid",
        "timestamp",
        "commit",
        "command",
        "variant",
        "elapsed",
        "input",
        uuid_w = w.uuid,
        ts_w = w.timestamp,
        cm_w = w.commit,
        cmd_w = w.command,
        var_w = w.variant,
        el_w = w.elapsed,
        in_w = w.input,
    )
    .expect("write to String is infallible");
}

fn append_table_row(out: &mut String, row: &StoredRow, w: &TableWidths) {
    use std::fmt::Write;
    let uuid_short = short_uuid(&row.uuid);
    let elapsed_str = format_elapsed(row.elapsed_ms);
    let input_str = format_input(&row.input_file, row.input_mb);
    write!(
        out,
        "{:<uuid_w$}  {:<ts_w$}  {:<cm_w$}  {:<cmd_w$}  {:<var_w$}  {:>el_w$}  {:<in_w$}",
        uuid_short,
        row.timestamp,
        row.commit,
        row.command,
        row.variant,
        elapsed_str,
        input_str,
        uuid_w = w.uuid,
        ts_w = w.timestamp,
        cm_w = w.commit,
        cmd_w = w.command,
        var_w = w.variant,
        el_w = w.elapsed,
        in_w = w.input,
    )
    .expect("write to String is infallible");
}

fn format_elapsed(ms: i64) -> String {
    format!("{ms} ms")
}

fn format_input(input_file: &str, input_mb: Option<f64>) -> String {
    if input_file.is_empty() {
        return String::new();
    }
    let basename = Path::new(input_file)
        .file_stem()
        .map_or(input_file, |s| s.to_str().unwrap_or(input_file));
    match input_mb {
        Some(mb) => format!("{basename} ({mb:.0} MB)"),
        None => basename.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Compare formatting internals
// ---------------------------------------------------------------------------

struct CompareWidths {
    command: usize,
    variant: usize,
    input: usize,
    col_a: usize,
    col_b: usize,
    change: usize,
    has_output: bool,
    output_a: usize,
    output_b: usize,
    output_change: usize,
}

struct ComparisonPair {
    key: String,
    a_ms: Option<i64>,
    b_ms: Option<i64>,
    a_hotpath: Option<HotpathData>,
    b_hotpath: Option<HotpathData>,
    a_output_bytes: Option<i64>,
    b_output_bytes: Option<i64>,
    /// Pre-formatted input string for display.
    input_display: String,
}

/// Find output_bytes in a StoredRow's kv pairs.
fn find_output_bytes(kv: &[KvPair]) -> Option<i64> {
    kv.iter().find(|p| p.key == "output_bytes").and_then(|p| match &p.value {
        KvValue::Int(v) => Some(*v),
        _ => None,
    })
}

fn build_comparison_pairs(
    rows_a: &[StoredRow],
    rows_b: &[StoredRow],
) -> Vec<ComparisonPair> {
    use std::collections::HashMap;

    struct RowData {
        elapsed_ms: i64,
        hotpath: Option<HotpathData>,
        output_bytes: Option<i64>,
        input_display: String,
    }

    let mut keys: Vec<String> = Vec::new();
    let mut a_map: HashMap<String, RowData> = HashMap::new();
    let mut b_map: HashMap<String, RowData> = HashMap::new();

    for row in rows_a {
        let key = pair_key(&row.command, &row.variant, &row.input_file);
        if let std::collections::hash_map::Entry::Vacant(e) = a_map.entry(key.clone()) {
            keys.push(key);
            e.insert(RowData {
                elapsed_ms: row.elapsed_ms,
                hotpath: take_hotpath_for_compare(row),
                output_bytes: find_output_bytes(&row.kv),
                input_display: format_input(&row.input_file, row.input_mb),
            });
        }
    }
    for row in rows_b {
        let key = pair_key(&row.command, &row.variant, &row.input_file);
        if let std::collections::hash_map::Entry::Vacant(e) = b_map.entry(key.clone()) {
            if !a_map.contains_key(&key) {
                keys.push(key.clone());
            }
            e.insert(RowData {
                elapsed_ms: row.elapsed_ms,
                hotpath: take_hotpath_for_compare(row),
                output_bytes: find_output_bytes(&row.kv),
                input_display: format_input(&row.input_file, row.input_mb),
            });
        }
    }

    keys.into_iter()
        .map(|k| {
            let a = a_map.remove(&k);
            let b = b_map.remove(&k);
            let input_display = a.as_ref().or(b.as_ref())
                .map(|r| r.input_display.clone())
                .unwrap_or_default();
            let a_output_bytes = a.as_ref().and_then(|r| r.output_bytes);
            let b_output_bytes = b.as_ref().and_then(|r| r.output_bytes);
            ComparisonPair {
                key: k,
                a_ms: a.as_ref().map(|r| r.elapsed_ms),
                b_ms: b.as_ref().map(|r| r.elapsed_ms),
                a_hotpath: a.and_then(|r| r.hotpath),
                b_hotpath: b.and_then(|r| r.hotpath),
                a_output_bytes,
                b_output_bytes,
                input_display,
            }
        })
        .collect()
}

/// Extract hotpath data for comparison. Uses a shallow reconstruction since
/// StoredRow doesn't impl Clone. Returns None if no hotpath data.
fn take_hotpath_for_compare(row: &StoredRow) -> Option<HotpathData> {
    row.hotpath.as_ref()?;
    // We need to reconstruct since HotpathData doesn't derive Clone.
    // This is called only for compare pairs (a handful of rows).
    let hp = row.hotpath.as_ref()?;
    Some(HotpathData {
        functions: hp.functions.iter().map(|f| HotpathFunction {
            section: f.section.clone(),
            description: f.description.clone(),
            ordinal: f.ordinal,
            name: f.name.clone(),
            calls: f.calls,
            avg: f.avg.clone(),
            total: f.total.clone(),
            percent_total: f.percent_total.clone(),
            p50: f.p50.clone(),
            p95: f.p95.clone(),
            p99: f.p99.clone(),
        }).collect(),
        threads: hp.threads.iter().map(|t| HotpathThread {
            name: t.name.clone(),
            status: t.status.clone(),
            cpu_percent: t.cpu_percent.clone(),
            cpu_percent_max: t.cpu_percent_max.clone(),
            cpu_user: t.cpu_user.clone(),
            cpu_sys: t.cpu_sys.clone(),
            cpu_total: t.cpu_total.clone(),
            alloc_bytes: t.alloc_bytes.clone(),
            dealloc_bytes: t.dealloc_bytes.clone(),
            mem_diff: t.mem_diff.clone(),
        }).collect(),
        thread_summary: hp.thread_summary.iter().map(|kv| KvPair {
            key: kv.key.clone(),
            value: match &kv.value {
                KvValue::Int(v) => KvValue::Int(*v),
                KvValue::Real(v) => KvValue::Real(*v),
                KvValue::Text(v) => KvValue::Text(v.clone()),
            },
        }).collect(),
    })
}

fn pair_key(command: &str, variant: &str, input_file: &str) -> String {
    format!("{command}\t{variant}\t{input_file}")
}

fn split_pair_key(key: &str) -> (&str, &str, &str) {
    let mut parts = key.splitn(3, '\t');
    let cmd = parts.next().unwrap_or("");
    let var = parts.next().unwrap_or("");
    let input = parts.next().unwrap_or("");
    (cmd, var, input)
}

fn compute_compare_widths(
    commit_a: &str,
    commit_b: &str,
    pairs: &[ComparisonPair],
) -> CompareWidths {
    let has_output = pairs.iter().any(|p| p.a_output_bytes.is_some() || p.b_output_bytes.is_some());
    let mut w = CompareWidths {
        command: 7,
        variant: 7,
        input: 5,
        col_a: commit_a.len().max(2),
        col_b: commit_b.len().max(2),
        change: 6,
        has_output,
        output_a: if has_output { "output_a".len() } else { 0 },
        output_b: if has_output { "output_b".len() } else { 0 },
        output_change: if has_output { "out_chg".len() } else { 0 },
    };
    for pair in pairs {
        let (cmd, var, _) = split_pair_key(&pair.key);
        w.command = w.command.max(cmd.len());
        w.variant = w.variant.max(var.len());
        w.input = w.input.max(pair.input_display.len());
        w.col_a = w.col_a.max(format_ms_or_dash(pair.a_ms).len());
        w.col_b = w.col_b.max(format_ms_or_dash(pair.b_ms).len());
        w.change = w.change.max(format_change(pair.a_ms, pair.b_ms).len());
        if has_output {
            w.output_a = w.output_a.max(format_bytes_or_dash(pair.a_output_bytes).len());
            w.output_b = w.output_b.max(format_bytes_or_dash(pair.b_output_bytes).len());
            w.output_change = w.output_change.max(format_change_bytes(pair.a_output_bytes, pair.b_output_bytes).len());
        }
    }
    w
}

fn append_compare_header(
    out: &mut String,
    commit_a: &str,
    commit_b: &str,
    w: &CompareWidths,
) {
    use std::fmt::Write;
    write!(
        out,
        "{:<cmd_w$}  {:<var_w$}  {:<in_w$}  {:>a_w$}  {:>b_w$}  {:>ch_w$}",
        "command",
        "variant",
        "input",
        commit_a,
        commit_b,
        "change",
        cmd_w = w.command,
        var_w = w.variant,
        in_w = w.input,
        a_w = w.col_a,
        b_w = w.col_b,
        ch_w = w.change,
    )
    .expect("write to String is infallible");
    if w.has_output {
        write!(
            out,
            "  {:>oa_w$}  {:>ob_w$}  {:>oc_w$}",
            "output_a",
            "output_b",
            "out_chg",
            oa_w = w.output_a,
            ob_w = w.output_b,
            oc_w = w.output_change,
        )
        .expect("write to String is infallible");
    }
}

fn append_compare_row(
    out: &mut String,
    pair: &ComparisonPair,
    w: &CompareWidths,
) {
    use std::fmt::Write;
    let (cmd, var, _) = split_pair_key(&pair.key);
    let a_str = format_ms_or_dash(pair.a_ms);
    let b_str = format_ms_or_dash(pair.b_ms);
    let ch = format_change(pair.a_ms, pair.b_ms);
    write!(
        out,
        "{:<cmd_w$}  {:<var_w$}  {:<in_w$}  {:>a_w$}  {:>b_w$}  {:>ch_w$}",
        cmd,
        var,
        pair.input_display,
        a_str,
        b_str,
        ch,
        cmd_w = w.command,
        var_w = w.variant,
        in_w = w.input,
        a_w = w.col_a,
        b_w = w.col_b,
        ch_w = w.change,
    )
    .expect("write to String is infallible");
    if w.has_output {
        let oa = format_bytes_or_dash(pair.a_output_bytes);
        let ob = format_bytes_or_dash(pair.b_output_bytes);
        let oc = format_change_bytes(pair.a_output_bytes, pair.b_output_bytes);
        write!(
            out,
            "  {:>oa_w$}  {:>ob_w$}  {:>oc_w$}",
            oa,
            ob,
            oc,
            oa_w = w.output_a,
            ob_w = w.output_b,
            oc_w = w.output_change,
        )
        .expect("write to String is infallible");
    }
}

fn format_ms_or_dash(ms: Option<i64>) -> String {
    match ms {
        Some(v) => format!("{v} ms"),
        None => String::from("--"),
    }
}

fn format_change(a_ms: Option<i64>, b_ms: Option<i64>) -> String {
    match (a_ms, b_ms) {
        (Some(a), Some(b)) if a != 0 => {
            #[allow(clippy::cast_precision_loss)]
            let pct = ((b - a) as f64 / a as f64) * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => String::from("--"),
    }
}

fn format_bytes_or_dash(bytes: Option<i64>) -> String {
    match bytes {
        Some(b) => {
            #[allow(clippy::cast_precision_loss)]
            let mb = b as f64 / (1024.0 * 1024.0);
            format!("{mb:.1} MB")
        }
        None => String::from("--"),
    }
}

fn format_change_bytes(a: Option<i64>, b: Option<i64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if a != 0 => {
            #[allow(clippy::cast_precision_loss)]
            let pct = ((b - a) as f64 / a as f64) * 100.0;
            if pct >= 0.0 {
                format!("+{pct:.1}%")
            } else {
                format!("{pct:.1}%")
            }
        }
        _ => String::from("--"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helper: build a StoredRow with sensible defaults, overriding key fields
    // -----------------------------------------------------------------------

    fn row(command: &str, variant: &str, input_file: &str, elapsed_ms: i64) -> StoredRow {
        StoredRow {
            id: 0,
            timestamp: String::from("2026-03-01 00:00:00"),
            hostname: String::from("testhost"),
            commit: String::from("aabbccdd"),
            subject: String::from("test commit"),
            command: command.to_owned(),
            variant: variant.to_owned(),
            input_file: input_file.to_owned(),
            input_mb: None,
            elapsed_ms,
            cargo_features: String::new(),
            cargo_profile: String::from("release"),
            kernel: String::new(),
            cpu_governor: String::new(),
            avail_memory_mb: None,
            storage_notes: String::new(),
            peak_rss_mb: None,
            uuid: String::from("abcdef1234567890"),
            cli_args: String::new(),
            project: String::from("test"),
            kv: vec![],
            distribution: None,
            hotpath: None,
        }
    }

    // -----------------------------------------------------------------------
    // pair_key / split_pair_key roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn pair_key_roundtrip_normal() {
        let key = pair_key("read", "mmap", "denmark.osm.pbf");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "read");
        assert_eq!(var, "mmap");
        assert_eq!(input, "denmark.osm.pbf");
    }

    #[test]
    fn pair_key_roundtrip_empty_fields() {
        let key = pair_key("read", "", "");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "read");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    #[test]
    fn pair_key_roundtrip_all_empty() {
        let key = pair_key("", "", "");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    #[test]
    fn pair_key_preserves_tabs_in_values() {
        // If a field contained a tab, splitn(3, '\t') would mangle it.
        // pair_key("a\tb", "c", "d") produces "a\tb\tc\td"
        // splitn(3, '\t') splits into ["a", "b", "c\td"]
        let key = pair_key("a\tb", "c", "d");
        let (cmd, var, input) = split_pair_key(&key);
        assert_eq!(cmd, "a", "tab in command corrupts first field");
        assert_eq!(var, "b", "original variant is lost");
        assert_eq!(input, "c\td", "variant bleeds into input field");
    }

    #[test]
    fn split_pair_key_no_tabs() {
        let (cmd, var, input) = split_pair_key("notabs");
        assert_eq!(cmd, "notabs");
        assert_eq!(var, "");
        assert_eq!(input, "");
    }

    // -----------------------------------------------------------------------
    // build_comparison_pairs
    // -----------------------------------------------------------------------

    #[test]
    fn comparison_pairs_both_have_same_benchmark() {
        let a = vec![row("read", "mmap", "dk.pbf", 100)];
        let b = vec![row("read", "mmap", "dk.pbf", 90)];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, Some(100));
        assert_eq!(pairs[0].b_ms, Some(90));
    }

    #[test]
    fn comparison_pairs_a_only() {
        let a = vec![row("read", "mmap", "dk.pbf", 100)];
        let b: Vec<StoredRow> = vec![];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, Some(100));
        assert_eq!(pairs[0].b_ms, None);
    }

    #[test]
    fn comparison_pairs_b_only() {
        let a: Vec<StoredRow> = vec![];
        let b = vec![row("write", "", "out.pbf", 200)];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, None);
        assert_eq!(pairs[0].b_ms, Some(200));
    }

    #[test]
    fn comparison_pairs_deduplication_first_entry_wins() {
        // Two rows in A with the same key -- first one should win.
        let a = vec![
            row("read", "mmap", "dk.pbf", 100),
            row("read", "mmap", "dk.pbf", 999),
        ];
        let b = vec![
            row("read", "mmap", "dk.pbf", 50),
            row("read", "mmap", "dk.pbf", 888),
        ];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a_ms, Some(100), "first A entry should win, not 999");
        assert_eq!(pairs[0].b_ms, Some(50), "first B entry should win, not 888");
    }

    #[test]
    fn comparison_pairs_ordering_a_first_then_b_new() {
        // A has benchmarks X and Y (in that order).
        // B has benchmarks Y and Z (in that order).
        // Expected key order: X, Y (from A), then Z (new from B).
        let a = vec![
            row("x-cmd", "", "", 10),
            row("y-cmd", "", "", 20),
        ];
        let b = vec![
            row("y-cmd", "", "", 25),
            row("z-cmd", "", "", 30),
        ];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 3);
        let key_strings: Vec<String> = pairs
            .iter()
            .map(|p| split_pair_key(&p.key).0.to_owned())
            .collect();
        assert_eq!(key_strings, vec!["x-cmd", "y-cmd", "z-cmd"]);

        // x-cmd: A-only
        assert_eq!(pairs[0].a_ms, Some(10));
        assert_eq!(pairs[0].b_ms, None);
        // y-cmd: both
        assert_eq!(pairs[1].a_ms, Some(20));
        assert_eq!(pairs[1].b_ms, Some(25));
        // z-cmd: B-only
        assert_eq!(pairs[2].a_ms, None);
        assert_eq!(pairs[2].b_ms, Some(30));
    }

    #[test]
    fn comparison_pairs_variant_and_input_matter() {
        // Same command but different variant/input should be separate pairs.
        let a = vec![
            row("read", "mmap", "dk.pbf", 100),
            row("read", "stdio", "dk.pbf", 200),
            row("read", "mmap", "se.pbf", 300),
        ];
        let b = vec![
            row("read", "mmap", "dk.pbf", 90),
        ];
        let pairs = build_comparison_pairs(&a, &b);

        assert_eq!(pairs.len(), 3);
        // Only the first pair should have both sides.
        assert!(pairs[0].a_ms.is_some() && pairs[0].b_ms.is_some());
        assert!(pairs[1].a_ms.is_some() && pairs[1].b_ms.is_none());
        assert!(pairs[2].a_ms.is_some() && pairs[2].b_ms.is_none());
    }

    #[test]
    fn comparison_pairs_empty_both_sides() {
        let pairs = build_comparison_pairs(&[], &[]);
        assert!(pairs.is_empty());
    }

    // -----------------------------------------------------------------------
    // format_change
    // -----------------------------------------------------------------------

    #[test]
    fn format_change_improvement() {
        // 100 -> 80 = -20%
        let result = format_change(Some(100), Some(80));
        assert_eq!(result, "-20.0%");
    }

    #[test]
    fn format_change_regression() {
        // 100 -> 130 = +30%
        let result = format_change(Some(100), Some(130));
        assert_eq!(result, "+30.0%");
    }

    #[test]
    fn format_change_same_value() {
        let result = format_change(Some(500), Some(500));
        assert_eq!(result, "+0.0%");
    }

    #[test]
    fn format_change_zero_baseline() {
        // a=0 falls through the guard `a != 0`, returns "--"
        let result = format_change(Some(0), Some(100));
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_missing_a() {
        let result = format_change(None, Some(100));
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_missing_b() {
        let result = format_change(Some(100), None);
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_both_missing() {
        let result = format_change(None, None);
        assert_eq!(result, "--");
    }

    #[test]
    fn format_change_large_regression() {
        // 1 -> 1001 = +100000%
        let result = format_change(Some(1), Some(1001));
        assert_eq!(result, "+100000.0%");
    }

    #[test]
    fn format_change_near_zero_result() {
        // 1000 -> 999: -0.1%
        let result = format_change(Some(1000), Some(999));
        assert_eq!(result, "-0.1%");
    }

    #[test]
    fn format_change_both_zero() {
        // a=0 hits the guard, returns "--"
        let result = format_change(Some(0), Some(0));
        assert_eq!(result, "--");
    }

    // -----------------------------------------------------------------------
    // format_input
    // -----------------------------------------------------------------------

    #[test]
    fn format_input_empty_filename() {
        let result = format_input("", None);
        assert_eq!(result, "");
    }

    #[test]
    fn format_input_empty_filename_with_mb() {
        // Even if MB is provided, empty filename returns empty.
        let result = format_input("", Some(42.0));
        assert_eq!(result, "");
    }

    #[test]
    fn format_input_with_extension_no_mb() {
        let result = format_input("denmark.osm.pbf", None);
        // file_stem strips only the last extension: "denmark.osm"
        assert_eq!(result, "denmark.osm");
    }

    #[test]
    fn format_input_with_extension_and_mb() {
        let result = format_input("denmark.osm.pbf", Some(123.4));
        assert_eq!(result, "denmark.osm (123 MB)");
    }

    #[test]
    fn format_input_no_extension() {
        let result = format_input("rawfile", None);
        assert_eq!(result, "rawfile");
    }

    #[test]
    fn format_input_no_extension_with_mb() {
        let result = format_input("rawfile", Some(0.5));
        assert_eq!(result, "rawfile (0 MB)");
    }

    #[test]
    fn format_input_path_with_directory() {
        // file_stem should extract from the basename
        let result = format_input("data/inputs/denmark.pbf", None);
        assert_eq!(result, "denmark");
    }

    #[test]
    fn format_input_single_extension() {
        let result = format_input("test.csv", Some(10.0));
        assert_eq!(result, "test (10 MB)");
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
    // format_elapsed
    // -----------------------------------------------------------------------

    #[test]
    fn format_elapsed_positive() {
        assert_eq!(format_elapsed(1234), "1234 ms");
    }

    #[test]
    fn format_elapsed_zero() {
        assert_eq!(format_elapsed(0), "0 ms");
    }

    #[test]
    fn format_elapsed_negative() {
        // Shouldn't happen in practice, but verify it doesn't panic.
        assert_eq!(format_elapsed(-5), "-5 ms");
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
    // Old schema definitions for migration tests
    // -----------------------------------------------------------------------

    /// v0 schema: no uuid, cli_args, metadata, peak_rss_mb, or project columns.
    const V0_SCHEMA: &str = "\
        CREATE TABLE runs (
            id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, hostname TEXT NOT NULL,
            [commit] TEXT NOT NULL, subject TEXT NOT NULL, command TEXT NOT NULL,
            variant TEXT, input_file TEXT, input_mb REAL, elapsed_ms INTEGER NOT NULL,
            cargo_features TEXT, cargo_profile TEXT DEFAULT 'release',
            kernel TEXT, cpu_governor TEXT, avail_memory_mb INTEGER,
            storage_notes TEXT, extra TEXT
        )";

    /// v2 schema: adds uuid, cli_args, metadata over v0.
    const V2_SCHEMA: &str = "\
        CREATE TABLE runs (
            id INTEGER PRIMARY KEY, timestamp TEXT NOT NULL, hostname TEXT NOT NULL,
            [commit] TEXT NOT NULL, subject TEXT NOT NULL, command TEXT NOT NULL,
            variant TEXT, input_file TEXT, input_mb REAL, elapsed_ms INTEGER NOT NULL,
            cargo_features TEXT, cargo_profile TEXT DEFAULT 'release',
            kernel TEXT, cpu_governor TEXT, avail_memory_mb INTEGER,
            storage_notes TEXT, extra TEXT, uuid TEXT, cli_args TEXT, metadata TEXT
        )";

    const V0_INSERT: &str = "\
        INSERT INTO runs (timestamp, hostname, [commit], subject, command, variant,
            input_file, input_mb, elapsed_ms, extra)
        VALUES ('2026-01-01 00:00:00', 'testhost', 'aabb', 'old commit', 'bench read',
            'mmap', 'denmark.osm.pbf', 42.5, 1234, ?1)";

    const V2_INSERT: &str = "\
        INSERT INTO runs (timestamp, hostname, [commit], subject, command, variant,
            input_file, input_mb, elapsed_ms, extra, uuid, cli_args, metadata)
        VALUES ('2026-01-01 00:00:00', 'testhost', 'aabb', 'old commit', 'bench read',
            'mmap', 'denmark.osm.pbf', 42.5, 1234, ?1, 'existing_uuid', '--fast', ?2)";

    /// Create a temp directory and db path with a unique name per test.
    fn test_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("brokkr_test_{name}"));
        drop(std::fs::create_dir_all(&dir));
        let db_path = dir.join("test.db");
        drop(std::fs::remove_file(&db_path));
        (dir, db_path)
    }

    fn cleanup(dir: &std::path::Path, db_path: &std::path::Path) {
        drop(std::fs::remove_file(db_path));
        // WAL/SHM files.
        drop(std::fs::remove_file(db_path.with_extension("db-wal")));
        drop(std::fs::remove_file(db_path.with_extension("db-shm")));
        drop(std::fs::remove_dir(dir));
    }

    // -----------------------------------------------------------------------
    // Migration: v0 -> v3
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v0_to_v3() {
        let (dir, db_path) = test_db("migrate_v0");

        // Create v0 database with one row.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V0_SCHEMA).unwrap();
            conn.execute(V0_INSERT, rusqlite::params![rusqlite::types::Null]).unwrap();
        }

        // Open via ResultsDb — triggers all migrations.
        let db = ResultsDb::open(&db_path).expect("open should migrate v0 to v3");

        // Row is preserved and queryable.
        let rows = db.query(&QueryFilter {
            commit: Some(String::from("aabb")),
            command: None,
            variant: None,
            limit: 10,
        }).expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "bench read");
        assert_eq!(rows[0].elapsed_ms, 1234);

        // UUID was backfilled.
        assert!(!rows[0].uuid.is_empty(), "uuid should be backfilled");

        // project defaults to pbfhogg.
        assert_eq!(rows[0].project, "pbfhogg");

        // Schema version is 3.
        let version: i64 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 3);

        // Child tables exist (can query without error).
        db.conn.execute_batch("SELECT COUNT(*) FROM run_distribution").unwrap();
        db.conn.execute_batch("SELECT COUNT(*) FROM run_kv").unwrap();
        db.conn.execute_batch("SELECT COUNT(*) FROM hotpath_functions").unwrap();
        db.conn.execute_batch("SELECT COUNT(*) FROM hotpath_threads").unwrap();

        // New columns exist.
        assert!(has_column(&db.conn, "runs", "peak_rss_mb"));
        assert!(has_column(&db.conn, "runs", "project"));
        assert!(has_column(&db.conn, "runs", "cli_args"));
        assert!(has_column(&db.conn, "runs", "metadata"));

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with distribution JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_distribution_json() {
        let (dir, db_path) = test_db("migrate_v2_dist");

        let extra = r#"{"samples":10,"min_ms":100,"p50_ms":150,"p95_ms":200,"max_ms":250,"output_bytes":999}"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(V2_INSERT, rusqlite::params![extra, rusqlite::types::Null]).unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate v2 to v3");

        // project column added with default.
        let rows = db.query(&QueryFilter {
            commit: Some(String::from("aabb")),
            command: None, variant: None, limit: 10,
        }).expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project, "pbfhogg");

        // Distribution migrated to child table.
        let dist: (i64, i64, i64, i64, i64) = db.conn.query_row(
            "SELECT samples, min_ms, p50_ms, p95_ms, max_ms FROM run_distribution WHERE run_id = 1",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ).expect("distribution row should exist");
        assert_eq!(dist, (10, 100, 150, 200, 250));

        // Extra kv (output_bytes) migrated to run_kv.
        let val: i64 = db.conn.query_row(
            "SELECT value_int FROM run_kv WHERE run_id = 1 AND key = 'output_bytes'",
            [], |r| r.get(0),
        ).expect("output_bytes kv should exist");
        assert_eq!(val, 999);

        let version: i64 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 3);

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with hotpath JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_hotpath_json() {
        let (dir, db_path) = test_db("migrate_v2_hotpath");

        let extra = r#"{
            "functions_timing": {
                "description": "wall-clock timing",
                "data": [
                    {"name": "parse_header", "calls": 100, "avg": "1.2ms", "total": "120ms", "percent_total": "60%"},
                    {"name": "parse_body", "calls": 50, "avg": "2.0ms", "total": "100ms", "percent_total": "40%"}
                ]
            },
            "threads": {
                "rss_bytes": "1048576",
                "data": [
                    {"name": "main", "status": "running", "cpu_percent": "95%"}
                ]
            }
        }"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(V2_INSERT, rusqlite::params![extra, rusqlite::types::Null]).unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate hotpath");

        // Hotpath functions migrated.
        let count: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM hotpath_functions WHERE run_id = 1", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(count, 2, "should have 2 hotpath function rows");

        // Check first function.
        let (name, calls, section): (String, i64, String) = db.conn.query_row(
            "SELECT name, calls, section FROM hotpath_functions WHERE run_id = 1 AND ordinal = 0",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).unwrap();
        assert_eq!(name, "parse_header");
        assert_eq!(calls, 100);
        assert_eq!(section, "timing");

        // Thread data migrated.
        let thread_name: String = db.conn.query_row(
            "SELECT name FROM hotpath_threads WHERE run_id = 1", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(thread_name, "main");

        // Thread summary kv migrated.
        let rss: String = db.conn.query_row(
            "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'threads.rss_bytes'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(rss, "1048576");

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Migration: v2 -> v3 with metadata JSON
    // -----------------------------------------------------------------------

    #[test]
    fn migrate_v2_metadata_json() {
        let (dir, db_path) = test_db("migrate_v2_meta");

        let metadata = r#"{"compression":"zlib","io_mode":"buffered"}"#;

        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(V2_SCHEMA).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute(V2_INSERT, rusqlite::params![
                rusqlite::types::Null, metadata
            ]).unwrap();
        }

        let db = ResultsDb::open(&db_path).expect("open should migrate metadata");

        // Metadata migrated to run_kv with meta. prefix.
        let compression: String = db.conn.query_row(
            "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'meta.compression'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(compression, "zlib");

        let io_mode: String = db.conn.query_row(
            "SELECT value_text FROM run_kv WHERE run_id = 1 AND key = 'meta.io_mode'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(io_mode, "buffered");

        drop(db);
        cleanup(&dir, &db_path);
    }

    // -----------------------------------------------------------------------
    // Fresh database gets schema version 3
    // -----------------------------------------------------------------------

    #[test]
    fn fresh_db_has_correct_schema_version() {
        let (dir, db_path) = test_db("fresh_version");

        let db = ResultsDb::open(&db_path).expect("open fresh db");

        let version: i64 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        drop(db);
        cleanup(&dir, &db_path);
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
