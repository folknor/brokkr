//! Sidecar profile database - separate from the results DB.
//!
//! Sidecar data (samples, markers, summaries) is stored in `.brokkr/sidecar.db`
//! which is gitignored. The results DB (`.brokkr/results.db`) is tracked in git
//! and stays small. The two are linked by result UUID.

use std::path::Path;

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SIDECAR_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS sidecar_samples (
    result_uuid TEXT NOT NULL,
    run_idx     INTEGER NOT NULL DEFAULT 0,
    sample_idx  INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,
    rss_kb      INTEGER,
    anon_kb     INTEGER,
    file_kb     INTEGER,
    shmem_kb    INTEGER,
    swap_kb     INTEGER,
    vsize_kb    INTEGER,
    vm_hwm_kb   INTEGER,
    utime       INTEGER,
    stime       INTEGER,
    num_threads INTEGER,
    minflt      INTEGER,
    majflt      INTEGER,
    rchar       INTEGER,
    wchar       INTEGER,
    read_bytes  INTEGER,
    write_bytes INTEGER,
    cancelled_write_bytes INTEGER,
    syscr       INTEGER,
    syscw       INTEGER,
    vol_cs      INTEGER,
    nonvol_cs   INTEGER,
    PRIMARY KEY (result_uuid, run_idx, sample_idx)
);

CREATE TABLE IF NOT EXISTS sidecar_markers (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL DEFAULT 0,
    marker_idx   INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,
    name         TEXT NOT NULL,
    PRIMARY KEY (result_uuid, run_idx, marker_idx)
);

CREATE TABLE IF NOT EXISTS sidecar_summary (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL DEFAULT 0,
    vm_hwm_kb    INTEGER,
    sample_count INTEGER,
    marker_count INTEGER,
    wall_time_ms INTEGER,
    PRIMARY KEY (result_uuid, run_idx)
);

CREATE TABLE IF NOT EXISTS sidecar_counters (
    result_uuid  TEXT NOT NULL,
    run_idx      INTEGER NOT NULL DEFAULT 0,
    timestamp_us INTEGER NOT NULL,
    name         TEXT NOT NULL,
    value        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_counters_uuid ON sidecar_counters(result_uuid, run_idx);

CREATE TABLE IF NOT EXISTS sidecar_meta (
    result_uuid    TEXT NOT NULL PRIMARY KEY,
    best_run_idx   INTEGER NOT NULL DEFAULT 0,
    total_runs     INTEGER NOT NULL DEFAULT 1,
    run_start_epoch INTEGER,
    pid            INTEGER,
    command        TEXT,
    binary_path    TEXT,
    binary_xxh128  TEXT,
    git_commit     TEXT,
    mode           TEXT,
    dataset        TEXT,
    exit_code      INTEGER
);

CREATE TABLE IF NOT EXISTS sidecar_latest (
    key   TEXT NOT NULL PRIMARY KEY,
    uuid  TEXT NOT NULL
);
";

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// Current sidecar schema version.
const SIDECAR_VERSION: i64 = 5;

/// Run pending migrations based on `PRAGMA user_version`.
fn run_sidecar_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current >= SIDECAR_VERSION {
        return Ok(());
    }

    // v0 -> v1: initial schema (handled by CREATE TABLE IF NOT EXISTS in DDL).
    // v1 -> v2: add sidecar_meta table (handled by CREATE TABLE IF NOT EXISTS in DDL).
    // v2 -> v3: add sidecar_counters table (handled by CREATE TABLE IF NOT EXISTS in DDL).
    //           Existing UUIDs without meta rows get defaults on query (best_run_idx=0).
    // v3 -> v4: add run provenance columns to sidecar_meta.
    if current < 4 {
        // ALTER TABLE is idempotent-safe: if the column already exists (e.g.
        // fresh DB created with the v4 DDL), the error is harmless.
        for col in &[
            "run_start_epoch INTEGER",
            "pid INTEGER",
            "command TEXT",
            "binary_path TEXT",
            "binary_xxh128 TEXT",
            "git_commit TEXT",
            "mode TEXT",
            "dataset TEXT",
            "exit_code INTEGER",
        ] {
            let sql = format!("ALTER TABLE sidecar_meta ADD COLUMN {col}");
            // Ignore "duplicate column name" errors from fresh databases.
            #[allow(clippy::let_underscore_must_use)]
            let _ = conn.execute_batch(&sql);
        }
    }
    // v4 -> v5: rename `variant` column to `mode` to match the results
    //           DB rename in v13→v14. Only applies if the old column
    //           still exists (fresh DBs created with the v5 DDL already
    //           have `mode`).
    if current < 5 {
        let has_variant = conn
            .prepare("PRAGMA table_info(sidecar_meta)")
            .and_then(|mut stmt| {
                let names: Vec<String> = stmt
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(Result::ok)
                    .collect();
                Ok(names.iter().any(|n| n == "variant"))
            })
            .unwrap_or(false);
        let has_mode = conn
            .prepare("PRAGMA table_info(sidecar_meta)")
            .and_then(|mut stmt| {
                let names: Vec<String> = stmt
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(Result::ok)
                    .collect();
                Ok(names.iter().any(|n| n == "mode"))
            })
            .unwrap_or(false);
        if has_variant && !has_mode {
            conn.execute_batch("ALTER TABLE sidecar_meta RENAME COLUMN variant TO mode")?;
        }
    }

    conn.pragma_update(None, "user_version", SIDECAR_VERSION)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// SidecarDb
// ---------------------------------------------------------------------------

/// Provenance metadata for a sidecar session: when it ran, what binary
/// produced it, and whether that binary still matches the current build.
pub struct RunInfo {
    pub run_start_epoch: Option<i64>,
    pub pid: Option<i64>,
    pub command: Option<String>,
    pub binary_path: Option<String>,
    pub binary_xxh128: Option<String>,
    pub git_commit: Option<String>,
    pub mode: Option<String>,
    pub dataset: Option<String>,
    pub exit_code: Option<i32>,
}

/// Handle to the sidecar profile database (`.brokkr/sidecar.db`).
pub struct SidecarDb {
    conn: rusqlite::Connection,
}

impl SidecarDb {
    /// Access the underlying connection (for tests and backup).
    #[cfg(test)]
    pub(crate) fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// Open (or create) the sidecar database at `path`. Creates schema,
    /// enables WAL mode, and runs pending migrations.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        run_sidecar_migrations(&conn)?;
        conn.execute_batch(SIDECAR_SCHEMA)?;
        conn.pragma_update(None, "user_version", SIDECAR_VERSION)?;
        Ok(Self { conn })
    }

    // -------------------------------------------------------------------
    // Write
    // -------------------------------------------------------------------

    /// Bulk-insert sidecar profile data for one benchmark run.
    #[allow(clippy::cast_possible_wrap)]
    pub fn store_run(
        &self,
        result_uuid: &str,
        run_idx: usize,
        data: &crate::sidecar::SidecarData,
    ) -> Result<(), DevError> {
        let tx = self.conn.unchecked_transaction()?;

        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO sidecar_samples (
                    result_uuid, run_idx, sample_idx, timestamp_us,
                    rss_kb, anon_kb, file_kb, shmem_kb, swap_kb, vsize_kb, vm_hwm_kb,
                    utime, stime, num_threads, minflt, majflt,
                    rchar, wchar, read_bytes, write_bytes, cancelled_write_bytes,
                    syscr, syscw, vol_cs, nonvol_cs
                ) VALUES (
                    ?1, ?2, ?3, ?4,
                    ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                    ?12, ?13, ?14, ?15, ?16,
                    ?17, ?18, ?19, ?20, ?21,
                    ?22, ?23, ?24, ?25
                )",
            )?;

            for s in &data.samples {
                stmt.execute(rusqlite::params![
                    result_uuid,
                    run_idx as i64,
                    s.sample_idx,
                    s.timestamp_us,
                    s.rss_kb,
                    s.anon_kb,
                    s.file_kb,
                    s.shmem_kb,
                    s.swap_kb,
                    s.vsize_kb,
                    s.vm_hwm_kb,
                    s.utime,
                    s.stime,
                    s.num_threads,
                    s.minflt,
                    s.majflt,
                    s.rchar,
                    s.wchar,
                    s.read_bytes,
                    s.write_bytes,
                    s.cancelled_write_bytes,
                    s.syscr,
                    s.syscw,
                    s.vol_cs,
                    s.nonvol_cs,
                ])?;
            }
        }

        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO sidecar_markers (
                    result_uuid, run_idx, marker_idx, timestamp_us, name
                ) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;

            for m in &data.markers {
                stmt.execute(rusqlite::params![
                    result_uuid,
                    run_idx as i64,
                    m.marker_idx,
                    m.timestamp_us,
                    m.name,
                ])?;
            }
        }

        // Insert counters.
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO sidecar_counters (
                    result_uuid, run_idx, timestamp_us, name, value
                ) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;

            for c in &data.counters {
                stmt.execute(rusqlite::params![
                    result_uuid,
                    run_idx as i64,
                    c.timestamp_us,
                    c.name,
                    c.value,
                ])?;
            }
        }

        tx.execute(
            "INSERT INTO sidecar_summary (
                result_uuid, run_idx, vm_hwm_kb, sample_count, marker_count, wall_time_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                result_uuid,
                run_idx as i64,
                data.summary.vm_hwm_kb,
                data.summary.sample_count,
                data.summary.marker_count,
                data.summary.wall_time_ms,
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Store metadata about a sidecar session (best run index, total runs,
    /// and optional run provenance).
    pub fn store_meta(
        &self,
        result_uuid: &str,
        best_run_idx: usize,
        total_runs: usize,
        run_info: Option<&RunInfo>,
    ) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sidecar_meta (
                result_uuid, best_run_idx, total_runs,
                run_start_epoch, pid, command, binary_path, binary_xxh128,
                git_commit, mode, dataset, exit_code
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                result_uuid,
                i64::try_from(best_run_idx).unwrap_or(0),
                i64::try_from(total_runs).unwrap_or(1),
                run_info.and_then(|r| r.run_start_epoch),
                run_info.and_then(|r| r.pid),
                run_info.as_ref().and_then(|r| r.command.as_deref()),
                run_info.as_ref().and_then(|r| r.binary_path.as_deref()),
                run_info.as_ref().and_then(|r| r.binary_xxh128.as_deref()),
                run_info.as_ref().and_then(|r| r.git_commit.as_deref()),
                run_info.as_ref().and_then(|r| r.mode.as_deref()),
                run_info.as_ref().and_then(|r| r.dataset.as_deref()),
                run_info.and_then(|r| r.exit_code),
            ],
        )?;
        Ok(())
    }

    /// Query metadata for a result UUID prefix.
    /// Returns (best_run_idx, total_runs), defaulting to (0, 1) if not found.
    pub fn query_meta(&self, uuid_prefix: &str) -> (usize, usize) {
        let uuid_prefix = self.resolve_latest(uuid_prefix);
        self.conn
            .query_row(
                "SELECT best_run_idx, total_runs FROM sidecar_meta
                 WHERE result_uuid LIKE ?1||'%'",
                rusqlite::params![uuid_prefix],
                |row| {
                    let best: i64 = row.get(0)?;
                    let total: i64 = row.get(1)?;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    Ok((best.max(0) as usize, total.max(0) as usize))
                },
            )
            .unwrap_or((0, 1))
    }

    /// Query run provenance for a result UUID prefix.
    pub fn query_run_info(&self, uuid_prefix: &str) -> Option<RunInfo> {
        let uuid_prefix = self.resolve_latest(uuid_prefix);
        self.conn
            .query_row(
                "SELECT run_start_epoch, pid, command, binary_path, binary_xxh128,
                        git_commit, mode, dataset, exit_code
                 FROM sidecar_meta WHERE result_uuid LIKE ?1||'%'",
                rusqlite::params![uuid_prefix],
                |row| {
                    Ok(RunInfo {
                        run_start_epoch: row.get(0)?,
                        pid: row.get(1)?,
                        command: row.get(2)?,
                        binary_path: row.get(3)?,
                        binary_xxh128: row.get(4)?,
                        git_commit: row.get(5)?,
                        mode: row.get(6)?,
                        dataset: row.get(7)?,
                        exit_code: row.get(8)?,
                    })
                },
            )
            .ok()
    }

    /// Record a UUID as the latest for a given key (e.g. "dirty").
    ///
    /// Used so `brokkr results dirty` resolves to the most recent
    /// dirty/failed run, even though each run gets a unique UUID.
    pub fn set_latest(&self, key: &str, uuid: &str) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sidecar_latest (key, uuid) VALUES (?1, ?2)",
            rusqlite::params![key, uuid],
        )?;
        Ok(())
    }

    /// Resolve a key to its latest UUID, or return the input unchanged.
    ///
    /// If `uuid_prefix` matches a key in `sidecar_latest`, returns the
    /// stored UUID. Otherwise returns the input as-is (for normal UUID
    /// prefix lookups).
    pub fn resolve_latest(&self, uuid_prefix: &str) -> String {
        self.conn
            .query_row(
                "SELECT uuid FROM sidecar_latest WHERE key = ?1",
                rusqlite::params![uuid_prefix],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| uuid_prefix.to_owned())
    }

    // -------------------------------------------------------------------
    // Read
    // -------------------------------------------------------------------

    /// Query sidecar samples for a result UUID prefix.
    ///
    /// If `run_idx` is `Some`, filters to that run only. If `None`, returns
    /// all runs. Resolves latest-keys (e.g. "dirty") before querying.
    pub fn query_samples(
        &self,
        uuid_prefix: &str,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Sample>, DevError> {
        let uuid_prefix = self.resolve_latest(uuid_prefix);
        let (sql, run_filter) = match run_idx {
            Some(idx) => (
                "SELECT sample_idx, timestamp_us,
                        rss_kb, anon_kb, file_kb, shmem_kb, swap_kb, vsize_kb, vm_hwm_kb,
                        utime, stime, num_threads, minflt, majflt,
                        rchar, wchar, read_bytes, write_bytes, cancelled_write_bytes,
                        syscr, syscw, vol_cs, nonvol_cs
                 FROM sidecar_samples
                 WHERE result_uuid LIKE ?1||'%' AND run_idx = ?2
                 ORDER BY sample_idx",
                Some(i64::try_from(idx).unwrap_or(0)),
            ),
            None => (
                "SELECT sample_idx, timestamp_us,
                        rss_kb, anon_kb, file_kb, shmem_kb, swap_kb, vsize_kb, vm_hwm_kb,
                        utime, stime, num_threads, minflt, majflt,
                        rchar, wchar, read_bytes, write_bytes, cancelled_write_bytes,
                        syscr, syscw, vol_cs, nonvol_cs
                 FROM sidecar_samples
                 WHERE result_uuid LIKE ?1||'%'
                 ORDER BY run_idx, sample_idx",
                None,
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(crate::sidecar::Sample {
                sample_idx: row.get(0)?,
                timestamp_us: row.get(1)?,
                rss_kb: row.get(2)?,
                anon_kb: row.get(3)?,
                file_kb: row.get(4)?,
                shmem_kb: row.get(5)?,
                swap_kb: row.get(6)?,
                vsize_kb: row.get(7)?,
                vm_hwm_kb: row.get(8)?,
                utime: row.get(9)?,
                stime: row.get(10)?,
                num_threads: row.get(11)?,
                minflt: row.get(12)?,
                majflt: row.get(13)?,
                rchar: row.get(14)?,
                wchar: row.get(15)?,
                read_bytes: row.get(16)?,
                write_bytes: row.get(17)?,
                cancelled_write_bytes: row.get(18)?,
                syscr: row.get(19)?,
                syscw: row.get(20)?,
                vol_cs: row.get(21)?,
                nonvol_cs: row.get(22)?,
            })
        };
        let rows = match run_filter {
            Some(idx) => stmt.query_map(rusqlite::params![uuid_prefix, idx], map_row)?,
            None => stmt.query_map(rusqlite::params![uuid_prefix], map_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Query sidecar markers for a result UUID prefix.
    ///
    /// If `run_idx` is `Some`, filters to that run only.
    /// Resolves latest-keys (e.g. "dirty") before querying.
    pub fn query_markers(
        &self,
        uuid_prefix: &str,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Marker>, DevError> {
        let uuid_prefix = self.resolve_latest(uuid_prefix);
        let (sql, run_filter) = match run_idx {
            Some(idx) => (
                "SELECT marker_idx, timestamp_us, name
                 FROM sidecar_markers
                 WHERE result_uuid LIKE ?1||'%' AND run_idx = ?2
                 ORDER BY marker_idx",
                Some(i64::try_from(idx).unwrap_or(0)),
            ),
            None => (
                "SELECT marker_idx, timestamp_us, name
                 FROM sidecar_markers
                 WHERE result_uuid LIKE ?1||'%'
                 ORDER BY run_idx, marker_idx",
                None,
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(crate::sidecar::Marker {
                marker_idx: row.get(0)?,
                timestamp_us: row.get(1)?,
                name: row.get(2)?,
            })
        };
        let rows = match run_filter {
            Some(idx) => stmt.query_map(rusqlite::params![uuid_prefix, idx], map_row)?,
            None => stmt.query_map(rusqlite::params![uuid_prefix], map_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Query sidecar counters for a result UUID prefix.
    ///
    /// If `run_idx` is `Some`, filters to that run only.
    /// Resolves latest-keys (e.g. "dirty") before querying.
    pub fn query_counters(
        &self,
        uuid_prefix: &str,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Counter>, DevError> {
        let uuid_prefix = self.resolve_latest(uuid_prefix);
        let (sql, run_filter) = match run_idx {
            Some(idx) => (
                "SELECT timestamp_us, name, value
                 FROM sidecar_counters
                 WHERE result_uuid LIKE ?1||'%' AND run_idx = ?2
                 ORDER BY timestamp_us, name",
                Some(i64::try_from(idx).unwrap_or(0)),
            ),
            None => (
                "SELECT timestamp_us, name, value
                 FROM sidecar_counters
                 WHERE result_uuid LIKE ?1||'%'
                 ORDER BY run_idx, timestamp_us, name",
                None,
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let map_row = |row: &rusqlite::Row<'_>| {
            Ok(crate::sidecar::Counter {
                timestamp_us: row.get(0)?,
                name: row.get(1)?,
                value: row.get(2)?,
            })
        };
        let rows = match run_filter {
            Some(idx) => stmt.query_map(rusqlite::params![uuid_prefix, idx], map_row)?,
            None => stmt.query_map(rusqlite::params![uuid_prefix], map_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Delete every sidecar row whose `result_uuid` starts with
    /// `uuid_prefix`. Also drops any `sidecar_latest` pointer rows that
    /// currently resolve to a deleted UUID so `brokkr sidecar dirty`
    /// doesn't point at a ghost.
    ///
    /// Does NOT call `resolve_latest` on the input: the caller has already
    /// resolved the real UUID (invalidate_cmd computes the full target
    /// UUIDs up front so the dry-run view matches what gets deleted).
    ///
    /// Returns the number of `sidecar_summary` rows removed (one per
    /// (uuid, run_idx) session, i.e. the run count).
    pub fn delete_by_uuid_prefix(&self, uuid_prefix: &str) -> Result<usize, DevError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM sidecar_samples WHERE result_uuid LIKE ?1||'%'",
            rusqlite::params![uuid_prefix],
        )?;
        tx.execute(
            "DELETE FROM sidecar_markers WHERE result_uuid LIKE ?1||'%'",
            rusqlite::params![uuid_prefix],
        )?;
        tx.execute(
            "DELETE FROM sidecar_counters WHERE result_uuid LIKE ?1||'%'",
            rusqlite::params![uuid_prefix],
        )?;
        tx.execute(
            "DELETE FROM sidecar_meta WHERE result_uuid LIKE ?1||'%'",
            rusqlite::params![uuid_prefix],
        )?;
        tx.execute(
            "DELETE FROM sidecar_latest WHERE uuid LIKE ?1||'%'",
            rusqlite::params![uuid_prefix],
        )?;
        let removed = tx.execute(
            "DELETE FROM sidecar_summary WHERE result_uuid LIKE ?1||'%'",
            rusqlite::params![uuid_prefix],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Enumerate distinct `result_uuid` values whose prefix matches.
    /// Used by `brokkr invalidate` to find sidecar-only runs (dirty/failed)
    /// that have no row in the results DB.
    pub fn uuids_matching_prefix(&self, uuid_prefix: &str) -> Result<Vec<String>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT result_uuid FROM sidecar_summary WHERE result_uuid LIKE ?1||'%' \
             UNION SELECT DISTINCT result_uuid FROM sidecar_meta WHERE result_uuid LIKE ?1||'%'",
        )?;
        let rows = stmt.query_map(rusqlite::params![uuid_prefix], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Enumerate distinct `result_uuid` values whose `sidecar_meta.git_commit`
    /// starts with `commit_prefix`. Only sessions that were tagged with a
    /// commit (v4+ schema) are found this way.
    pub fn uuids_matching_commit_prefix(
        &self,
        commit_prefix: &str,
    ) -> Result<Vec<String>, DevError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT result_uuid FROM sidecar_meta WHERE git_commit LIKE ?1||'%'",
        )?;
        let rows = stmt.query_map(rusqlite::params![commit_prefix], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Check whether sidecar data exists for a result UUID prefix.
    ///
    /// Resolves latest-keys (e.g. "dirty") before querying.
    pub fn has_data(&self, uuid_prefix: &str) -> bool {
        let uuid_prefix = self.resolve_latest(uuid_prefix);
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM sidecar_summary WHERE result_uuid LIKE ?1||'%'",
                rusqlite::params![uuid_prefix],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

/// Create a self-contained backup of a sidecar database using SQLite's
/// online backup API.
///
/// The destination is opened in DELETE journal mode (no WAL side files),
/// so the backup is a single restorable file. Returns `Ok(())` on success.
pub fn backup_to_path(src: &Path, dst: &Path) -> Result<(), DevError> {
    let src_conn = rusqlite::Connection::open_with_flags(
        src,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| DevError::Database(format!("backup: open source: {e}")))?;

    let mut dst_conn = rusqlite::Connection::open(dst)
        .map_err(|e| DevError::Database(format!("backup: open dest: {e}")))?;
    dst_conn
        .pragma_update(None, "journal_mode", "DELETE")
        .map_err(|e| DevError::Database(format!("backup: set journal mode: {e}")))?;

    let backup = rusqlite::backup::Backup::new(&src_conn, &mut dst_conn)
        .map_err(|e| DevError::Database(format!("backup: init: {e}")))?;

    // Copy all pages in one step (-1 = all remaining pages).
    backup
        .step(-1)
        .map_err(|e| DevError::Database(format!("backup: step: {e}")))?;

    // Drop the backup handle to release the mutable borrow on dst_conn.
    drop(backup);

    // Verify the backup is consistent.
    let result: String = dst_conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|e| DevError::Database(format!("backup: quick_check: {e}")))?;
    if result != "ok" {
        return Err(DevError::Database(format!(
            "backup: integrity check failed on destination ({result})"
        )));
    }

    // Close connections explicitly before caller fsyncs.
    drop(dst_conn);
    drop(src_conn);

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brokkr-sidecar-test-{}-{}",
            std::process::id(),
            suffix,
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Create a sidecar DB and insert a minimal marker row, returning the path.
    fn create_test_sidecar(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let db = SidecarDb::open(&path).unwrap();
        db.conn
            .execute(
                "INSERT INTO sidecar_markers (result_uuid, run_idx, marker_idx, \
                 timestamp_us, name) VALUES ('test-uuid', 0, 0, 1000, 'test')",
                [],
            )
            .unwrap();
        drop(db);
        path
    }

    #[test]
    fn creates_restorable_backup() {
        let dir = temp_dir("restorable");
        let src = create_test_sidecar(&dir, "src.db");
        let dst = dir.join("backup.db");

        backup_to_path(&src, &dst).unwrap();

        // Verify the backup is a single file (no WAL).
        assert!(dst.exists());
        assert!(!dir.join("backup.db-wal").exists());

        // Verify expected data.
        let conn = rusqlite::Connection::open_with_flags(
            &dst,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sidecar_markers WHERE result_uuid = 'test-uuid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_contains_latest_data_even_with_reader_open() {
        let dir = temp_dir("reader");
        let src = create_test_sidecar(&dir, "src-reader.db");
        let dst = dir.join("backup-reader.db");

        // Open a reader connection that stays alive during backup.
        let _reader = rusqlite::Connection::open_with_flags(
            &src,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();

        // Write more data while the reader is open.
        {
            let db = SidecarDb::open(&src).unwrap();
            db.conn
                .execute(
                    "INSERT INTO sidecar_markers (result_uuid, run_idx, marker_idx, \
                     timestamp_us, name) VALUES ('second-uuid', 0, 0, 2000, 'second')",
                    [],
                )
                .unwrap();
            // Don't drop db yet - keep writer alive too.

            backup_to_path(&src, &dst).unwrap();
        }

        // Verify backup has both rows.
        let conn = rusqlite::Connection::open_with_flags(
            &dst,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sidecar_markers", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_overwrites_existing_destination() {
        let dir = temp_dir("overwrite");
        let src = create_test_sidecar(&dir, "src-overwrite.db");
        let dst = dir.join("backup-overwrite.db");

        // Create an initial backup.
        backup_to_path(&src, &dst).unwrap();

        // Add more data to source.
        {
            let db = SidecarDb::open(&src).unwrap();
            db.conn()
                .execute(
                    "INSERT INTO sidecar_markers (result_uuid, run_idx, marker_idx, \
                     timestamp_us, name) VALUES ('second', 0, 0, 2000, 'second')",
                    [],
                )
                .unwrap();
        }

        // Backup again over the existing destination.
        backup_to_path(&src, &dst).unwrap();

        // Verify the backup has both rows.
        let conn = rusqlite::Connection::open_with_flags(
            &dst,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sidecar_markers", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);

        std::fs::remove_dir_all(&dir).ok();
    }
}
