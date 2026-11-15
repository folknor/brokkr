//! The lock queue: one record per brokkr waiting for the global lock, so
//! `brokkr kill` can find and cancel a queued command and `brokkr lock` can
//! list the queue.
//!
//! # The record and its liveness proof
//!
//! A waiter joins on its first failed flock attempt, before it prints the
//! wait line, so every waiter that has announced itself is recorded. It
//! creates `<token>.pending`, takes an exclusive flock on it, writes its
//! record and renames it to `<token>.queued`. The flock is the liveness proof,
//! as in `crate::test_orphans`: the kernel drops it however the waiter dies,
//! and it answers the same from every PID namespace that shares the
//! filesystem. A `.queued` or `.cancel` file whose flock can be taken belongs
//! to a dead waiter and is swept; a `.pending` one only once it is also past
//! [`PENDING_GRACE`]. A join that loses its own pending file to such a sweep
//! starts again under a fresh token. A join that cannot complete refuses the
//! acquisition: a waiter never waits unrecorded.
//!
//! # Cancelling
//!
//! Both sides act on the one name `<token>.queued`, and `rename` and `unlink`
//! of one name are atomic against each other, so exactly one side wins:
//!
//! - `brokkr kill` renames it to `<token>.cancel`. Success commits the cancel.
//! - The waiter, each poll round, looks for its `.queued` name; gone means
//!   cancelled, and it ends its wait as interrupted.
//! - The waiter that wins the flock claims its place by unlinking `.queued`,
//!   only once its holder record and control socket are published, so a kill
//!   that loses the race finds an addressable holder instead. A claim that
//!   finds the name gone was cancelled: it releases the flock without doing
//!   any work.
//!
//! No signal is sent, so a cancel works across PID namespaces, and a waiter
//! that is stopped (SIGSTOP) when its cancel commits can never run. Only "no
//! such file" is a verdict; any other error is reported as one.

use std::fs::File;
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::RunToken;
use super::lock_record::{escape_value, fields, unescape_value};

/// A published waiter.
const QUEUED_EXT: &str = "queued";
/// A waiter whose cancel committed and that has not yet let go of its record.
const CANCEL_EXT: &str = "cancel";
/// A record between creation and its flock; renamed to [`QUEUED_EXT`] once
/// complete and locked.
const PENDING_EXT: &str = "pending";

/// How old an unlocked `.pending` file must be before a sweep deletes it.
const PENDING_GRACE: Duration = Duration::from_secs(60);

/// Join attempts before a waiter gives up and refuses.
const JOIN_ATTEMPTS: usize = 3;

const PID: &str = "pid";
const STARTTIME: &str = "starttime";
const BOOT_ID: &str = "boot_id";
const PID_NS: &str = "pid_ns";
const TIME_NS: &str = "time_ns";
const CONFIGURED: &str = "configured";
const PROJECT: &str = "project";
const COMMAND: &str = "command";
const ROOT: &str = "root";
const JOINED: &str = "joined";
const ARGS: &str = "args";

/// What a waiter records about itself. PIDs are the waiter's own namespace's,
/// meaningful to a reader only where `process::shares_namespace_tokens`
/// holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaiterRecord {
    pub pid: u32,
    pub starttime: String,
    pub boot_id: String,
    pub pid_ns: String,
    pub time_ns: String,
    /// Whether a `brokkr.toml` was found: `project` is its key when it was,
    /// and empty (with the state root the only identity) when not.
    pub configured: bool,
    pub project: String,
    pub root: String,
    pub command: String,
    /// The invocation minus `argv[0]`, shell-quoted for display.
    pub args: String,
    /// When the waiter joined, in seconds since the Unix epoch.
    pub joined: u64,
}

impl WaiterRecord {
    fn encode(&self) -> Vec<u8> {
        format!(
            "{PID}={}\n{STARTTIME}={}\n{BOOT_ID}={}\n{PID_NS}={}\n{TIME_NS}={}\n{CONFIGURED}={}\n\
             {PROJECT}={}\n{COMMAND}={}\n{ROOT}={}\n{JOINED}={}\n{ARGS}={}\n",
            self.pid,
            escape_value(&self.starttime),
            escape_value(&self.boot_id),
            escape_value(&self.pid_ns),
            escape_value(&self.time_ns),
            u8::from(self.configured),
            escape_value(&self.project),
            escape_value(&self.command),
            escape_value(&self.root),
            self.joined,
            escape_value(&self.args),
        )
        .into_bytes()
    }

    /// Strict: every key must be present and the numeric ones must parse.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let fields = fields(bytes)?;
        let raw = |key: &str| fields.get(key).copied();
        let text = |key: &str| raw(key).map(unescape_value);
        let configured = match raw(CONFIGURED)? {
            "1" => true,
            "0" => false,
            _ => return None,
        };
        let pid = raw(PID)?.parse().ok().filter(|&pid: &u32| pid > 0)?;
        Some(Self {
            pid,
            starttime: text(STARTTIME)?,
            boot_id: text(BOOT_ID)?,
            pid_ns: text(PID_NS)?,
            time_ns: text(TIME_NS)?,
            configured,
            project: text(PROJECT)?,
            root: text(ROOT)?,
            command: text(COMMAND)?,
            args: text(ARGS)?,
            joined: raw(JOINED)?.parse().ok()?,
        })
    }
}

fn entry_path(dir: &Path, token: &RunToken, ext: &str) -> PathBuf {
    dir.join(format!("{}.{ext}", token.as_str()))
}

/// One non-blocking flock attempt: `Ok(true)` taken, `Ok(false)` held by
/// another open file description.
fn try_flock(file: &File, op: libc::c_int) -> io::Result<bool> {
    loop {
        // SAFETY: flock takes an fd we own for the duration of the call.
        if unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EWOULDBLOCK) => return Ok(false),
            _ => return Err(err),
        }
    }
}

/// This waiter's place in the queue. Dropping it leaves the queue: both of
/// its names are removed, whichever one it holds by then.
pub(super) struct Ticket {
    dir: PathBuf,
    token: RunToken,
    /// Holds the exclusive flock that proves this waiter is alive.
    _file: File,
}

/// Why one join attempt failed.
enum JoinFailure {
    /// A sweep took this attempt's pending file; start again.
    Raced,
    Fatal(io::Error),
}

impl Ticket {
    /// Publish `record` in `dir`, sweeping what dead waiters left first.
    pub(super) fn join(dir: &Path, record: &WaiterRecord) -> io::Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        sweep(dir);
        let bytes = record.encode();
        for _ in 0..JOIN_ATTEMPTS {
            match Self::try_join(dir, &bytes) {
                Ok(ticket) => return Ok(ticket),
                Err(JoinFailure::Raced) => {}
                Err(JoinFailure::Fatal(e)) => return Err(e),
            }
        }
        Err(io::Error::other(format!(
            "a sweep took the record on each of {JOIN_ATTEMPTS} attempts"
        )))
    }

    fn try_join(dir: &Path, bytes: &[u8]) -> Result<Self, JoinFailure> {
        let token = RunToken::mint()
            .ok_or_else(|| JoinFailure::Fatal(io::Error::other("no entropy for a queue token")))?;
        let pending = entry_path(dir, &token, PENDING_EXT);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)
            .map_err(JoinFailure::Fatal)?;
        match try_flock(&file, libc::LOCK_EX) {
            Ok(true) => {}
            // A sweep holds it and is about to remove it.
            Ok(false) => return Err(JoinFailure::Raced),
            Err(e) => {
                drop(std::fs::remove_file(&pending));
                return Err(JoinFailure::Fatal(e));
            }
        }
        if let Err(e) = file.write_all(bytes) {
            drop(std::fs::remove_file(&pending));
            return Err(JoinFailure::Fatal(e));
        }
        match std::fs::rename(&pending, entry_path(dir, &token, QUEUED_EXT)) {
            Ok(()) => Ok(Self {
                dir: dir.to_owned(),
                token,
                _file: file,
            }),
            // A sweep removed the pending name before this attempt locked it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(JoinFailure::Raced),
            Err(e) => {
                drop(std::fs::remove_file(&pending));
                Err(JoinFailure::Fatal(e))
            }
        }
    }

    /// Whether a `brokkr kill` has cancelled this waiter: its `.queued` name
    /// is gone. Any error other than "no such file" is an error, not a verdict.
    pub(super) fn cancelled(&self) -> io::Result<bool> {
        match std::fs::symlink_metadata(entry_path(&self.dir, &self.token, QUEUED_EXT)) {
            Ok(_) => Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Claim this waiter's place once it holds the flock: `Ok(true)` claimed,
    /// so no cancel can commit any more; `Ok(false)` a cancel committed first.
    pub(super) fn claim(&self) -> io::Result<bool> {
        match std::fs::remove_file(entry_path(&self.dir, &self.token, QUEUED_EXT)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        // Whichever name this waiter holds by now; a claimed ticket holds
        // neither.
        drop(std::fs::remove_file(entry_path(
            &self.dir,
            &self.token,
            QUEUED_EXT,
        )));
        drop(std::fs::remove_file(entry_path(
            &self.dir,
            &self.token,
            CANCEL_EXT,
        )));
    }
}

/// Remove what dead waiters left: every `.queued` and `.cancel` file whose
/// exclusive flock this process can take, and every `.pending` one past its
/// grace whose flock it can take. Best effort: a file that cannot be locked
/// or inspected stays.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((stem, ext)) = name.to_str().and_then(|n| n.split_once('.')) else {
            continue;
        };
        if RunToken::parse(stem).is_none() {
            continue;
        }
        let path = entry.path();
        let eligible = match ext {
            QUEUED_EXT | CANCEL_EXT => true,
            PENDING_EXT => entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= PENDING_GRACE),
            _ => false,
        };
        if !eligible {
            continue;
        }
        if let Ok(file) = File::open(&path)
            && matches!(try_flock(&file, libc::LOCK_EX), Ok(true))
        {
            drop(std::fs::remove_file(&path));
        }
    }
}

/// Where a live entry stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Waiting for the lock.
    Waiting,
    /// Its cancel committed; the waiter has not let go of its record yet.
    Cancelling,
}

/// One live entry of the queue.
#[derive(Debug)]
pub struct Entry {
    pub token: RunToken,
    pub standing: Standing,
    /// The record, or why it could not be read. A live entry whose record
    /// cannot be read is still a waiter; it cannot be placed in any scope.
    pub record: Result<WaiterRecord, String>,
}

/// Every live entry under `dir`, oldest first (unreadable ones last). Reads
/// only: nothing is swept. A missing directory is an empty queue; one that
/// cannot be read is an error.
pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some((stem, ext)) = name.to_str().and_then(|n| n.split_once('.')) else {
            continue;
        };
        let Some(token) = RunToken::parse(stem) else {
            continue;
        };
        let standing = match ext {
            QUEUED_EXT => Standing::Waiting,
            CANCEL_EXT => Standing::Cancelling,
            _ => continue,
        };
        let file = match File::open(entry.path()) {
            Ok(file) => file,
            // Left the queue since the directory was read.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                out.push(Entry {
                    token,
                    standing,
                    record: Err(format!("cannot open its record: {e}")),
                });
                continue;
            }
        };
        // A shared probe conflicts only with the waiter's exclusive flock.
        let record = match try_flock(&file, libc::LOCK_SH) {
            // Nobody holds it: a dead waiter's leftover.
            Ok(true) => continue,
            // Through the descriptor already open: the record is immutable,
            // and its name can go (a claim, another kill's rename) at any
            // moment without making it unreadable.
            Ok(false) => read_all(&file)
                .map_err(|e| format!("cannot read its record: {e}"))
                .and_then(|bytes| {
                    WaiterRecord::decode(&bytes).ok_or_else(|| "its record does not decode".into())
                }),
            Err(e) => Err(format!("cannot probe its flock: {e}")),
        };
        out.push(Entry {
            token,
            standing,
            record,
        });
    }
    out.sort_by_key(|e| {
        (
            e.record.as_ref().map_or(u64::MAX, |r| r.joined),
            e.token.as_str().to_owned(),
        )
    });
    Ok(out)
}

fn read_all(mut file: &File) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether `path` exists; only "no such file" is absence.
fn present(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// What a cancel request found.
pub enum Cancel {
    /// The cancel committed: the waiter can no longer take the lock. The
    /// handle confirms it let go ([`confirm`]).
    Committed(Committed),
    /// Another kill had already committed its cancel.
    AlreadyCancelled,
    /// It left the queue first: it took the lock, or exited.
    Left,
}

/// A committed cancel, holding the cancelled record open so its waiter's
/// flock can be watched.
pub struct Committed {
    file: File,
}

/// Cancel the waiter `token` under `dir` (see the module doc).
pub fn cancel(dir: &Path, token: &RunToken) -> io::Result<Cancel> {
    let queued = entry_path(dir, token, QUEUED_EXT);
    let cancelled = entry_path(dir, token, CANCEL_EXT);
    let gone = || {
        Ok(if present(&cancelled)? {
            Cancel::AlreadyCancelled
        } else {
            Cancel::Left
        })
    };
    let file = match File::open(&queued) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return gone(),
        Err(e) => return Err(e),
    };
    match std::fs::rename(&queued, &cancelled) {
        Ok(()) => Ok(Cancel::Committed(Committed { file })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => gone(),
        Err(e) => Err(e),
    }
}

/// Wait up to `budget`, shared by all, for each cancelled waiter to let go of
/// its record: `Ok(true)` where it did, `Ok(false)` where it still holds it,
/// and the error where its flock could not be probed. A cancel stays
/// committed whatever this finds.
pub fn confirm(committed: &[&Committed], budget: Duration) -> Vec<io::Result<bool>> {
    let deadline = Instant::now() + budget;
    let mut released: Vec<io::Result<bool>> = committed.iter().map(|_| Ok(false)).collect();
    loop {
        for (state, one) in released.iter_mut().zip(committed) {
            if matches!(state, Ok(false)) {
                *state = try_flock(&one.file, libc::LOCK_SH);
            }
        }
        let settled = released.iter().all(|s| !matches!(s, Ok(false)));
        if settled || Instant::now() >= deadline {
            return released;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn record(project: &str) -> WaiterRecord {
        WaiterRecord {
            pid: 4242,
            starttime: "123".into(),
            boot_id: "boot".into(),
            pid_ns: "1".into(),
            time_ns: "2".into(),
            configured: true,
            project: project.into(),
            root: "/r\nkey=injected".into(),
            command: "check".into(),
            args: "check --x".into(),
            joined: 7,
        }
    }

    #[test]
    fn a_record_round_trips_and_escapes_its_values() {
        // A key is compared as written, padding included.
        let r = record(" p ");
        assert_eq!(WaiterRecord::decode(&r.encode()), Some(r));
        assert!(WaiterRecord::decode(b"pid=1\nproject=p\n").is_none());
    }

    #[test]
    fn a_joined_waiter_is_listed_and_leaves_on_drop() {
        let dir = crate::test_scratch::scratch("lock_queue", "join");
        let ticket = Ticket::join(&dir, &record("p")).unwrap();
        let listed = list(&dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].standing, Standing::Waiting);
        assert_eq!(listed[0].record.as_ref().unwrap().project, "p");
        drop(ticket);
        assert!(list(&dir).unwrap().is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// The handshake: a committed cancel is seen by the waiter, makes its
    /// claim fail, and is confirmed once the waiter lets go.
    #[test]
    fn a_committed_cancel_wins_over_the_claim() {
        let dir = crate::test_scratch::scratch("lock_queue", "cancel");
        let ticket = Ticket::join(&dir, &record("p")).unwrap();
        let token = list(&dir).unwrap().remove(0).token;
        let Cancel::Committed(committed) = cancel(&dir, &token).unwrap() else {
            panic!("the cancel must commit");
        };
        assert!(ticket.cancelled().unwrap());
        assert!(!ticket.claim().unwrap(), "a cancelled waiter cannot claim");
        assert_eq!(list(&dir).unwrap()[0].standing, Standing::Cancelling);
        assert!(matches!(
            cancel(&dir, &token).unwrap(),
            Cancel::AlreadyCancelled
        ));
        assert!(matches!(
            confirm(&[&committed], Duration::ZERO).as_slice(),
            [Ok(false)]
        ));
        drop(ticket);
        assert!(matches!(
            confirm(&[&committed], Duration::from_secs(1)).as_slice(),
            [Ok(true)]
        ));
    }

    /// The other order: a claim that won leaves nothing to cancel.
    #[test]
    fn a_claim_wins_over_a_later_cancel() {
        let dir = crate::test_scratch::scratch("lock_queue", "claim");
        let ticket = Ticket::join(&dir, &record("p")).unwrap();
        let token = list(&dir).unwrap().remove(0).token;
        assert!(ticket.claim().unwrap());
        assert!(
            list(&dir).unwrap().is_empty(),
            "a claimed waiter is no longer queued"
        );
        assert!(matches!(cancel(&dir, &token).unwrap(), Cancel::Left));
    }

    /// A dead waiter's record (its flock gone) is not listed and is swept by
    /// the next join; a live one is never swept.
    #[test]
    fn a_dead_waiters_record_is_ignored_and_swept() {
        let dir = crate::test_scratch::scratch("lock_queue", "sweep");
        let token = RunToken::mint().unwrap();
        let dead = entry_path(&dir, &token, QUEUED_EXT);
        std::fs::write(&dead, record("gone").encode()).unwrap();
        assert!(list(&dir).unwrap().is_empty());
        let live = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!dead.exists(), "the join swept the dead record");
        sweep(&dir);
        assert_eq!(
            list(&dir).unwrap().len(),
            1,
            "a live record survives a sweep"
        );
        drop(live);
    }
}
