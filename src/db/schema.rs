//! Database schema, DDL, and connection lifecycle.

use std::path::Path;

use crate::error::DevError;
use super::ResultsDb;
use super::migrate;

// ---------------------------------------------------------------------------
// Schema DDL
// ---------------------------------------------------------------------------

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
    cpu_percent_avg TEXT,
    alloc_bytes     TEXT,
    dealloc_bytes   TEXT,
    mem_diff        TEXT
);
CREATE INDEX IF NOT EXISTS idx_hotpath_threads_run_id ON hotpath_threads(run_id);
";

// ---------------------------------------------------------------------------
// SQL fragments shared across query modules
// ---------------------------------------------------------------------------

pub(super) const SELECT_COLS: &str = "\
id, timestamp, hostname, [commit], subject, command, variant, \
input_file, input_mb, elapsed_ms, peak_rss_mb, cargo_features, cargo_profile, \
kernel, cpu_governor, avail_memory_mb, storage_notes, uuid, \
cli_args, project";

pub(super) const INSERT_SQL: &str = "\
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

// ---------------------------------------------------------------------------
// Connection lifecycle
// ---------------------------------------------------------------------------

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
}
