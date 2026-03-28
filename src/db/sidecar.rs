//! Sidecar profile database — separate from the results DB.
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

CREATE TABLE IF NOT EXISTS sidecar_latest (
    key   TEXT NOT NULL PRIMARY KEY,
    uuid  TEXT NOT NULL
);
";

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// Current sidecar schema version.
const SIDECAR_VERSION: i64 = 1;

/// Run pending migrations based on `PRAGMA user_version`.
fn run_sidecar_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current >= SIDECAR_VERSION {
        return Ok(());
    }

    // v0 -> v1: initial schema (handled by CREATE TABLE IF NOT EXISTS in DDL).
    // Future migrations go here:
    // if current < 2 { migrate_v1_to_v2(conn)?; }

    conn.pragma_update(None, "user_version", SIDECAR_VERSION)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// SidecarDb
// ---------------------------------------------------------------------------

/// Handle to the sidecar profile database (`.brokkr/sidecar.db`).
pub struct SidecarDb {
    conn: rusqlite::Connection,
}

impl SidecarDb {
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
    /// If `uuid_prefix` is a latest-key (e.g. "dirty"), it is resolved
    /// to the actual UUID first.
    pub fn query_samples(
        &self,
        uuid_prefix: &str,
    ) -> Result<Vec<crate::sidecar::Sample>, DevError> {
        let uuid_prefix = self.resolve_latest(uuid_prefix);
        let mut stmt = self.conn.prepare(
            "SELECT sample_idx, timestamp_us,
                    rss_kb, anon_kb, file_kb, shmem_kb, swap_kb, vsize_kb, vm_hwm_kb,
                    utime, stime, num_threads, minflt, majflt,
                    rchar, wchar, read_bytes, write_bytes, cancelled_write_bytes,
                    syscr, syscw, vol_cs, nonvol_cs
             FROM sidecar_samples
             WHERE result_uuid LIKE ?1||'%'
             ORDER BY run_idx, sample_idx",
        )?;
        let rows = stmt.query_map(rusqlite::params![uuid_prefix], |row| {
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
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Query sidecar markers for a result UUID prefix.
    ///
    /// Resolves latest-keys (e.g. "dirty") before querying.
    pub fn query_markers(
        &self,
        uuid_prefix: &str,
    ) -> Result<Vec<crate::sidecar::Marker>, DevError> {
        let uuid_prefix = self.resolve_latest(uuid_prefix);
        let mut stmt = self.conn.prepare(
            "SELECT marker_idx, timestamp_us, name
             FROM sidecar_markers
             WHERE result_uuid LIKE ?1||'%'
             ORDER BY run_idx, marker_idx",
        )?;
        let rows = stmt.query_map(rusqlite::params![uuid_prefix], |row| {
            Ok(crate::sidecar::Marker {
                marker_idx: row.get(0)?,
                timestamp_us: row.get(1)?,
                name: row.get(2)?,
            })
        })?;
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
