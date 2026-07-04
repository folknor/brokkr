//! Per-db `user_version` migrations for the corpus runs database.
//!
//! Mirrors `src/db/migrate.rs`. Version 1 was the initial schema; version 2
//! adds the per-probe `disposition.runtime_ms` column; version 3 adds the
//! `disposition.boundary_ours`/`boundary_tv` window-boundary-artifact discount
//! columns; version 4 adds `run.wall_ms`, brokkr's own measured whole-run
//! harness wall (the pre-run runtime ceiling estimates from these, not from
//! summing the harness's overlapping per-probe `runtime_ms`). On a fresh
//! database the schema DDL in `schema.rs` creates the current tables (columns
//! included) and stamps the version, so the migration steps below only run for
//! an older db on disk. The `has_table` helper lives here so a future column
//! add stays a localized change, exactly as in `ResultsDb`.

use crate::error::DevError;

/// Current schema version. Increment when adding a migration below.
pub(super) const SCHEMA_VERSION: i64 = 4;

/// Run all pending migrations based on `PRAGMA user_version`. On a fresh
/// database the schema DDL in `schema.rs` creates the v1 tables and stamps the
/// version, so there is nothing to migrate.
pub(super) fn run_migrations(conn: &rusqlite::Connection) -> Result<(), DevError> {
    if !has_table(conn, "run") {
        return Ok(());
    }

    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    // v1 -> v2: add the per-probe wall-clock runtime column. IF NOT EXISTS-style
    // guard via `has_column` keeps the migration idempotent if rerun.
    if current < 2 && !has_column(conn, "disposition", "runtime_ms") {
        conn.execute("ALTER TABLE disposition ADD COLUMN runtime_ms REAL", [])?;
    }

    // v2 -> v3: add the window-boundary-artifact discount columns. NOT NULL is
    // safe under ALTER ADD because of the DEFAULT 0 - existing rows (raw counts,
    // no discount recorded) read back as zero, which is exactly "nothing
    // discounted". Idempotency guarded by `has_column` as above.
    if current < 3 {
        if !has_column(conn, "disposition", "boundary_ours") {
            conn.execute(
                "ALTER TABLE disposition ADD COLUMN boundary_ours INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !has_column(conn, "disposition", "boundary_tv") {
            conn.execute(
                "ALTER TABLE disposition ADD COLUMN boundary_tv INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
    }

    // v3 -> v4: add brokkr's measured whole-run harness wall. Nullable (pre-v4
    // runs never recorded it, and a spawn failure has no wall), so no DEFAULT -
    // an absent wall reads back as NULL, which the ceiling estimator treats as
    // "no measured wall for this run". Idempotency guarded by `has_column`.
    if current < 4 && !has_column(conn, "run", "wall_ms") {
        conn.execute("ALTER TABLE run ADD COLUMN wall_ms REAL", [])?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

/// Check whether a column exists on a table (via `PRAGMA table_info`).
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| {
            let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for name in names {
                if name? == column {
                    found = true;
                }
            }
            Ok(found)
        })
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A v1 `disposition` table (the runtime_ms-less shape) plus the minimal
    /// `run` table the migration guard checks for. Enough to exercise the
    /// v1 -> v2 column add without touching disk.
    const V1_SCHEMA: &str = "\
        CREATE TABLE run (run_id INTEGER PRIMARY KEY);
        CREATE TABLE disposition (run_id INTEGER NOT NULL, probe TEXT NOT NULL, \
            outcome TEXT NOT NULL, disposition TEXT NOT NULL, PRIMARY KEY (run_id, probe));";

    #[test]
    fn v1_to_current_adds_all_columns_and_is_idempotent() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        assert!(!has_column(&conn, "disposition", "runtime_ms"));
        assert!(!has_column(&conn, "disposition", "boundary_ours"));
        assert!(!has_column(&conn, "disposition", "boundary_tv"));

        run_migrations(&conn).unwrap();
        // Every step v1 -> v4 runs when starting from v1.
        assert!(has_column(&conn, "disposition", "runtime_ms"));
        assert!(has_column(&conn, "disposition", "boundary_ours"));
        assert!(has_column(&conn, "disposition", "boundary_tv"));
        assert!(has_column(&conn, "run", "wall_ms"));
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Rerunning is a no-op (the version guard short-circuits; has_column
        // would also prevent a duplicate ALTER).
        run_migrations(&conn).unwrap();
        assert!(has_column(&conn, "disposition", "boundary_ours"));
    }

    #[test]
    fn v2_to_v3_adds_only_the_boundary_columns() {
        // A v2 db already has runtime_ms; the v2 -> v3 step adds just the two
        // boundary columns and leaves existing rows reading back as 0.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute("ALTER TABLE disposition ADD COLUMN runtime_ms REAL", [])
            .unwrap();
        conn.execute(
            "INSERT INTO disposition (run_id, probe, outcome, disposition) \
             VALUES (1, 'p1', 'parity', 'accepted')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();

        run_migrations(&conn).unwrap();
        assert!(has_column(&conn, "disposition", "boundary_ours"));
        assert!(has_column(&conn, "disposition", "boundary_tv"));
        // The pre-existing row reads back as "nothing discounted".
        let (bo, bt): (i64, i64) = conn
            .query_row(
                "SELECT boundary_ours, boundary_tv FROM disposition WHERE probe = 'p1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((bo, bt), (0, 0));
    }
}
