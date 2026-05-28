//! Per-db `user_version` migrations for the corpus runs database.
//!
//! Mirrors `src/db/migrate.rs`. Version 1 is the initial schema, so there are
//! no migrations yet - `run_migrations` is a no-op until `SCHEMA_VERSION`
//! advances. The scaffold (and the `has_table` helper) lives here so that a
//! future v2 column add is a localized change, exactly as in `ResultsDb`.

use crate::error::DevError;

/// Current schema version. Increment when adding a migration below.
pub(super) const SCHEMA_VERSION: i64 = 1;

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

    // Future migrations slot in here, e.g.:
    //   if current < 2 { migrate_v1_to_v2(conn)?; }

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
