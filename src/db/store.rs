//! Shared SQLite connection and schema-version policy for measurement stores.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::error::DevError;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Check whether a SQLite table exists, preserving inspection errors for the
/// caller instead of interpreting them as absence.
pub(crate) fn table_exists(conn: &Connection, table: &str) -> Result<bool, DevError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Check whether a SQLite table has a column, preserving inspection errors
/// for the caller instead of interpreting them as absence.
pub(crate) fn column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, DevError> {
    let quoted_table = format!("\"{}\"", table.replace('"', "\"\""));
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({quoted_table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(columns.iter().any(|name| name == column))
}

/// Open a database for a store that may create or migrate data.
pub(crate) fn open_read_write(
    path: &Path,
    store_name: &str,
    schema_version: i64,
) -> Result<Connection, DevError> {
    let conn = Connection::open(path)?;
    configure_connection(&conn)?;
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure_supported_version(store_name, current, schema_version)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(conn)
}

/// Open an existing database without any operation that can change its file.
pub(crate) fn open_read_only(path: &Path) -> Result<Connection, DevError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    configure_connection(&conn)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

/// Apply a store's migrations and schema under one transaction, with this
/// module as the sole owner of its `user_version` stamp.
pub(crate) fn migrate(
    conn: &Connection,
    store_name: &str,
    schema_version: i64,
    apply: impl FnOnce(&Connection, i64) -> Result<(), DevError>,
) -> Result<(), DevError> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure_supported_version(store_name, current, schema_version)?;
    if current == schema_version {
        return Ok(());
    }

    // Reserve the write lock before reading the version. Two first writers
    // must not both plan migrations from the same stale stamp.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let migration = (|| {
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure_supported_version(store_name, current, schema_version)?;
        if current < schema_version {
            apply(conn, current)?;
            conn.pragma_update(None, "user_version", schema_version)?;
        }
        Ok::<(), DevError>(())
    })();
    if let Err(error) = migration {
        // The original error is the one worth reporting; a failed rollback
        // leaves the connection to be dropped.
        conn.execute_batch("ROLLBACK").ok();
        return Err(error);
    }
    if let Err(error) = conn.execute_batch("COMMIT") {
        // The original error is the one worth reporting; a failed rollback
        // leaves the connection to be dropped.
        conn.execute_batch("ROLLBACK").ok();
        return Err(error.into());
    }
    Ok(())
}

/// Open an existing database for reading at the current schema. A store on an
/// OLDER schema is first migrated by `migrate_store` (the store's own
/// read-write open, which runs the versioned migrations), then reopened
/// read-only, so an upgrade never makes a read-mostly project's history
/// unreadable. A store stamped NEWER than this brokkr knows is refused, and no
/// path here ever lowers a stamp.
pub(crate) fn open_read_only_migrating(
    path: &Path,
    store_name: &str,
    schema_version: i64,
    migrate_store: impl FnOnce() -> Result<(), DevError>,
) -> Result<Connection, DevError> {
    let conn = open_read_only(path)?;
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure_supported_version(store_name, current, schema_version)?;
    if current == schema_version {
        return Ok(conn);
    }
    drop(conn);
    migrate_store()?;
    open_read_only(path)
}

fn configure_connection(conn: &Connection) -> Result<(), DevError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

fn ensure_supported_version(
    store_name: &str,
    current: i64,
    schema_version: i64,
) -> Result<(), DevError> {
    if current > schema_version {
        return Err(DevError::Database(format!(
            "{store_name} schema version {current} is newer than this brokkr supports ({schema_version}); upgrade brokkr before opening this store"
        )));
    }
    Ok(())
}
