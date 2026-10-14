//! Sidecar profile database - separate from the results DB.
//!
//! Sidecar data (samples, markers) is stored in `.brokkr/sidecar.db`; the
//! results DB (`.brokkr/results.db`) is stored beside it. Both are gitignored
//! and linked by result UUID.

use std::path::Path;

use super::RunId;
use super::like::{ESCAPE, prefix_pattern, require_prefix};
use crate::error::DevError;
use crate::output::Ending;
use rusqlite::OptionalExtension;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SIDECAR_SCHEMA: &str = "\
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
    capture_truncated INTEGER NOT NULL DEFAULT 0,
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
    exit_code      INTEGER,
    ending         TEXT
);

CREATE TABLE IF NOT EXISTS sidecar_latest (
    key   TEXT NOT NULL PRIMARY KEY,
    uuid  TEXT NOT NULL
);
";

pub(crate) fn sample_table_schema() -> String {
    let mut columns = vec![
        "result_uuid TEXT NOT NULL".to_owned(),
        "run_idx INTEGER NOT NULL DEFAULT 0".to_owned(),
    ];
    columns.extend(crate::sidecar::Sample::db_column_definitions());
    columns.push("PRIMARY KEY (result_uuid, run_idx, sample_idx)".to_owned());
    format!(
        "CREATE TABLE IF NOT EXISTS sidecar_samples (\n    {}\n);\n",
        columns.join(",\n    ")
    )
}

fn sidecar_schema() -> String {
    format!("{}{SIDECAR_SCHEMA}", sample_table_schema())
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// Current sidecar schema version.
const SIDECAR_VERSION: i64 = 8;

/// Apply migrations newer than `current`; version checks and stamping are
/// centralized in `db::store`.
fn run_sidecar_migrations(conn: &rusqlite::Connection, current: i64) -> Result<(), DevError> {
    // v0 -> v1: initial schema (handled by CREATE TABLE IF NOT EXISTS in DDL).
    // v1 -> v2: add sidecar_meta table (handled by CREATE TABLE IF NOT EXISTS in DDL).
    // v2 -> v3: add sidecar_counters table (handled by CREATE TABLE IF NOT EXISTS in DDL).
    //           Existing UUIDs without meta rows get defaults on query (best_run_idx=0).
    // v3 -> v4: add run provenance columns to sidecar_meta.
    if current < 4 {
        for (name, declaration) in [
            ("run_start_epoch", "run_start_epoch INTEGER"),
            ("pid", "pid INTEGER"),
            ("command", "command TEXT"),
            ("binary_path", "binary_path TEXT"),
            ("binary_xxh128", "binary_xxh128 TEXT"),
            ("git_commit", "git_commit TEXT"),
            ("mode", "mode TEXT"),
            ("dataset", "dataset TEXT"),
            ("exit_code", "exit_code INTEGER"),
        ] {
            if !super::store::column_exists(conn, "sidecar_meta", name)? {
                conn.execute_batch(&format!(
                    "ALTER TABLE sidecar_meta ADD COLUMN {declaration}"
                ))?;
            }
        }
    }
    // v4 -> v5: rename `variant` column to `mode` to match the results
    //           DB rename in v13→v14. Only applies if the old column
    //           still exists (fresh DBs created with the v5 DDL already
    //           have `mode`).
    if current < 5 {
        let has_variant = super::store::column_exists(conn, "sidecar_meta", "variant")?;
        let has_mode = super::store::column_exists(conn, "sidecar_meta", "mode")?;
        if has_variant && !has_mode {
            conn.execute_batch("ALTER TABLE sidecar_meta RENAME COLUMN variant TO mode")?;
        }
    }
    // v5 -> v6: add `ending`, the supervisor's typed account of how the run
    //           ended (`exit_code` alone could not say "deadline" or "stop
    //           marker", and folded a signal into 128+N). Rows written before
    //           it keep their `exit_code` and read back through it.
    if current < 6 && !super::store::column_exists(conn, "sidecar_meta", "ending")? {
        conn.execute_batch("ALTER TABLE sidecar_meta ADD COLUMN ending TEXT")?;
    }
    // v6 -> v7: sidecar_summary is an identity index only. Its metric
    // columns were written but never queried; samples and markers are the
    // authoritative values.
    if current < 7 {
        drop_summary_payload(conn)?;
    }
    // v7 -> v8: record whether the bounded in-memory trajectory omitted any
    // samples, markers, counters, or overlong FIFO records.
    if current < 8 && !super::store::column_exists(conn, "sidecar_summary", "capture_truncated")? {
        conn.execute_batch(
            "ALTER TABLE sidecar_summary ADD COLUMN capture_truncated INTEGER NOT NULL DEFAULT 0",
        )?;
    }

    Ok(())
}

fn drop_summary_payload(conn: &rusqlite::Connection) -> Result<(), DevError> {
    for column in ["vm_hwm_kb", "sample_count", "marker_count", "wall_time_ms"] {
        if super::store::column_exists(conn, "sidecar_summary", column)? {
            conn.execute_batch(&format!("ALTER TABLE sidecar_summary DROP COLUMN {column}"))?;
        }
    }
    Ok(())
}

/// The stored spelling of a run's ending: `exited:N`, `signal:N`,
/// `deadline:MS`, `stop_marker:NAME`, `interrupted`, `wait_failed:WHY`.
fn encode_ending(ending: &Ending) -> String {
    match ending {
        Ending::Exited(code) => format!("exited:{code}"),
        Ending::Signalled(signal) => format!("signal:{signal}"),
        Ending::Deadline(limit) => format!("deadline:{}", limit.as_millis()),
        Ending::StopMarker(marker) => format!("stop_marker:{marker}"),
        Ending::Interrupted => "interrupted".to_owned(),
        Ending::WaitFailed(why) => format!("wait_failed:{why}"),
    }
}

/// Read a stored ending back; `None` for a spelling this version does not
/// know (never guessed into a different ending).
fn decode_ending(text: &str) -> Option<Ending> {
    let (kind, value) = text.split_once(':').unwrap_or((text, ""));
    match kind {
        "exited" => value.parse().ok().map(Ending::Exited),
        "signal" => value.parse().ok().map(Ending::Signalled),
        "deadline" => value
            .parse()
            .ok()
            .map(|ms| Ending::Deadline(std::time::Duration::from_millis(ms))),
        "stop_marker" => Some(Ending::StopMarker(value.to_owned())),
        "interrupted" => Some(Ending::Interrupted),
        "wait_failed" => Some(Ending::WaitFailed(value.to_owned())),
        _ => None,
    }
}

/// The legacy integer column, still written so older brokkr versions reading
/// this database see what they always saw: the exit code, or `128 + signal`.
fn legacy_exit_code(ending: &Ending) -> Option<i32> {
    match ending {
        Ending::Exited(code) => Some(*code),
        Ending::Signalled(signal) => Some(128 + signal),
        Ending::Deadline(_)
        | Ending::StopMarker(_)
        | Ending::Interrupted
        | Ending::WaitFailed(_) => None,
    }
}

/// A row from before v6: only the integer, read by the convention it was
/// written with (above 128 is a signal).
fn ending_from_legacy_exit_code(code: i32) -> Ending {
    if code > 128 {
        Ending::Signalled(code - 128)
    } else {
        Ending::Exited(code)
    }
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
    /// Read with the results store's policy: legacy non-commit text is kept
    /// verbatim, never refused ([`crate::git::ShortCommit::from_stored_lossy`]).
    pub git_commit: Option<crate::git::ShortCommit>,
    pub mode: Option<String>,
    pub dataset: Option<String>,
    /// How the run ended, as the child supervisor assigned it. `None` when
    /// the caller could not observe it.
    pub ending: Option<Ending>,
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

    /// Open (or create) the sidecar database at `path`, enable WAL, and apply
    /// pending migrations.
    pub fn open(path: &Path) -> Result<Self, DevError> {
        let conn = super::store::open_read_write(path, "sidecar.db", SIDECAR_VERSION)?;
        super::store::migrate(&conn, "sidecar.db", SIDECAR_VERSION, |conn, version| {
            conn.execute_batch(&sidecar_schema())?;
            run_sidecar_migrations(conn, version)?;
            Ok(())
        })?;
        Ok(Self { conn })
    }

    /// Open an existing sidecar database for reading. An older schema is
    /// migrated first (through [`Self::open`]); a newer one is refused.
    pub(crate) fn open_read_only(path: &Path) -> Result<Self, DevError> {
        let conn =
            super::store::open_read_only_migrating(path, "sidecar.db", SIDECAR_VERSION, || {
                Self::open(path).map(drop)
            })?;
        Ok(Self { conn })
    }

    // -------------------------------------------------------------------
    // Write
    // -------------------------------------------------------------------

    /// Bulk-insert sidecar profile data for one benchmark run.
    /// Sidecar rows share the durable lifetime of results; only an explicit
    /// `invalidate` removes them, so the store has no age-based pruning.
    #[allow(clippy::cast_possible_wrap)]
    pub fn store_run(
        &self,
        result_uuid: &str,
        run_idx: usize,
        data: &crate::sidecar::SidecarData,
    ) -> Result<(), DevError> {
        let tx = self.conn.unchecked_transaction()?;

        {
            let sample_columns = crate::sidecar::Sample::db_column_names();
            let columns = sample_columns.join(", ");
            let placeholders = (1..=sample_columns.len() + 2)
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO sidecar_samples (result_uuid, run_idx, {columns}) VALUES ({placeholders})"
            );
            let mut stmt = tx.prepare_cached(&sql)?;

            for s in &data.samples {
                let mut values = vec![
                    rusqlite::types::Value::Text(result_uuid.to_owned()),
                    rusqlite::types::Value::Integer(run_idx as i64),
                ];
                values.extend(s.db_values());
                stmt.execute(rusqlite::params_from_iter(values))?;
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
            "INSERT INTO sidecar_summary (result_uuid, run_idx, capture_truncated)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                result_uuid,
                run_idx as i64,
                if data.summary.capture_truncated {
                    1_i64
                } else {
                    0_i64
                }
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
                git_commit, mode, dataset, exit_code, ending
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                result_uuid,
                i64::try_from(best_run_idx).unwrap_or(0),
                i64::try_from(total_runs).unwrap_or(1),
                run_info.and_then(|r| r.run_start_epoch),
                run_info.and_then(|r| r.pid),
                run_info.as_ref().and_then(|r| r.command.as_deref()),
                run_info.as_ref().and_then(|r| r.binary_path.as_deref()),
                run_info.as_ref().and_then(|r| r.binary_xxh128.as_deref()),
                run_info
                    .as_ref()
                    .and_then(|r| r.git_commit.as_ref())
                    .map(crate::git::ShortCommit::as_str),
                run_info.as_ref().and_then(|r| r.mode.as_deref()),
                run_info.as_ref().and_then(|r| r.dataset.as_deref()),
                run_info
                    .and_then(|r| r.ending.as_ref())
                    .and_then(legacy_exit_code),
                run_info.and_then(|r| r.ending.as_ref()).map(encode_ending),
            ],
        )?;
        Ok(())
    }

    /// Query metadata for one identity already resolved by `RunStore`.
    /// Missing metadata keeps the historical default, while database errors
    /// remain visible to the caller.
    pub(crate) fn query_meta_for(&self, id: &RunId) -> Result<(usize, usize), DevError> {
        let found = self
            .conn
            .query_row(
                "SELECT best_run_idx, total_runs FROM sidecar_meta
                 WHERE result_uuid = ?1",
                rusqlite::params![id.as_str()],
                |row| {
                    let best: i64 = row.get(0)?;
                    let total: i64 = row.get(1)?;
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    Ok((best.max(0) as usize, total.max(0) as usize))
                },
            )
            .optional()?;
        Ok(found.unwrap_or((0, 1)))
    }

    /// Whether any selected sidecar run was stored after a trajectory limit
    /// omitted data. `None` selects every run in the session.
    pub(crate) fn query_capture_truncated_for(
        &self,
        id: &RunId,
        run_idx: Option<usize>,
    ) -> Result<bool, DevError> {
        let truncated = match run_idx {
            Some(idx) => self.conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sidecar_summary
                    WHERE result_uuid = ?1 AND run_idx = ?2 AND capture_truncated != 0
                 )",
                rusqlite::params![id.as_str(), i64::try_from(idx).unwrap_or(i64::MAX)],
                |row| row.get::<_, bool>(0),
            )?,
            None => self.conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sidecar_summary
                    WHERE result_uuid = ?1 AND capture_truncated != 0
                 )",
                rusqlite::params![id.as_str()],
                |row| row.get::<_, bool>(0),
            )?,
        };
        Ok(truncated)
    }

    /// Query provenance for one identity already resolved by `RunStore`.
    pub(crate) fn query_run_info_for(&self, id: &RunId) -> Result<Option<RunInfo>, DevError> {
        let info = self
            .conn
            .query_row(
                "SELECT run_start_epoch, pid, command, binary_path, binary_xxh128,
                        git_commit, mode, dataset, exit_code, ending
                 FROM sidecar_meta WHERE result_uuid = ?1",
                rusqlite::params![id.as_str()],
                |row| {
                    let stored: Option<String> = row.get(9)?;
                    let legacy: Option<i32> = row.get(8)?;
                    let ending = match stored {
                        Some(text) => decode_ending(&text),
                        None => legacy.map(ending_from_legacy_exit_code),
                    };
                    Ok(RunInfo {
                        run_start_epoch: row.get(0)?,
                        pid: row.get(1)?,
                        command: row.get(2)?,
                        binary_path: row.get(3)?,
                        binary_xxh128: row.get(4)?,
                        git_commit: row
                            .get::<_, Option<String>>(5)?
                            .map(|raw| crate::git::ShortCommit::from_stored_lossy(&raw)),
                        mode: row.get(6)?,
                        dataset: row.get(7)?,
                        ending,
                    })
                },
            )
            .optional()?;
        Ok(info)
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
    pub(crate) fn resolve_latest(&self, uuid_prefix: &str) -> Result<String, DevError> {
        let latest = self
            .conn
            .query_row(
                "SELECT uuid FROM sidecar_latest WHERE key = ?1",
                rusqlite::params![uuid_prefix],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(latest.unwrap_or_else(|| uuid_prefix.to_owned()))
    }

    // -------------------------------------------------------------------
    // Read
    // -------------------------------------------------------------------

    /// Query samples for an identity already resolved by `RunStore`.
    pub(crate) fn query_samples_for(
        &self,
        id: &RunId,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Sample>, DevError> {
        let uuid_prefix = id.as_str();
        let columns = crate::sidecar::Sample::db_column_names().join(", ");
        let (sql, run_filter) = match run_idx {
            Some(idx) => (
                format!(
                    "SELECT {columns} FROM sidecar_samples \
                     WHERE result_uuid = ?1 AND run_idx = ?2 ORDER BY sample_idx"
                ),
                Some(i64::try_from(idx).unwrap_or(0)),
            ),
            None => (
                format!(
                    "SELECT {columns} FROM sidecar_samples \
                     WHERE result_uuid = ?1 ORDER BY run_idx, sample_idx"
                ),
                None,
            ),
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let map_row = crate::sidecar::Sample::from_db_row;
        let rows = match run_filter {
            Some(idx) => stmt.query_map(rusqlite::params![uuid_prefix, idx], map_row)?,
            None => stmt.query_map(rusqlite::params![uuid_prefix], map_row)?,
        };
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Query markers for an identity already resolved by `RunStore`. If
    /// `run_idx` is `Some`, filters to that run only.
    pub(crate) fn query_markers_for(
        &self,
        id: &RunId,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Marker>, DevError> {
        let uuid_prefix = id.as_str();
        let (sql, run_filter) = match run_idx {
            Some(idx) => (
                "SELECT marker_idx, timestamp_us, name
                 FROM sidecar_markers
                 WHERE result_uuid = ?1 AND run_idx = ?2
                 ORDER BY marker_idx",
                Some(i64::try_from(idx).unwrap_or(0)),
            ),
            None => (
                "SELECT marker_idx, timestamp_us, name
                 FROM sidecar_markers
                 WHERE result_uuid = ?1
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

    /// Query counters for an identity already resolved by `RunStore`.
    pub(crate) fn query_counters_for(
        &self,
        id: &RunId,
        run_idx: Option<usize>,
    ) -> Result<Vec<crate::sidecar::Counter>, DevError> {
        let uuid_prefix = id.as_str();
        let (sql, run_filter) = match run_idx {
            Some(idx) => (
                "SELECT timestamp_us, name, value
                 FROM sidecar_counters
                 WHERE result_uuid = ?1 AND run_idx = ?2
                 ORDER BY timestamp_us, name",
                Some(i64::try_from(idx).unwrap_or(0)),
            ),
            None => (
                "SELECT timestamp_us, name, value
                 FROM sidecar_counters
                 WHERE result_uuid = ?1
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

    /// Delete one canonical session identity, along with any `sidecar_latest`
    /// pointer rows that resolve to it so `brokkr sidecar dirty` doesn't point
    /// at a ghost. Equality makes it impossible for a resolved UUID to be
    /// interpreted as a prefix selection.
    ///
    /// Returns the number of `sidecar_summary` rows removed (one per
    /// (uuid, run_idx) session, i.e. the run count).
    pub(crate) fn delete_by_run_id(&self, id: &RunId) -> Result<usize, DevError> {
        let tx = self.conn.unchecked_transaction()?;
        for (table, column) in [
            ("sidecar_samples", "result_uuid"),
            ("sidecar_markers", "result_uuid"),
            ("sidecar_counters", "result_uuid"),
            ("sidecar_meta", "result_uuid"),
            ("sidecar_latest", "uuid"),
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE {column} = ?1"),
                rusqlite::params![id.as_str()],
            )?;
        }
        let removed = tx.execute(
            "DELETE FROM sidecar_summary WHERE result_uuid = ?1",
            rusqlite::params![id.as_str()],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Enumerate distinct `result_uuid` values whose prefix matches, sorted.
    /// Used by `brokkr invalidate` to find sidecar-only runs (dirty/failed)
    /// that have no row in the results DB, and by [`super::RunStore`].
    pub fn uuids_matching_prefix(&self, uuid_prefix: &str) -> Result<Vec<String>, DevError> {
        require_prefix(uuid_prefix, "uuid")?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT result_uuid FROM sidecar_summary WHERE result_uuid LIKE ?1 {ESCAPE} \
             UNION SELECT result_uuid FROM sidecar_meta WHERE result_uuid LIKE ?1 {ESCAPE} \
             ORDER BY 1"
        ))?;
        let rows = stmt.query_map(rusqlite::params![prefix_pattern(uuid_prefix)], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Enumerate distinct `result_uuid` values whose `sidecar_meta.git_commit`
    /// matches `commit_prefix` under the results DB's two-way prefix rule
    /// ([`super::query::commit_match_expr`]), so `invalidate --commit` drops
    /// the same runs from both databases. Only sessions that were tagged with
    /// a commit (v4+ schema) are found this way.
    pub fn uuids_matching_commit_prefix(
        &self,
        commit_prefix: &str,
    ) -> Result<Vec<String>, DevError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT result_uuid FROM sidecar_meta WHERE {}",
            super::query::commit_match_expr_on("git_commit", 1, 2)
        ))?;
        let (pattern, raw) = super::query::commit_params(commit_prefix);
        let rows = stmt.query_map(rusqlite::params![pattern, raw], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(DevError::from)
    }

    /// Check whether one already resolved identity has sidecar summary rows.
    pub(crate) fn has_data_for(&self, id: &RunId) -> Result<bool, DevError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sidecar_summary WHERE result_uuid = ?1)",
            rusqlite::params![id.as_str()],
            |row| row.get(0),
        )?)
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
    let src_conn =
        rusqlite::Connection::open_with_flags(src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
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
        crate::test_scratch::scratch("db-sidecar", suffix)
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

    fn insert_session(db: &SidecarDb, uuid: &str, marker: &str) {
        db.conn
            .execute(
                "INSERT INTO sidecar_summary (result_uuid, run_idx) VALUES (?1, 0)",
                rusqlite::params![uuid],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO sidecar_markers (result_uuid, run_idx, marker_idx, \
                 timestamp_us, name) VALUES (?1, 0, 0, 1000, ?2)",
                rusqlite::params![uuid, marker],
            )
            .unwrap();
    }

    // A read-only open of a store on the previous schema migrates it first and
    // then reads current rows; existing history stays readable after an upgrade.
    #[test]
    fn read_only_open_migrates_an_older_store() {
        let path = crate::test_scratch::scratch("db-sidecar", "ro_older").join("s.db");
        let db = SidecarDb::open(&path).unwrap();
        insert_session(&db, "old111", "M");
        drop(db);
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute_batch(
            "ALTER TABLE sidecar_meta DROP COLUMN ending;
             ALTER TABLE sidecar_summary DROP COLUMN capture_truncated;
             ALTER TABLE sidecar_summary ADD COLUMN vm_hwm_kb INTEGER;
             ALTER TABLE sidecar_summary ADD COLUMN sample_count INTEGER;
             ALTER TABLE sidecar_summary ADD COLUMN marker_count INTEGER;
             ALTER TABLE sidecar_summary ADD COLUMN wall_time_ms INTEGER;",
        )
        .unwrap();
        raw.pragma_update(None, "user_version", SIDECAR_VERSION - 3)
            .unwrap();
        drop(raw);

        let ro = SidecarDb::open_read_only(&path).unwrap();
        let version: i64 = ro
            .conn()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SIDECAR_VERSION);
        let has_ending = ro.conn().prepare("SELECT ending FROM sidecar_meta").is_ok();
        assert!(has_ending, "the migrated schema has the current column");
        for column in ["vm_hwm_kb", "sample_count", "marker_count", "wall_time_ms"] {
            assert!(
                !super::super::store::column_exists(ro.conn(), "sidecar_summary", column).unwrap(),
                "legacy summary column {column} is removed"
            );
        }
        let markers: i64 = ro
            .conn()
            .query_row("SELECT COUNT(*) FROM sidecar_markers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(markers, 1, "existing rows survive the migration");
    }

    // A store stamped newer than this brokkr knows is refused, and the stamp is
    // left exactly as found.
    #[test]
    fn read_only_open_refuses_a_newer_store() {
        let path = crate::test_scratch::scratch("db-sidecar", "ro_newer").join("s.db");
        drop(SidecarDb::open(&path).unwrap());
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.pragma_update(None, "user_version", SIDECAR_VERSION + 1)
            .unwrap();
        drop(raw);

        let err = SidecarDb::open_read_only(&path).err().expect("refused");
        assert!(err.to_string().contains("newer than this brokkr"), "{err}");
        let raw = rusqlite::Connection::open(&path).unwrap();
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SIDECAR_VERSION + 1, "no downgrade");
    }

    // A prefix shared by two sessions must be surfaced as ambiguous so callers
    // cannot accidentally combine two timelines.
    #[test]
    fn ambiguous_prefix_is_reported_not_merged() {
        let path = crate::test_scratch::scratch("db-sidecar", "ambiguous").join("s.db");
        let db = SidecarDb::open(&path).unwrap();
        insert_session(&db, "abc111", "A");
        insert_session(&db, "abc222", "B");

        let store =
            super::super::RunStore::open_read_only(&path.with_file_name("r.db"), &path).unwrap();
        let ids = match store.resolve(super::super::RunRef::Uuid("abc")).unwrap() {
            super::super::Resolved::Ambiguous(ids) => ids,
            _ => panic!("expected an ambiguous prefix"),
        };
        assert!(
            ids.iter().any(|id| id.as_str() == "abc111")
                && ids.iter().any(|id| id.as_str() == "abc222"),
            "lists both candidates: {ids:?}"
        );

        let id = match store.resolve(super::super::RunRef::Uuid("abc1")).unwrap() {
            super::super::Resolved::One(id) => id,
            _ => panic!("expected one result"),
        };
        let markers = store.query_sidecar_markers(&id, None).unwrap();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].name, "A");
        assert!(matches!(
            store.resolve(super::super::RunRef::Uuid("abc2")).unwrap(),
            super::super::Resolved::One(id) if id.as_str() == "abc222"
        ));
    }

    #[test]
    fn empty_and_wildcard_prefixes_match_nothing_extra() {
        let path = crate::test_scratch::scratch("db-sidecar", "empty_wildcard").join("s.db");
        let db = SidecarDb::open(&path).unwrap();
        insert_session(&db, "abc111", "A");

        let store =
            super::super::RunStore::open_read_only(&path.with_file_name("r.db"), &path).unwrap();
        assert!(store.resolve(super::super::RunRef::Uuid("")).is_err());
        assert!(
            db.uuids_matching_prefix("a_c").unwrap().is_empty(),
            "_ is literal"
        );
        assert!(
            db.uuids_matching_prefix("%").unwrap().is_empty(),
            "% is literal"
        );
        assert_eq!(
            db.delete_by_run_id(&RunId::new("abc".to_owned())).unwrap(),
            0,
            "a prefix is not an identity"
        );
        assert_eq!(
            db.delete_by_run_id(&RunId::new("abc111".to_owned()))
                .unwrap(),
            1
        );
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
        let conn =
            rusqlite::Connection::open_with_flags(&dst, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
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
        let _reader =
            rusqlite::Connection::open_with_flags(&src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
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
        let conn =
            rusqlite::Connection::open_with_flags(&dst, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sidecar_markers", [], |row| row.get(0))
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
        let conn =
            rusqlite::Connection::open_with_flags(&dst, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sidecar_markers", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        std::fs::remove_dir_all(&dir).ok();
    }
}
