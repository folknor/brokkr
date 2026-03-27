//! Command history database.
//!
//! Global SQLite database at `$XDG_DATA_HOME/brokkr/history.db` that records
//! every brokkr invocation with timing, exit status, and system context.

use std::path::PathBuf;

use rusqlite::Connection;

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA_VERSION: i64 = 1;

const CREATE_TABLE: &str = "\
    CREATE TABLE IF NOT EXISTS history (
        id              INTEGER PRIMARY KEY,
        timestamp       TEXT NOT NULL DEFAULT (datetime('now')),
        project         TEXT,
        cwd             TEXT NOT NULL,
        command         TEXT NOT NULL,
        elapsed_ms      INTEGER,
        exit_status     INTEGER NOT NULL,
        hostname        TEXT NOT NULL,
        commit_hash     TEXT,
        dirty           INTEGER,
        kernel          TEXT,
        avail_memory_mb INTEGER
    )";

const CREATE_INDEXES: &str = "\
    CREATE INDEX IF NOT EXISTS idx_history_timestamp ON history(timestamp);
    CREATE INDEX IF NOT EXISTS idx_history_project ON history(project);
    CREATE INDEX IF NOT EXISTS idx_history_exit ON history(exit_status)";

// ---------------------------------------------------------------------------
// Database handle
// ---------------------------------------------------------------------------

pub struct HistoryDb {
    conn: Connection,
}

impl HistoryDb {
    /// Open (or create) the history database at the XDG data path.
    pub fn open() -> Result<Self, DevError> {
        let path = db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        run_migrations(&conn)?;
        conn.execute_batch(CREATE_TABLE)?;
        conn.execute_batch(CREATE_INDEXES)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn })
    }

    /// Insert a history row.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(&self, row: &HistoryRow) -> Result<(), DevError> {
        self.conn.execute(
            "INSERT INTO history (project, cwd, command, elapsed_ms, exit_status, \
                hostname, commit_hash, dirty, kernel, avail_memory_mb) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                row.project,
                row.cwd,
                row.command,
                row.elapsed_ms,
                row.exit_status,
                row.hostname,
                row.commit_hash,
                row.dirty,
                row.kernel,
                row.avail_memory_mb,
            ],
        )?;
        Ok(())
    }

    /// Query history rows with optional filters.
    pub fn query(&self, filter: &HistoryFilter) -> Result<Vec<HistoryEntry>, DevError> {
        let mut clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref cmd) = filter.command {
            params.push(Box::new(format!("%{cmd}%")));
            clauses.push(format!("command LIKE ?{}", params.len()));
        }
        if let Some(ref project) = filter.project {
            params.push(Box::new(project.clone()));
            clauses.push(format!("project = ?{}", params.len()));
        }
        if filter.failed {
            clauses.push("exit_status != 0".to_owned());
        }
        if let Some(ref since) = filter.since {
            params.push(Box::new(since.clone()));
            clauses.push(format!("timestamp >= ?{}", params.len()));
        }
        if let Some(slow_ms) = filter.slow_ms {
            params.push(Box::new(slow_ms));
            clauses.push(format!("elapsed_ms >= ?{}", params.len()));
        }

        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };

        let limit = if filter.all {
            String::new()
        } else {
            format!(" LIMIT {}", filter.limit)
        };

        let sql = format!(
            "SELECT id, timestamp, project, cwd, command, elapsed_ms, exit_status, \
                    hostname, commit_hash, dirty, kernel, avail_memory_mb \
             FROM history{where_clause} ORDER BY id DESC{limit}"
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(AsRef::as_ref).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(HistoryEntry {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    project: row.get(2)?,
                    cwd: row.get(3)?,
                    command: row.get(4)?,
                    elapsed_ms: row.get(5)?,
                    exit_status: row.get(6)?,
                    hostname: row.get(7)?,
                    commit_hash: row.get(8)?,
                    dirty: row.get(9)?,
                    kernel: row.get(10)?,
                    avail_memory_mb: row.get(11)?,
                })
            })?
            .filter_map(Result::ok)
            .collect();

        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Row data for insertion.
pub struct HistoryRow {
    pub project: Option<String>,
    pub cwd: String,
    pub command: String,
    pub elapsed_ms: i64,
    pub exit_status: i32,
    pub hostname: String,
    pub commit_hash: Option<String>,
    pub dirty: Option<bool>,
    pub kernel: Option<String>,
    pub avail_memory_mb: Option<i64>,
}

/// Row data returned from queries.
#[allow(dead_code)]
pub struct HistoryEntry {
    pub id: i64,
    pub timestamp: String,
    pub project: Option<String>,
    pub cwd: String,
    pub command: String,
    pub elapsed_ms: Option<i64>,
    pub exit_status: i32,
    pub hostname: String,
    pub commit_hash: Option<String>,
    pub dirty: Option<bool>,
    pub kernel: Option<String>,
    pub avail_memory_mb: Option<i64>,
}

/// Filters for history queries.
pub struct HistoryFilter {
    pub command: Option<String>,
    pub project: Option<String>,
    pub failed: bool,
    pub since: Option<String>,
    pub slow_ms: Option<i64>,
    pub limit: usize,
    pub all: bool,
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format history entries for display.
pub fn format_history(entries: &[HistoryEntry]) -> String {
    if entries.is_empty() {
        return String::from("no history entries");
    }

    let mut lines = Vec::with_capacity(entries.len());
    for entry in entries {
        let project = entry.project.as_deref().unwrap_or("-");
        let elapsed = match entry.elapsed_ms {
            Some(ms) if ms >= 60_000 => format!("{:.1}m", ms as f64 / 60_000.0),
            Some(ms) if ms >= 1_000 => format!("{:.1}s", ms as f64 / 1_000.0),
            Some(ms) => format!("{ms}ms"),
            None => "-".to_owned(),
        };

        let fail_tag = if entry.exit_status != 0 {
            format!("  [FAIL:{}]", entry.exit_status)
        } else {
            String::new()
        };

        let dirty_tag = if entry.dirty == Some(true) { "*" } else { "" };
        let commit = entry.commit_hash.as_deref().unwrap_or("");
        let commit_display = if commit.is_empty() {
            String::new()
        } else {
            format!(" {commit}{dirty_tag}")
        };

        lines.push(format!(
            "{ts}  {project:<10} {elapsed:>7}  {cmd}{commit_display}{fail_tag}",
            ts = &entry.timestamp,
            cmd = entry.command,
        ));
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Return the path to the history database.
///
/// Uses `$XDG_DATA_HOME/brokkr/history.db`, falling back to
/// `$HOME/.local/share/brokkr/history.db`.
fn db_path() -> Result<PathBuf, DevError> {
    let data_dir = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local").join("share")
    } else {
        return Err(DevError::Config(
            "cannot determine data directory: neither XDG_DATA_HOME nor HOME is set".into(),
        ));
    };

    Ok(data_dir.join("brokkr").join("history.db"))
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

fn run_migrations(conn: &Connection) -> Result<(), DevError> {
    if !has_table(conn, "history") {
        return Ok(());
    }

    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if current >= SCHEMA_VERSION {
        return Ok(());
    }

    // Future migrations go here:
    // if current < 2 { migrate_v1_to_v2(conn)?; }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn has_table(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Open an in-memory history DB for testing.
    fn test_db() -> HistoryDb {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.execute_batch(CREATE_TABLE).unwrap();
        conn.execute_batch(CREATE_INDEXES).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();
        HistoryDb { conn }
    }

    fn make_row(command: &str, exit_status: i32) -> HistoryRow {
        HistoryRow {
            project: Some("pbfhogg".into()),
            cwd: "/home/folk/Programs/pbfhogg".into(),
            command: command.into(),
            elapsed_ms: 1234,
            exit_status,
            hostname: "testhost".into(),
            commit_hash: Some("abc123".into()),
            dirty: Some(false),
            kernel: Some("6.18.0".into()),
            avail_memory_mb: Some(16000),
        }
    }

    #[test]
    fn insert_and_query_basic() {
        let db = test_db();
        db.insert(&make_row("bench self --runs 3", 0)).unwrap();
        db.insert(&make_row("check", 1)).unwrap();

        let filter = HistoryFilter {
            command: None,
            project: None,
            failed: false,
            since: None,
            slow_ms: None,
            limit: 25,
            all: false,
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 2);
        // Most recent first.
        assert_eq!(entries[0].command, "check");
        assert_eq!(entries[1].command, "bench self --runs 3");
    }

    #[test]
    fn filter_by_command() {
        let db = test_db();
        db.insert(&make_row("bench self", 0)).unwrap();
        db.insert(&make_row("check", 0)).unwrap();
        db.insert(&make_row("bench read", 0)).unwrap();

        let filter = HistoryFilter {
            command: Some("bench".into()),
            project: None,
            failed: false,
            since: None,
            slow_ms: None,
            limit: 25,
            all: false,
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn filter_by_project() {
        let db = test_db();
        db.insert(&make_row("check", 0)).unwrap();

        let mut row = make_row("check", 0);
        row.project = Some("elivagar".into());
        db.insert(&row).unwrap();

        let filter = HistoryFilter {
            command: None,
            project: Some("elivagar".into()),
            failed: false,
            since: None,
            slow_ms: None,
            limit: 25,
            all: false,
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].project.as_deref(), Some("elivagar"));
    }

    #[test]
    fn filter_failed_only() {
        let db = test_db();
        db.insert(&make_row("check", 0)).unwrap();
        db.insert(&make_row("bench self", 1)).unwrap();

        let filter = HistoryFilter {
            command: None,
            project: None,
            failed: true,
            since: None,
            slow_ms: None,
            limit: 25,
            all: false,
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "bench self");
    }

    #[test]
    fn limit_respected() {
        let db = test_db();
        for i in 0..10 {
            db.insert(&make_row(&format!("cmd{i}"), 0)).unwrap();
        }

        let filter = HistoryFilter {
            command: None,
            project: None,
            failed: false,
            since: None,
            slow_ms: None,
            limit: 3,
            all: false,
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn all_ignores_limit() {
        let db = test_db();
        for i in 0..10 {
            db.insert(&make_row(&format!("cmd{i}"), 0)).unwrap();
        }

        let filter = HistoryFilter {
            command: None,
            project: None,
            failed: false,
            since: None,
            slow_ms: None,
            limit: 3,
            all: true,
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 10);
    }

    #[test]
    fn format_history_empty() {
        assert_eq!(format_history(&[]), "no history entries");
    }

    #[test]
    fn format_history_shows_fail_tag() {
        let db = test_db();
        db.insert(&make_row("check", 1)).unwrap();

        let filter = HistoryFilter {
            command: None,
            project: None,
            failed: false,
            since: None,
            slow_ms: None,
            limit: 25,
            all: false,
        };
        let entries = db.query(&filter).unwrap();
        let output = format_history(&entries);
        assert!(output.contains("[FAIL:1]"));
    }

    #[test]
    fn format_elapsed_units() {
        let db = test_db();

        let mut row = make_row("fast", 0);
        row.elapsed_ms = 50;
        db.insert(&row).unwrap();

        let mut row = make_row("medium", 0);
        row.elapsed_ms = 5_500;
        db.insert(&row).unwrap();

        let mut row = make_row("slow", 0);
        row.elapsed_ms = 120_000;
        db.insert(&row).unwrap();

        let filter = HistoryFilter {
            command: None,
            project: None,
            failed: false,
            since: None,
            slow_ms: None,
            limit: 25,
            all: false,
        };
        let entries = db.query(&filter).unwrap();
        let output = format_history(&entries);
        assert!(
            output.contains("2.0m"),
            "120s should show as minutes: {output}"
        );
        assert!(
            output.contains("5.5s"),
            "5500ms should show as seconds: {output}"
        );
        assert!(output.contains("50ms"), "50ms should show as ms: {output}");
    }

    #[test]
    fn no_project_row() {
        let db = test_db();
        let mut row = make_row("lock", 0);
        row.project = None;
        row.commit_hash = None;
        row.dirty = None;
        db.insert(&row).unwrap();

        let filter = HistoryFilter {
            command: None,
            project: None,
            failed: false,
            since: None,
            slow_ms: None,
            limit: 25,
            all: false,
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].project.is_none());
        let output = format_history(&entries);
        assert!(output.contains("-"), "no project should show as '-'");
    }

    #[test]
    fn db_path_uses_xdg_data_home() {
        // Save and restore XDG_DATA_HOME.
        let old = std::env::var("XDG_DATA_HOME").ok();
        // SAFETY: test is single-threaded and restores the original value.
        unsafe { std::env::set_var("XDG_DATA_HOME", "/tmp/test-xdg-data") };
        let path = db_path().unwrap();
        assert_eq!(path, PathBuf::from("/tmp/test-xdg-data/brokkr/history.db"));
        match old {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    #[test]
    fn fresh_db_schema_version() {
        let db = test_db();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
