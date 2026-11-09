//! The frozen migration chain and the open policy.
//!
//! [`MIGRATIONS`] owns the schema: step N takes `user_version` N to N + 1,
//! and a step once shipped never changes, since every store and the results
//! server replay the same chain. A new column or table is a new step. The
//! golden test in `tests.rs` fails when a shipped step's product changes.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::StoreError;

/// Version 1: the evidence model. Sample columns are spelled out here, not
/// generated from [`super::Sample`], so a later sample field is a new step.
const V1: &str = "
CREATE TABLE configs (
    digest TEXT PRIMARY KEY NOT NULL,
    text   TEXT NOT NULL
) STRICT;

CREATE TABLE runs (
    id                   INTEGER PRIMARY KEY,
    uuid                 TEXT NOT NULL UNIQUE,
    format               INTEGER NOT NULL,
    project              TEXT NOT NULL,
    bench                TEXT NOT NULL,
    checkout             TEXT NOT NULL,
    hostname             TEXT NOT NULL,
    git_commit           TEXT NOT NULL,
    dirty                INTEGER NOT NULL CHECK (dirty IN (0, 1)),
    subject              TEXT,
    started_unix_ms      INTEGER NOT NULL,
    brokkr_version       TEXT NOT NULL,
    argv                 TEXT NOT NULL,
    kernel               TEXT,
    cpu_governor         TEXT,
    avail_memory_mb      INTEGER,
    captured_env         TEXT NOT NULL,
    target               TEXT,
    profile              TEXT NOT NULL,
    features             TEXT NOT NULL,
    default_features     INTEGER NOT NULL CHECK (default_features IN (0, 1)),
    resolved_bin         TEXT,
    resolved_features    TEXT,
    artifact             TEXT,
    config_digest        TEXT NOT NULL REFERENCES configs(digest),
    iterations_requested INTEGER NOT NULL CHECK (iterations_requested > 0),
    outcome              TEXT NOT NULL CHECK (outcome IN ('completed', 'failed', 'interrupted')),
    failure              TEXT,
    best_iteration       INTEGER,
    pairing_key          TEXT NOT NULL,
    CHECK ((outcome = 'completed') = (best_iteration IS NOT NULL)),
    CHECK ((outcome = 'failed') = (failure IS NOT NULL)),
    CHECK ((resolved_bin IS NULL) = (resolved_features IS NULL))
) STRICT;
CREATE INDEX runs_by_pairing ON runs(project, pairing_key);
CREATE INDEX runs_by_bench ON runs(project, bench, started_unix_ms);

CREATE TABLE run_params (
    run_id    INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    name      TEXT NOT NULL,
    many      INTEGER NOT NULL CHECK (many IN (0, 1)),
    from_path TEXT,
    PRIMARY KEY (run_id, name)
) STRICT;

CREATE TABLE run_param_values (
    run_id   INTEGER NOT NULL,
    name     TEXT NOT NULL,
    position INTEGER NOT NULL,
    key      TEXT NOT NULL,
    digest   TEXT,
    PRIMARY KEY (run_id, name, position),
    FOREIGN KEY (run_id, name) REFERENCES run_params(run_id, name) ON DELETE CASCADE
) STRICT;

CREATE TABLE iterations (
    run_id  INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    idx     INTEGER NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('completed', 'failed', 'interrupted')),
    verdict TEXT,
    failure TEXT,
    wall_us INTEGER,
    record  TEXT,
    PRIMARY KEY (run_id, idx),
    CHECK (outcome = 'completed' OR verdict IS NULL),
    CHECK ((outcome = 'failed') = (failure IS NOT NULL))
) STRICT;

CREATE TABLE children (
    id                   INTEGER PRIMARY KEY,
    run_id               INTEGER NOT NULL,
    iteration            INTEGER NOT NULL,
    idx                  INTEGER NOT NULL,
    program              TEXT NOT NULL,
    argv                 TEXT NOT NULL,
    env                  TEXT NOT NULL,
    pid                  INTEGER,
    origin_offset_us     INTEGER NOT NULL,
    wall_us              INTEGER NOT NULL,
    ending_kind          TEXT NOT NULL CHECK (ending_kind IN
        ('exited', 'signal', 'stop_marker', 'deadline', 'interrupted', 'wait_failed')),
    ending_value         INTEGER,
    ending_text          TEXT,
    rejected_lines       INTEGER NOT NULL,
    fifo_unread          INTEGER NOT NULL CHECK (fifo_unread IN (0, 1)),
    records_capped       INTEGER NOT NULL CHECK (records_capped IN (0, 1)),
    samples_capped       INTEGER NOT NULL CHECK (samples_capped IN (0, 1)),
    output_truncated     INTEGER NOT NULL CHECK (output_truncated IN (0, 1)),
    clock_outliers       INTEGER NOT NULL,
    clock_outlier_min_us INTEGER,
    calls_seen           TEXT NOT NULL,
    calls_overflowed     INTEGER NOT NULL CHECK (calls_overflowed IN (0, 1)),
    UNIQUE (run_id, iteration, idx),
    FOREIGN KEY (run_id, iteration) REFERENCES iterations(run_id, idx) ON DELETE CASCADE
) STRICT;

CREATE TABLE samples (
    child_id              INTEGER NOT NULL REFERENCES children(id) ON DELETE CASCADE,
    sample_idx            INTEGER NOT NULL,
    timestamp_us          INTEGER NOT NULL,
    rss_kb                INTEGER NOT NULL,
    anon_kb               INTEGER NOT NULL,
    file_kb               INTEGER NOT NULL,
    shmem_kb              INTEGER NOT NULL,
    swap_kb               INTEGER NOT NULL,
    vsize_kb              INTEGER NOT NULL,
    vm_hwm_kb             INTEGER NOT NULL,
    utime                 INTEGER NOT NULL,
    stime                 INTEGER NOT NULL,
    num_threads           INTEGER NOT NULL,
    minflt                INTEGER NOT NULL,
    majflt                INTEGER NOT NULL,
    rchar                 INTEGER NOT NULL,
    wchar                 INTEGER NOT NULL,
    read_bytes            INTEGER NOT NULL,
    write_bytes           INTEGER NOT NULL,
    cancelled_write_bytes INTEGER NOT NULL,
    syscr                 INTEGER NOT NULL,
    syscw                 INTEGER NOT NULL,
    vol_cs                INTEGER NOT NULL,
    nonvol_cs             INTEGER NOT NULL,
    PRIMARY KEY (child_id, sample_idx)
) STRICT;

CREATE TABLE markers (
    child_id     INTEGER NOT NULL REFERENCES children(id) ON DELETE CASCADE,
    record_idx   INTEGER NOT NULL,
    marker_idx   INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,
    call_id      TEXT,
    name         TEXT NOT NULL,
    PRIMARY KEY (child_id, record_idx),
    UNIQUE (child_id, marker_idx)
) STRICT;

CREATE TABLE counters (
    child_id     INTEGER NOT NULL REFERENCES children(id) ON DELETE CASCADE,
    record_idx   INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,
    call_id      TEXT,
    name         TEXT NOT NULL,
    value        INTEGER NOT NULL,
    PRIMARY KEY (child_id, record_idx)
) STRICT;

CREATE TABLE sink_records (
    child_id     INTEGER NOT NULL REFERENCES children(id) ON DELETE CASCADE,
    record_idx   INTEGER NOT NULL,
    timestamp_us INTEGER NOT NULL,
    call_id      TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('sealing', 'terminal')),
    name         TEXT,
    value        INTEGER,
    end_state    TEXT,
    drops        TEXT,
    PRIMARY KEY (child_id, record_idx),
    CHECK ((kind = 'sealing') = (name IS NOT NULL AND value IS NOT NULL)),
    CHECK ((kind = 'terminal') = (end_state IS NOT NULL AND drops IS NOT NULL))
) STRICT;
";

/// Version 2: hotpath capture. A run's measurement, every earlier run a
/// timing run, and each marked child's capture beside it.
const V2: &str = "
ALTER TABLE runs ADD COLUMN measurement TEXT NOT NULL DEFAULT 'timing'
    CHECK (measurement IN ('timing', 'hotpath'));

CREATE TABLE child_hotpath (
    child_id   INTEGER PRIMARY KEY REFERENCES children(id) ON DELETE CASCADE,
    stage      TEXT NOT NULL,
    occurrence INTEGER NOT NULL CHECK (occurrence > 0),
    status     TEXT NOT NULL CHECK (status IN
        ('captured', 'missing', 'invalid', 'unreadable', 'over_budget')),
    reason     TEXT,
    report     TEXT,
    CHECK ((status = 'captured') = (report IS NOT NULL)),
    CHECK ((status IN ('invalid', 'unreadable')) = (reason IS NOT NULL))
) STRICT;
";

/// Version 3: external executables. A `program` call's child, with the
/// absolute path it ran and that file's digest as read at launch, or why it
/// could not be read. A child without a row made no separate observation.
const V3: &str = "
CREATE TABLE child_executable (
    child_id     INTEGER PRIMARY KEY REFERENCES children(id) ON DELETE CASCADE,
    path         TEXT NOT NULL CHECK (substr(path, 1, 1) = '/'),
    digest       TEXT,
    digest_error TEXT,
    CHECK ((digest IS NOT NULL) <> (digest_error IS NOT NULL)),
    CHECK (digest_error IS NULL OR digest_error <> '')
) STRICT;
";

/// Version 4: marks. One append-only log of mark requests per store, each
/// row the exact request as submitted; what they mean is a fold over a
/// project's rows in id order (`marks.rs`). No run row is touched.
const V4: &str = "
CREATE TABLE marks (
    id         INTEGER PRIMARY KEY,
    uuid       TEXT NOT NULL UNIQUE,
    project    TEXT NOT NULL,
    at_unix_ms INTEGER NOT NULL,
    hostname   TEXT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN
        ('retire', 'unretire', 'invalidate', 'revalidate', 'rename')),
    name       TEXT,
    target     TEXT,
    run        TEXT REFERENCES runs(uuid),
    reason     TEXT,
    CHECK ((kind IN ('retire', 'unretire', 'rename')) = (name IS NOT NULL)),
    CHECK ((kind = 'rename') = (target IS NOT NULL)),
    CHECK ((kind IN ('invalidate', 'revalidate')) = (run IS NOT NULL)),
    CHECK ((kind IN ('retire', 'invalidate')) = (reason IS NOT NULL))
) STRICT;
CREATE INDEX marks_by_project ON marks(project, id);
";

/// Version 5: alloc profiling. V2's CHECK on `measurement` cannot be widened
/// in place, and rebuilding `runs` would cascade through every table that
/// references it, so an alloc run is stored as `measurement = 'hotpath'`
/// with `alloc` set; every earlier run is not alloc.
const V5: &str = "
ALTER TABLE runs ADD COLUMN alloc INTEGER NOT NULL DEFAULT 0
    CHECK (alloc IN (0, 1) AND (alloc = 0 OR measurement = 'hotpath'));
";

/// Version 6: stderr tails. Each child brokkr spooled stderr for, with
/// whether its call's borrow was released before the snapshot, the size
/// observed and what was kept of the end. A child without a row was not
/// observed.
const V6: &str = "
CREATE TABLE child_stderr (
    child_id INTEGER PRIMARY KEY REFERENCES children(id) ON DELETE CASCADE,
    released INTEGER NOT NULL CHECK (released IN (0, 1)),
    bytes    INTEGER CHECK (bytes IS NULL OR bytes >= 0),
    status   TEXT NOT NULL CHECK (status IN ('kept', 'over_budget', 'unreadable')),
    text     TEXT,
    raw      INTEGER CHECK (raw IS NULL OR raw >= 0),
    cut      INTEGER CHECK (cut IS NULL OR cut IN (0, 1)),
    reason   TEXT,
    CHECK ((status = 'kept') = (text IS NOT NULL AND raw IS NOT NULL AND cut IS NOT NULL)),
    CHECK ((status = 'kept') OR (text IS NULL AND raw IS NULL AND cut IS NULL)),
    CHECK ((status = 'unreadable') = (reason IS NOT NULL))
) STRICT;
";

/// The chain. Shipped steps are frozen; append only.
pub(super) const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6];

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The schema version this code writes.
pub(super) fn schema_version() -> i64 {
    i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX)
}

/// Open the store at `path`, creating the file (not its directory), and bring
/// it to [`schema_version`]. Refused: a store newer than this code or stamped
/// negative, a file
/// with tables but no version stamp (not this store's), and a filesystem
/// where WAL does not take.
pub(super) fn open(path: &Path) -> Result<Connection, StoreError> {
    open_writable(path, true)
}

/// Open an existing store for writing, never creating its file: `None` only
/// when opening failed and the file is confirmed missing. A failure after
/// the file opened (a newer stamp, a migration) is that failure, whatever
/// became of the file since.
pub(super) fn open_existing(path: &Path) -> Result<Option<Connection>, StoreError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Err(error) = Connection::open_with_flags(path, flags) {
        return match std::fs::metadata(path) {
            Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(other) => Err(StoreError::Failed(format!(
                "{} cannot be read: {other}",
                path.display()
            ))),
            Ok(_) => Err(error.into()),
        };
    }
    open_writable(path, false).map(Some)
}

/// [`open`], creating the file only when `create` is set: a reader's
/// migration opens without it, so a file removed since the reader found it
/// is an error rather than a fresh store.
fn open_writable(path: &Path, create: bool) -> Result<Connection, StoreError> {
    let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    refuse_newer(&conn)?;
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Refused(format!(
            "{} cannot use WAL (journal mode stayed {mode}); the store must be on a local filesystem",
            path.display()
        )));
    }
    migrate(&conn)?;
    Ok(conn)
}

/// Open an existing store for reading, never creating its file: `None` when
/// there is no file at `path`. A store on an older schema is migrated first,
/// through a writable connection that may not create the file, then reopened
/// read-only; a newer or negative stamp and a file with tables but no stamp
/// are refused, as [`open`] refuses them. SQLite may still make the `-wal`
/// and `-shm` files a WAL read needs.
pub(super) fn open_read_only(path: &Path) -> Result<Option<Connection>, StoreError> {
    let read_only = || -> Result<Option<Connection>, StoreError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        match Connection::open_with_flags(path, flags) {
            Ok(conn) => {
                conn.busy_timeout(BUSY_TIMEOUT)?;
                conn.pragma_update(None, "query_only", true)?;
                Ok(Some(conn))
            }
            // Absent only when the file is confirmed missing: a path the
            // user cannot search is an error, not an empty history.
            Err(error) => match std::fs::metadata(path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(other) => Err(StoreError::Failed(format!(
                    "{} cannot be read: {other}",
                    path.display()
                ))),
                Ok(_) => Err(error.into()),
            },
        }
    };
    let Some(conn) = read_only()? else {
        return Ok(None);
    };
    if refuse_newer(&conn)? == schema_version() {
        return Ok(Some(conn));
    }
    drop(conn);
    drop(open_writable(path, false)?);
    read_only()?
        .ok_or_else(|| StoreError::Failed(format!("{} vanished after migration", path.display())))
        .map(Some)
}

fn user_version(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

fn refuse_newer(conn: &Connection) -> Result<i64, StoreError> {
    let current = user_version(conn)?;
    let known = schema_version();
    if current > known {
        return Err(StoreError::Refused(format!(
            "schema version {current} is newer than this brokkr knows ({known}); upgrade brokkr"
        )));
    }
    if current < 0 {
        return Err(StoreError::Refused(format!(
            "schema version {current} is no version of the bench store"
        )));
    }
    Ok(current)
}

/// Apply the pending steps under one `BEGIN IMMEDIATE`, the stamp re-read
/// inside it so two first writers cannot both plan from a stale one.
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    if refuse_newer(conn)? == schema_version() {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let applied = (|| {
        let current = refuse_newer(conn)?;
        if current == 0 {
            let tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if tables > 0 {
                return Err(StoreError::Refused(
                    "the file holds tables but no schema version; it is not a bench store"
                        .to_owned(),
                ));
            }
        }
        let start = usize::try_from(current).unwrap_or(usize::MAX);
        for step in MIGRATIONS.iter().skip(start) {
            conn.execute_batch(step)?;
        }
        conn.pragma_update(None, "user_version", schema_version())?;
        Ok(())
    })();
    if let Err(error) = applied {
        // The original error is the one worth reporting.
        conn.execute_batch("ROLLBACK").ok();
        return Err(error);
    }
    if let Err(error) = conn.execute_batch("COMMIT") {
        conn.execute_batch("ROLLBACK").ok();
        return Err(error.into());
    }
    Ok(())
}
