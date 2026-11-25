//! Command history database.
//!
//! Global SQLite database at `$XDG_DATA_HOME/brokkr/history.db` that records
//! every brokkr invocation with timing, exit status, and sanitized system context.

use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::Connection;

use crate::db::like;
use crate::error::DevError;
use crate::git::ShortCommit;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA_VERSION: i64 = 3;

/// `elapsed_ms` is the whole invocation, lock queue included; `lock_wait_ms`
/// is the part of it spent queued for the global lock, NULL on rows written
/// before it was recorded.
const CREATE_TABLE: &str = "\
    CREATE TABLE IF NOT EXISTS history (
        id              INTEGER PRIMARY KEY,
        timestamp       TEXT NOT NULL DEFAULT (datetime('now')),
        project         TEXT,
        command         TEXT NOT NULL,
        elapsed_ms      INTEGER,
        exit_status     INTEGER NOT NULL,
        commit_hash     TEXT,
        dirty           INTEGER,
        kernel          TEXT,
        avail_memory_mb INTEGER,
        lock_wait_ms    INTEGER
    )";

const SELECT_COLUMNS: &str = "id, timestamp, project, command, elapsed_ms, exit_status, \
     commit_hash, dirty, kernel, avail_memory_mb, lock_wait_ms";

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

/// Why the history store could not be opened or written, kept typed so the
/// automatic recorder can tell an environment that simply has no writable
/// history store (a sandbox with a read-only data dir) from a real fault.
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Other(DevError),
}

impl StoreError {
    /// The store is unreachable from this environment, not broken: the data
    /// dir cannot be created or written, or the file opened read-only. Every
    /// brokkr invocation in such an environment would hit it, so the
    /// recorder stays quiet about it.
    pub fn is_environmental(&self) -> bool {
        use rusqlite::ErrorCode;
        match self {
            StoreError::Io(e) => matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            ),
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _)) => matches!(
                f.code,
                ErrorCode::CannotOpen | ErrorCode::ReadOnly | ErrorCode::PermissionDenied
            ),
            StoreError::Other(DevError::Io(e)) => matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            ),
            StoreError::Other(DevError::Database(message)) => {
                let message = message.to_ascii_lowercase();
                message.contains("unable to open database file")
                    || message.contains("readonly database")
                    || message.contains("permission denied")
            }
            _ => false,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl From<StoreError> for DevError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Io(e) => DevError::Io(e),
            StoreError::Sqlite(e) => e.into(),
            StoreError::Other(e) => e,
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "{e}"),
            StoreError::Sqlite(e) => write!(f, "{e}"),
            StoreError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl HistoryDb {
    /// Open the history database for queries. An older store is migrated
    /// before the read-only connection is returned; an absent store is an
    /// empty in-memory view and does not create a file.
    pub fn open() -> Result<Self, DevError> {
        let path = db_path()?;
        if !path.exists() {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(CREATE_TABLE)?;
            conn.execute_batch(CREATE_INDEXES)?;
            conn.pragma_update(None, "query_only", true)?;
            return Ok(Self { conn });
        }

        let conn = crate::db::store::open_read_only_migrating(
            &path,
            "command history",
            SCHEMA_VERSION,
            || {
                drop(Self::open_typed()?);
                Ok(())
            },
        )?;
        Ok(Self { conn })
    }

    /// [`Self::open`] with the failure kept typed (see [`StoreError`]).
    pub fn open_typed() -> Result<Self, StoreError> {
        let path = db_path().map_err(StoreError::Other)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = crate::db::store::open_read_write(&path, "command history", SCHEMA_VERSION)
            .map_err(StoreError::Other)?;
        run_migrations(&conn)?;
        conn.execute_batch(CREATE_TABLE)?;
        conn.execute_batch(CREATE_INDEXES)?;
        Ok(Self { conn })
    }

    /// Insert a history row.
    pub fn insert(&self, row: &HistoryRow) -> Result<(), StoreError> {
        let commit_hash = row.commit_hash.as_ref().map(ShortCommit::as_str);
        self.conn.execute(
            "INSERT INTO history (project, command, elapsed_ms, exit_status, \
                commit_hash, dirty, kernel, avail_memory_mb, lock_wait_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                row.project,
                row.command,
                row.elapsed_ms,
                row.exit_status,
                commit_hash,
                row.dirty,
                row.kernel,
                row.avail_memory_mb,
                row.lock_wait_ms,
            ],
        )?;
        Ok(())
    }

    /// The newest row: the latest command to complete, its end to the
    /// second (a row is written when its command ends).
    pub fn latest(&self) -> Result<Option<crate::bench::store::PreviousCommand>, DevError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT command, CAST(strftime('%s', timestamp) AS INTEGER), elapsed_ms
                 FROM history ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok(crate::bench::store::PreviousCommand {
                        command: row.get(0)?,
                        ended_unix_s: row.get(1)?,
                        elapsed_ms: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Look up a single entry by row id. Returns `Ok(None)` when the id
    /// doesn't exist.
    pub fn get_by_id(&self, id: i64) -> Result<Option<HistoryEntry>, DevError> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {SELECT_COLUMNS} FROM history WHERE id = ?1"))?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(entry_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Query history rows with optional filters.
    pub fn query(&self, filter: &HistoryFilter) -> Result<Vec<HistoryEntry>, DevError> {
        let mut clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref cmd) = filter.command {
            params.push(Box::new(like::contains_pattern(cmd)));
            clauses.push(format!("command LIKE ?{} {}", params.len(), like::ESCAPE));
        }
        if let Some(ref project) = filter.project {
            params.push(Box::new(project.clone()));
            clauses.push(format!("project = ?{}", params.len()));
        }
        if filter.failed {
            clauses.push("exit_status != 0".to_owned());
        }
        if let Some(status) = filter.status {
            params.push(Box::new(status));
            clauses.push(format!("exit_status = ?{}", params.len()));
        }
        if let Some(ref since) = filter.since {
            params.push(Box::new(since.clone()));
            clauses.push(format!("timestamp >= ?{}", params.len()));
        }
        if let Some(ref until) = filter.until {
            params.push(Box::new(until.clone()));
            // A bare date means "through the end of that day". Compared as a
            // string against 'YYYY-MM-DD HH:MM:SS', '2026-03-05' sorts before
            // every timestamp on the 5th, so `<=` would exclude the whole day;
            // compare against the start of the next day instead.
            if until.len() == 10 {
                clauses.push(format!("timestamp < datetime(?{}, '+1 day')", params.len()));
            } else {
                clauses.push(format!("timestamp <= ?{}", params.len()));
            }
        }
        if let Some(slow_ms) = filter.slow_ms {
            params.push(Box::new(slow_ms));
            // The command's own time, as the listing shows it: a run that
            // was only slow to get the lock is not a slow command.
            clauses.push(format!(
                "elapsed_ms - COALESCE(lock_wait_ms, 0) >= ?{}",
                params.len()
            ));
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
            "SELECT {SELECT_COLUMNS} FROM history{where_clause} ORDER BY id DESC{limit}"
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(AsRef::as_ref).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), entry_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }
}

/// One row selected with [`SELECT_COLUMNS`].
fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        timestamp: row.get(1)?,
        project: row.get(2)?,
        command: row.get(3)?,
        elapsed_ms: row.get(4)?,
        exit_status: row.get(5)?,
        commit_hash: row.get(6)?,
        dirty: row.get(7)?,
        kernel: row.get(8)?,
        avail_memory_mb: row.get(9)?,
        lock_wait_ms: row.get(10)?,
    })
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Row data for insertion.
pub struct HistoryRow {
    pub project: Option<String>,
    pub command: String,
    pub elapsed_ms: i64,
    pub exit_status: i32,
    pub commit_hash: Option<ShortCommit>,
    pub dirty: Option<bool>,
    pub kernel: Option<String>,
    pub avail_memory_mb: Option<i64>,
    /// The part of `elapsed_ms` spent queued for the global lock.
    pub lock_wait_ms: i64,
}

/// Row data returned from queries.
#[allow(dead_code)]
pub struct HistoryEntry {
    pub id: i64,
    pub timestamp: String,
    pub project: Option<String>,
    pub command: String,
    pub elapsed_ms: Option<i64>,
    pub exit_status: i32,
    pub commit_hash: Option<String>,
    pub dirty: Option<bool>,
    pub kernel: Option<String>,
    pub avail_memory_mb: Option<i64>,
    /// `None` on rows written before the wait was recorded.
    pub lock_wait_ms: Option<i64>,
}

impl HistoryEntry {
    /// The command's own time: the whole invocation less its lock queue.
    fn run_ms(&self) -> Option<i64> {
        self.elapsed_ms
            .map(|ms| ms.saturating_sub(self.lock_wait_ms.unwrap_or(0)).max(0))
    }

    /// The lock queue time, when there was any.
    fn wait_ms(&self) -> Option<i64> {
        self.lock_wait_ms.filter(|ms| *ms > 0)
    }
}

/// Filters for history queries.
#[derive(Default)]
pub struct HistoryFilter {
    pub command: Option<String>,
    pub project: Option<String>,
    pub failed: bool,
    pub since: Option<String>,
    pub until: Option<String>,
    pub status: Option<i32>,
    pub slow_ms: Option<i64>,
    pub limit: usize,
    pub all: bool,
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format a single history entry as a labelled detail block for
/// `brokkr history <id>`.
pub fn format_detail(entry: &HistoryEntry) -> String {
    let mut fields: Vec<(&str, String)> = Vec::new();
    fields.push(("id", entry.id.to_string()));
    fields.push(("timestamp", entry.timestamp.clone()));
    if let Some(ref p) = entry.project {
        fields.push(("project", p.clone()));
    }
    fields.push(("command", format_history_command(&entry.command)));
    if let Some(ms) = entry.run_ms() {
        fields.push(("elapsed", format!("{ms} ms")));
    }
    if let Some(ms) = entry.wait_ms() {
        fields.push(("lock wait", format!("{ms} ms")));
    }
    fields.push(("exit_status", entry.exit_status.to_string()));
    if let Some(ref c) = entry.commit_hash {
        let dirty = if entry.dirty == Some(true) {
            " (dirty)"
        } else {
            ""
        };
        fields.push(("commit", format!("{c}{dirty}")));
    }
    if let Some(ref k) = entry.kernel {
        fields.push(("kernel", k.clone()));
    }
    if let Some(mb) = entry.avail_memory_mb {
        fields.push(("memory", format!("{mb} MB")));
    }
    let width = fields.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (label, value) in &fields {
        out.push_str(&format!("{label:<width$}  {value}\n"));
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Format history entries for display.
pub fn format_history(entries: &[HistoryEntry]) -> String {
    if entries.is_empty() {
        return String::from("no history entries");
    }

    // The id leads each line: it is what `brokkr history <id>` takes, and
    // nothing else in the listing identifies a row.
    let id_width = entries
        .iter()
        .map(|e| e.id.to_string().len())
        .max()
        .unwrap_or(1);
    let mut lines = Vec::with_capacity(entries.len());
    for entry in entries {
        let project = entry.project.as_deref().unwrap_or("-");
        let elapsed = entry.run_ms().map_or_else(|| "-".to_owned(), short_duration);
        // The queue time rides after the command's own, only when there was
        // one: it says how busy the lock was, not how long the command took.
        let wait_tag = entry
            .wait_ms()
            .map_or_else(String::new, |ms| format!("  (+{} lock)", short_duration(ms)));

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
            "{id:>id_width$}  {ts}  {project:<10} {elapsed:>7}  {cmd}{commit_display}{fail_tag}{wait_tag}",
            id = entry.id,
            ts = entry.timestamp,
            cmd = format_history_command(&entry.command),
        ));
    }

    lines.join("\n")
}

#[allow(clippy::cast_precision_loss)] // Display rounding to one decimal.
fn short_duration(ms: i64) -> String {
    if ms >= 60_000 {
        format!("{:.1}m", ms as f64 / 60_000.0)
    } else if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

/// Render the structured argv used by new history rows. Older rows retain
/// their original joined text and pass through unchanged.
fn format_history_command(command: &str) -> String {
    serde_json::from_str::<Vec<String>>(command).map_or_else(
        |_| command.to_owned(),
        |argv| {
            argv.iter()
                .map(|arg| format!("{arg:?}"))
                .collect::<Vec<_>>()
                .join(" ")
        },
    )
}

/// Return argv safe for history storage. Credentials use the shared argv
/// policy; the history-only project directory option is also hidden because
/// it contains a local filesystem path.
pub(crate) fn redact_argv(argv: &[String]) -> Vec<String> {
    let mut redacted = redact_secret_argv(argv);
    let mut index = 0;
    while index < redacted.len() {
        if redacted[index] == "--project-dir" {
            if let Some(value) = redacted.get_mut(index + 1) {
                *value = "<redacted>".to_owned();
            }
            index += 2;
        } else {
            if redacted[index].starts_with("--project-dir=") {
                redacted[index] = "--project-dir=<redacted>".to_owned();
            }
            index += 1;
        }
    }
    redacted
}

/// Return argv with credentials removed and nothing else: the shared rule for
/// results rows, whose argv is the identity `results --compare` pairs by and
/// `--grep` selects on, and history rows. Environment values are redacted
/// only when their variable name is credential-like; project-directory values
/// survive. A positional such as a workload named `session_bench` cannot
/// swallow the flag after it.
pub(crate) fn redact_secret_argv(argv: &[String]) -> Vec<String> {
    let mut redacted = Vec::with_capacity(argv.len());
    let mut next_secret = false;
    let mut next_env = false;
    let mut next_header = false;
    for arg in argv {
        if std::mem::take(&mut next_secret) {
            redacted.push("<redacted>".to_owned());
            continue;
        }
        if std::mem::take(&mut next_env) {
            redacted.push(redact_assignment(arg));
            continue;
        }
        if std::mem::take(&mut next_header) {
            redacted.push(redact_header(arg));
            continue;
        }
        if arg == "--env" {
            redacted.push(arg.clone());
            next_env = true;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--env=") {
            redacted.push(format!("--env={}", redact_assignment(value)));
            continue;
        }
        if matches!(arg.as_str(), "-H" | "--header") {
            redacted.push(arg.clone());
            next_header = true;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--header=") {
            redacted.push(format!("--header={}", redact_header(value)));
            continue;
        }
        if let Some(value) = arg.strip_prefix("-H=") {
            redacted.push(format!("-H={}", redact_header(value)));
            continue;
        }
        if let Some(header) = arg.strip_prefix("-H").filter(|value| value.contains(':')) {
            redacted.push(format!("-H{}", redact_header(header)));
            continue;
        }
        if let Some((name, _)) = arg.split_once('=')
            && sensitive_name(name)
        {
            redacted.push(format!("{name}=<redacted>"));
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 && sensitive_name(arg) {
            redacted.push(arg.clone());
            next_secret = true;
            continue;
        }
        redacted.push(redact_url_credentials(arg));
    }
    redacted
}

fn redact_assignment(value: &str) -> String {
    match value.split_once('=') {
        Some((name, _)) if sensitive_name(name) => format!("{name}=<redacted>"),
        Some((name, value)) => format!("{name}={}", redact_env_value(name, value)),
        None => redact_url_credentials(value),
    }
}

fn redact_header(value: &str) -> String {
    match value.split_once(':') {
        Some((name, _)) if sensitive_name(name) => format!("{name}: <redacted>"),
        _ => redact_url_credentials(value),
    }
}

/// Match credential name components, not arbitrary substrings such as `auth`
/// inside `author`, `oauth`, or `authority`.
fn sensitive_name(name: &str) -> bool {
    let mut normalized = String::with_capacity(name.len());
    let chars: Vec<char> = name.trim().trim_start_matches('-').chars().collect();
    for (index, ch) in chars.iter().copied().enumerate() {
        if !ch.is_ascii_alphanumeric() {
            if !normalized.is_empty() && !normalized.ends_with('_') {
                normalized.push('_');
            }
            continue;
        }
        if ch.is_ascii_uppercase() && !normalized.is_empty() {
            let previous = chars[index - 1];
            let next_is_lower = chars.get(index + 1).is_some_and(char::is_ascii_lowercase);
            let uppercase_run = chars[..index]
                .iter()
                .rev()
                .take_while(|previous| previous.is_ascii_uppercase())
                .count();
            if previous.is_ascii_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_ascii_uppercase() && next_is_lower && uppercase_run > 1)
            {
                normalized.push('_');
            }
        }
        normalized.push(ch.to_ascii_lowercase());
    }
    let words: Vec<&str> = normalized
        .split('_')
        .filter(|word| !word.is_empty())
        .collect();
    [
        "password",
        "passwords",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "credential",
        "credentials",
        "authorization",
        "cookie",
        "cookies",
        "session",
        "dsn",
        "auth",
    ]
    .iter()
    .any(|part| words.contains(part))
        || words.windows(2).any(|pair| {
            matches!(pair, ["private", "key"])
                || matches!(pair, ["api", "key"])
                || matches!(pair, ["access", "key"])
                || matches!(pair, ["database", "url"])
        })
}

/// Redact likely credentials and the current user's home path from captured
/// text before it is printed or copied into the run log. This is a rendering
/// policy only: callers match against the original captured bytes.
///
/// The patterns are deliberately narrow, because this text is usually a
/// diagnostic the reader has to act on: compiler output is full of
/// `name: Type`, `crate::auth::login` and `src/session.rs:12:5`, none of which
/// may be touched. Only a value is ever replaced, never the rest of a line:
/// URL userinfo and credential query parameters, a long token after
/// `Bearer`/`Basic`, an `Authorization`/`Cookie` header line, the value of a
/// credential-named flag, and a credential-named field's value when assigned
/// with `=` or given as a quoted string after `:`.
pub(crate) fn redact_secret_text(text: &str) -> String {
    static URL_RE: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r#"(?i)[a-z][a-z0-9+.-]*://[^\s"'<>]+"#).ok());
    static BEARER_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?i)\b(?P<kind>bearer|basic)(?P<space>\s+)[A-Za-z0-9._~+/=-]{16,}").ok()
    });
    static FIELD_VALUE_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(?P<key>[a-z0-9][a-z0-9_.-]*)["']?(?:\s*=\s*(?P<value>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,;]+)|\s*:\s*(?P<qvalue>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'))"#,
        )
        .ok()
    });
    static HEADER_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^(?P<prefix>\s*)(?P<key>proxy-authorization|authorization|set-cookie|cookie)(?P<sep>\s*:\s*)\S[^\r\n]*",
        )
        .ok()
    });

    // These are fixed, maintainer-authored patterns. Keep compilation failure
    // fail-closed to avoid printing credentials; the captured-text unit test
    // exercises every pattern and checks that normal output is not withheld.
    let Some(url_re) = URL_RE.as_ref() else {
        return "[captured output withheld: output redaction unavailable]".to_owned();
    };
    let Some(bearer_re) = BEARER_RE.as_ref() else {
        return "[captured output withheld: output redaction unavailable]".to_owned();
    };
    let Some(field_value_re) = FIELD_VALUE_RE.as_ref() else {
        return "[captured output withheld: output redaction unavailable]".to_owned();
    };
    let Some(header_re) = HEADER_RE.as_ref() else {
        return "[captured output withheld: output redaction unavailable]".to_owned();
    };

    let mut rendered = text.to_owned();
    if let Some(home) = std::env::var_os("HOME")
        .and_then(|home| home.into_string().ok())
        .filter(|home| !home.is_empty() && std::path::Path::new(home).is_absolute())
    {
        rendered = rendered.replace(&home, "~");
    }

    rendered
        .split_inclusive('\n')
        .map(|line| {
            let urls_redacted = url_re
                .replace_all(line, |captures: &regex::Captures<'_>| {
                    redact_url_credentials(&captures[0])
                })
                .into_owned();
            let bearer_redacted = bearer_re
                .replace_all(&urls_redacted, |captures: &regex::Captures<'_>| {
                    format!("{}{}<redacted>", &captures["kind"], &captures["space"])
                })
                .into_owned();
            let headers_redacted = header_re
                .replace_all(&bearer_redacted, |captures: &regex::Captures<'_>| {
                    format!(
                        "{}{}{}<redacted>",
                        &captures["prefix"], &captures["key"], &captures["sep"]
                    )
                })
                .into_owned();
            let flags_redacted = redact_secret_flags(&headers_redacted);
            redact_secret_fields(&flags_redacted, field_value_re)
        })
        .collect()
}

fn redact_secret_flags(line: &str) -> String {
    let spans = shell_token_spans(line);
    let mut rendered = String::with_capacity(line.len());
    let mut copied_until = 0;
    let mut index = 0;
    while index + 1 < spans.len() {
        let (flag_start, flag_end) = spans[index];
        let flag = &line[flag_start..flag_end];
        let flag_name = flag
            .trim_matches(|ch: char| matches!(ch, '[' | ']' | '(' | ')' | '\'' | '"' | ',' | ':'));
        if flag_name.starts_with('-') && !flag_name.contains('=') && sensitive_name(flag_name) {
            let (value_start, value_end) = spans[index + 1];
            rendered.push_str(&line[copied_until..value_start]);
            let value = &line[value_start..value_end];
            if let Some(quote) = value.chars().next().filter(|ch| *ch == '"' || *ch == '\'') {
                rendered.push(quote);
                rendered.push_str("<redacted>");
                rendered.push(quote);
            } else {
                rendered.push_str("<redacted>");
            }
            copied_until = value_end;
            index += 2;
        } else {
            index += 1;
        }
    }

    if copied_until == 0 {
        line.to_owned()
    } else {
        rendered.push_str(&line[copied_until..]);
        rendered
    }
}

fn shell_token_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut token_start = None;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if token_start.is_none() {
            if ch.is_whitespace() {
                continue;
            }
            token_start = Some(index);
        }

        if let Some(active_quote) = quote {
            if active_quote == '\'' {
                if ch == active_quote {
                    quote = None;
                }
                continue;
            }
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }

        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch.is_whitespace()
            && let Some(start) = token_start.take()
        {
            spans.push((start, index));
        }
    }
    if let Some(start) = token_start {
        spans.push((start, line.len()));
    }
    spans
}

fn redact_secret_fields(line: &str, field_re: &Regex) -> String {
    let mut rendered = String::with_capacity(line.len());
    let mut copied_until = 0;
    for captures in field_re.captures_iter(line) {
        let Some(key) = captures.name("key") else {
            continue;
        };
        if !sensitive_name(key.as_str()) {
            continue;
        }
        let Some(value) = captures.name("value").or_else(|| captures.name("qvalue")) else {
            continue;
        };
        rendered.push_str(&line[copied_until..value.start()]);
        let value_text = value.as_str();
        if let Some(quote) = value_text.chars().next().filter(|ch| {
            (*ch == '"' || *ch == '\'') && value_text.len() > 1 && value_text.ends_with(*ch)
        }) {
            rendered.push(quote);
            rendered.push_str("<redacted>");
            rendered.push(quote);
        } else {
            rendered.push_str("<redacted>");
        }
        copied_until = value.end();
    }

    if copied_until == 0 {
        line.to_owned()
    } else {
        rendered.push_str(&line[copied_until..]);
        rendered
    }
}

/// Scrub one captured environment value before it reaches a durable run row.
/// The variable name remains useful for interpreting the row; recognized
/// credential names retain only that name and a redaction marker.
pub(crate) fn redact_env_value(name: &str, value: &str) -> String {
    if sensitive_name(name) {
        "<redacted>".to_owned()
    } else {
        redact_url_credentials(value)
    }
}

fn redact_url_credentials(value: &str) -> String {
    let Some((scheme, rest)) = value.split_once("://") else {
        return value.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let authority = authority.rfind('@').map_or_else(
        || authority.to_owned(),
        |at| format!("<redacted>@{}", &authority[at + 1..]),
    );
    let tail = redact_url_parameters(&rest[authority_end..]);
    format!("{scheme}://{authority}{tail}")
}

fn redact_url_parameters(tail: &str) -> String {
    let mut rendered = String::with_capacity(tail.len());
    let mut remaining = tail;
    while let Some(delimiter) = remaining.find(['?', '#']) {
        rendered.push_str(&remaining[..=delimiter]);
        remaining = &remaining[delimiter + 1..];
        let end = remaining.find(['?', '#']).unwrap_or(remaining.len());
        rendered.push_str(
            &remaining[..end]
                .split('&')
                .map(|field| match field.split_once('=') {
                    Some((name, _)) if sensitive_name(name) => format!("{name}=<redacted>"),
                    _ => field.to_owned(),
                })
                .collect::<Vec<_>>()
                .join("&"),
        );
        remaining = &remaining[end..];
    }
    rendered.push_str(remaining);
    rendered
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Return the path to the history database.
///
/// Uses `$XDG_DATA_HOME/brokkr/history.db`, falling back to
/// `$HOME/.local/share/brokkr/history.db` (an empty or relative
/// `XDG_DATA_HOME` counts as unset - see `user_dirs`).
fn db_path() -> Result<PathBuf, DevError> {
    crate::user_dirs::history_db().ok_or_else(|| {
        DevError::Config(
            "cannot determine data directory: neither XDG_DATA_HOME nor HOME is set".into(),
        )
    })
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// Bring the store to [`SCHEMA_VERSION`]. A fresh file gets its table and
/// version stamp; an older one its pending steps. Either way under one
/// `IMMEDIATE` transaction with the version re-read inside it: every brokkr
/// invocation opens this file, so two of them routinely race to initialise
/// or migrate it, and the loser must see the winner's work rather than
/// repeat it. A store stamped newer than this binary is refused by the
/// shared store policy instead of being stamped down.
fn run_migrations(conn: &Connection) -> Result<(), StoreError> {
    crate::db::store::migrate(conn, "command history", SCHEMA_VERSION, |conn, current| {
        let has_history = crate::db::store::table_exists(conn, "history")?;
        if current < 2 && has_history {
            // Version 1 stored absolute working directories and machine
            // hostnames. Rebuild the table so those values leave the store.
            conn.execute_batch("ALTER TABLE history RENAME TO history_v1;")?;
        }
        conn.execute_batch(CREATE_TABLE)?;
        if current < 2 && has_history {
            conn.execute_batch(
                "INSERT INTO history \
                 (id, timestamp, project, command, elapsed_ms, exit_status, \
                  commit_hash, dirty, kernel, avail_memory_mb) \
                 SELECT id, timestamp, project, command, elapsed_ms, exit_status, \
                        commit_hash, dirty, kernel, avail_memory_mb \
                 FROM history_v1; \
                 DROP TABLE history_v1;",
            )?;
            redact_project_dir_commands(conn)?;
        }
        // Version 3 adds the lock queue time. A version 1 store was just
        // rebuilt with it; a version 2 one gains it, NULL on every old row.
        if current == 2 && has_history {
            conn.execute_batch("ALTER TABLE history ADD COLUMN lock_wait_ms INTEGER;")?;
        }
        conn.execute_batch(CREATE_INDEXES)?;
        Ok(())
    })
    .map_err(StoreError::Other)
}

fn redact_project_dir_commands(conn: &Connection) -> Result<(), DevError> {
    let commands = {
        let mut statement =
            conn.prepare("SELECT id, command FROM history WHERE command LIKE '%--project-dir%'")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };

    for (id, command) in commands {
        let command = match serde_json::from_str::<Vec<String>>(&command) {
            Ok(argv) => serde_json::to_string(&redact_argv(&argv)).map_err(|error| {
                DevError::Database(format!("cannot redact stored history argv: {error}"))
            })?,
            Err(_) => "history --project-dir=<redacted>".to_owned(),
        };
        conn.execute(
            "UPDATE history SET command = ?1 WHERE id = ?2",
            rusqlite::params![command, id],
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    #[test]
    fn captured_text_redaction_hides_values_and_spares_locations() {
        let out = redact_secret_text("api_token=hunter2hunter2 failed at src/session.rs:12:5\n");
        assert!(!out.contains("hunter2hunter2"), "{out}");
        assert!(out.contains("api_token=<redacted>"), "{out}");
        assert!(out.contains("src/session.rs:12:5"), "{out}");
        assert!(!out.contains("output redaction unavailable"), "{out}");
    }

    #[test]
    fn an_unwritable_store_is_environmental_and_a_fault_is_not() {
        let io = |k| StoreError::Io(std::io::Error::from(k));
        assert!(io(std::io::ErrorKind::PermissionDenied).is_environmental());
        assert!(io(std::io::ErrorKind::ReadOnlyFilesystem).is_environmental());
        assert!(!io(std::io::ErrorKind::NotFound).is_environmental());
        let sqlite = |code| {
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
        };
        assert!(sqlite(rusqlite::ffi::SQLITE_READONLY).is_environmental());
        assert!(sqlite(rusqlite::ffi::SQLITE_CANTOPEN).is_environmental());
        assert!(!sqlite(rusqlite::ffi::SQLITE_CORRUPT).is_environmental());
    }

    #[test]
    fn migration_initialises_once_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        assert!(crate::db::store::table_exists(&conn, "history").unwrap());
        run_migrations(&conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

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
            command: command.into(),
            elapsed_ms: 1234,
            exit_status,
            commit_hash: Some("abc123".parse().unwrap()),
            dirty: Some(false),
            kernel: Some("6.18.0".into()),
            avail_memory_mb: Some(16000),
            lock_wait_ms: 0,
        }
    }

    #[test]
    fn listing_shows_own_time_and_tags_the_lock_wait() {
        let db = test_db();
        let mut row = make_row("fstrim", 0);
        row.elapsed_ms = 132_000;
        row.lock_wait_ms = 128_000;
        db.insert(&row).unwrap();
        db.insert(&make_row("check", 0)).unwrap();

        let entries = db
            .query(&HistoryFilter {
                limit: 25,
                ..Default::default()
            })
            .unwrap();
        let output = format_history(&entries);
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines[1].contains("4.0s"), "own time: {output}");
        assert!(lines[1].ends_with("(+2.1m lock)"), "wait tag: {output}");
        assert!(!lines[0].contains("lock)"), "no wait, no tag: {output}");

        let detail = format_detail(&entries[1]);
        assert!(detail.contains("4000 ms"), "{detail}");
        assert!(
            detail
                .lines()
                .any(|l| l.starts_with("lock wait") && l.ends_with(" 128000 ms")),
            "{detail}"
        );
    }

    #[test]
    fn slow_filter_reads_own_time() {
        let db = test_db();
        let mut row = make_row("queued", 0);
        row.elapsed_ms = 120_000;
        row.lock_wait_ms = 119_000;
        db.insert(&row).unwrap();
        let mut row = make_row("slow", 0);
        row.elapsed_ms = 90_000;
        db.insert(&row).unwrap();

        let got: Vec<String> = db
            .query(&HistoryFilter {
                slow_ms: Some(60_000),
                limit: 25,
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|e| e.command)
            .collect();
        assert_eq!(got, vec!["slow"]);
    }

    #[test]
    fn a_version_2_store_gains_the_wait_column_null_on_old_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE history (
                id INTEGER PRIMARY KEY,
                timestamp TEXT NOT NULL DEFAULT (datetime('now')),
                project TEXT, command TEXT NOT NULL, elapsed_ms INTEGER,
                exit_status INTEGER NOT NULL, commit_hash TEXT, dirty INTEGER,
                kernel TEXT, avail_memory_mb INTEGER);
             INSERT INTO history (command, elapsed_ms, exit_status)
                VALUES ('check', 5000, 0);
             PRAGMA user_version = 2;",
        )
        .unwrap();
        run_migrations(&conn).unwrap();
        let db = HistoryDb { conn };
        let entry = db.get_by_id(1).unwrap().unwrap();
        assert_eq!(entry.lock_wait_ms, None);
        assert_eq!(entry.run_ms(), Some(5000));
        db.insert(&make_row("new", 0)).unwrap();
        assert_eq!(db.get_by_id(2).unwrap().unwrap().lock_wait_ms, Some(0));
    }

    #[test]
    fn insert_and_query_basic() {
        let db = test_db();
        db.insert(&make_row("bench self --runs 3", 0)).unwrap();
        db.insert(&make_row("check", 1)).unwrap();

        let filter = HistoryFilter {
            limit: 25,
            ..Default::default()
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
            limit: 25,
            ..Default::default()
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
            project: Some("elivagar".into()),
            limit: 25,
            ..Default::default()
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
            failed: true,
            limit: 25,
            ..Default::default()
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
            limit: 3,
            ..Default::default()
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
            limit: 3,
            all: true,
            ..Default::default()
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 10);
    }

    fn insert_at(db: &HistoryDb, command: &str, timestamp: &str) {
        db.insert(&make_row(command, 0)).unwrap();
        db.conn
            .execute(
                "UPDATE history SET timestamp = ?1 WHERE id = last_insert_rowid()",
                [timestamp],
            )
            .unwrap();
    }

    #[test]
    fn until_date_includes_that_whole_day() {
        let db = test_db();
        insert_at(&db, "before", "2026-03-04 12:00:00");
        insert_at(&db, "on", "2026-03-05 23:59:59");
        insert_at(&db, "after", "2026-03-06 00:00:00");

        let filter = HistoryFilter {
            until: Some("2026-03-05".into()),
            limit: 25,
            ..Default::default()
        };
        let mut got: Vec<String> = db
            .query(&filter)
            .unwrap()
            .into_iter()
            .map(|e| e.command)
            .collect();
        got.sort();
        assert_eq!(got, vec!["before", "on"]);

        let filter = HistoryFilter {
            until: Some("2026-03-05 12:00:00".into()),
            limit: 25,
            ..Default::default()
        };
        let got: Vec<String> = db
            .query(&filter)
            .unwrap()
            .into_iter()
            .map(|e| e.command)
            .collect();
        assert_eq!(got, vec!["before"]);
    }

    #[test]
    fn format_history_shows_id() {
        let db = test_db();
        db.insert(&make_row("check", 0)).unwrap();
        let entries = db
            .query(&HistoryFilter {
                limit: 25,
                ..Default::default()
            })
            .unwrap();
        let output = format_history(&entries);
        assert!(
            output.trim_start().starts_with(&entries[0].id.to_string()),
            "id must lead the line: {output}"
        );
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
            limit: 25,
            ..Default::default()
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
            limit: 25,
            ..Default::default()
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
            limit: 25,
            ..Default::default()
        };
        let entries = db.query(&filter).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].project.is_none());
        let output = format_history(&entries);
        assert!(output.contains("-"), "no project should show as '-'");
    }

    #[test]
    fn db_path_uses_the_resolved_data_home_without_mutating_process_env() {
        match crate::user_dirs::xdg_data_home() {
            Some(data_home) => assert_eq!(
                db_path().unwrap(),
                data_home.join("brokkr").join("history.db")
            ),
            None => assert!(db_path().is_err()),
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
