//! Schema DDL and connection lifecycle for the corpus runs database.
//!
//! Mirrors `src/db/schema.rs`. FK `REFERENCES` clauses are declarative only -
//! enforcement is left off (as in `ResultsDb`), and the DB is append-only, so
//! there is no cascade-delete path.

use std::path::Path;

use super::CorpusDb;
use super::migrate;
use crate::error::DevError;

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS run (
    run_id            INTEGER PRIMARY KEY,
    started_at        TEXT NOT NULL,
    selector          TEXT NOT NULL,
    gated             INTEGER NOT NULL,
    result            TEXT NOT NULL,
    fail_reason       TEXT,
    harness_exit_code INTEGER,
    probe_count       INTEGER NOT NULL,
    harness_stderr    TEXT,
    wall_ms           REAL
);
CREATE INDEX IF NOT EXISTS idx_run_started ON run(started_at);

CREATE TABLE IF NOT EXISTS disposition (
    run_id        INTEGER NOT NULL REFERENCES run(run_id),
    probe         TEXT NOT NULL,
    outcome       TEXT NOT NULL,
    disposition   TEXT NOT NULL,
    expected      TEXT,
    gate_ok       INTEGER NOT NULL,
    matched       INTEGER NOT NULL,
    ours_only     INTEGER NOT NULL,
    tv_only       INTEGER NOT NULL,
    boundary_ours INTEGER NOT NULL DEFAULT 0,
    boundary_tv   INTEGER NOT NULL DEFAULT 0,
    count_tier    TEXT,
    acc_tier      TEXT,
    acc_profile   TEXT,
    acc_failing   TEXT NOT NULL DEFAULT '[]',
    p90_entry     REAL,
    p90_exit      REAL,
    p90_pnl       REAL,
    sig_domain    TEXT,
    sig_leg       TEXT,
    sig_dimension TEXT,
    sig_detail    TEXT,
    sig_breaches  INTEGER,
    error         TEXT,
    runtime_ms    REAL,
    PRIMARY KEY (run_id, probe)
);
CREATE INDEX IF NOT EXISTS idx_disposition_probe ON disposition(probe, run_id);

CREATE TABLE IF NOT EXISTS trade_diff (
    run_id            INTEGER NOT NULL REFERENCES run(run_id),
    probe             TEXT NOT NULL,
    our_index         INTEGER NOT NULL,
    tv_index          INTEGER NOT NULL,
    our_entry_ts      INTEGER NOT NULL,
    our_exit_ts       INTEGER NOT NULL,
    our_entry_price   REAL NOT NULL,
    our_exit_price    REAL NOT NULL,
    our_qty           REAL NOT NULL,
    our_pnl           REAL NOT NULL,
    entry_ts_delta    INTEGER,
    exit_ts_delta     INTEGER,
    entry_price_delta REAL,
    exit_price_delta  REAL,
    our_entry_bar     INTEGER,
    our_exit_bar      INTEGER,
    our_side          TEXT,
    our_entry_id      TEXT,
    our_exit_id       TEXT,
    tv_entry_ts       INTEGER,
    tv_exit_ts        INTEGER,
    tv_entry_price    REAL,
    tv_exit_price     REAL,
    tv_entry_qty      REAL,
    tv_pnl            REAL,
    tv_entry_signal   TEXT,
    tv_exit_signal    TEXT,
    PRIMARY KEY (run_id, probe, our_index, tv_index)
);

CREATE TABLE IF NOT EXISTS gate_miss (
    run_id   INTEGER NOT NULL REFERENCES run(run_id),
    probe    TEXT NOT NULL,
    expected TEXT,
    actual   TEXT,
    PRIMARY KEY (run_id, probe)
);

CREATE TABLE IF NOT EXISTS dense_na_site (
    id        INTEGER PRIMARY KEY,
    run_id    INTEGER NOT NULL REFERENCES run(run_id),
    probe     TEXT NOT NULL,
    name      TEXT NOT NULL,
    call_site TEXT NOT NULL,
    na_count  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_dense_na_run ON dense_na_site(run_id);
";

impl CorpusDb {
    /// Open (or create) the database read-write at `path`, creating the parent
    /// directory, enabling WAL, running migrations, and applying the schema.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate::run_migrations(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", migrate::SCHEMA_VERSION)?;
        Ok(Self { conn })
    }

    /// Open the database read-only for queries. The read-only flag is the
    /// load-bearing guard behind the `--where`/`--sql` raw-SQL paths: SQLite
    /// rejects any write regardless of what the interpolated SQL asks for.
    pub fn open_readonly(path: &Path) -> Result<Self, DevError> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        // Belt-and-suspenders; read-only open already blocks writes.
        conn.pragma_update(None, "query_only", "ON").ok();
        Ok(Self { conn })
    }

    /// Borrow the underlying connection (crate-internal, for sibling modules).
    pub(super) fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// In-memory database for tests - applies the schema, skips WAL/migrate
    /// (no file, nothing to migrate). Keeps test data out of `/tmp`.
    #[cfg(test)]
    pub(super) fn open_in_memory() -> Result<Self, DevError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }
}
