//! Per-db `user_version` migrations for the corpus runs database.
//!
//! Mirrors `src/db/migrate.rs`. Version 1 was the initial schema; version 2
//! adds the per-probe `disposition.runtime_ms` column. On a fresh database the
//! schema DDL in `schema.rs` creates the current tables (column included) and
//! stamps the version, so the migration steps below only run for a v1 db on
//! disk. The `has_table` helper lives here so a future column add stays a
//! localized change, exactly as in `ResultsDb`.

use crate::error::DevError;

/// Current schema version. Increment when adding a migration below.
pub(super) const SCHEMA_VERSION: i64 = 2;

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
    fn v1_to_v2_adds_runtime_ms_and_is_idempotent() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        assert!(!has_column(&conn, "disposition", "runtime_ms"));

        run_migrations(&conn).unwrap();
        assert!(has_column(&conn, "disposition", "runtime_ms"));
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Rerunning is a no-op (the version guard short-circuits; has_column
        // would also prevent a duplicate ALTER).
        run_migrations(&conn).unwrap();
        assert!(has_column(&conn, "disposition", "runtime_ms"));
    }
}
