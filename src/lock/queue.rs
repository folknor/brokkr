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
//!
//! # Admission order
//!
//! The record above is the waiter's stable identity: cancellation and claim
//! act on it alone and it never moves. Order lives in a separate index: one
//! empty `<seq>-<token>.ticket` file per waiter that holds a place, created,
//! deleted and (for any decision) scanned only under the `admit.lock` flock,
//! which is never unlinked. A waiter's seq is one past the largest ticket
//! present when it was created, so arrival is ticket creation.
//!
//! A waiter tries the exclusive flock on `brokkr.lock` only under
//! `admit.lock`, and only when every older ticket belongs to a waiter that is
//! gone (record absent, cancelled, or unflocked) or stale: its record's mtime,
//! which a live waiter refreshes every poll round ([`Ticket::heartbeat`]), is
//! older than [`HEARTBEAT_GRACE`] as observed under the mutex. Stale tickets
//! are forfeited (deleted) only when that try succeeds, together with the
//! winner's own, so a failed try costs nobody a place. A waiter that finds its
//! ticket gone has forfeited and rejoins at the tail; its record, and so its
//! cancellability, never changed. The invariant: at a successful exclusive
//! try, every older ticket was already gone or was observed stale under the
//! mutex.
//!
//! A waiter reads the index without the mutex first and takes it (never
//! blocking on it) only when that read says it may be next, so younger
//! waiters do not crowd the head out of the mutex. A process stopped while it
//! holds `admit.lock` stops admission until it resumes or dies; cancellation
//! does not need the mutex and keeps working.

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

/// A waiter's place in the admission order (see the module doc).
const TICKET_EXT: &str = "ticket";
/// The admission mutex, under the queue directory. Never unlinked.
const ADMIT_LOCK: &str = "admit.lock";

/// How long a waiter's heartbeat may go unrefreshed before a waiter behind it
/// may take the lock past it. Waiters refresh it every poll round.
pub(super) const HEARTBEAT_GRACE: Duration = Duration::from_secs(1);

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
    file: File,
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
                file,
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

    /// Refresh this waiter's heartbeat: its record's mtime, through the held
    /// descriptor, so it works whichever name the record has.
    pub(super) fn heartbeat(&self) -> io::Result<()> {
        // SAFETY: a null times pointer sets both stamps to now; the fd is ours.
        if unsafe { libc::futimens(self.file.as_raw_fd(), std::ptr::null()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// One admission round: `try_lock` is the non-blocking exclusive try on
    /// `brokkr.lock`, run only when this waiter is next (see the module doc).
    /// `Ok(true)` means it succeeded; `Ok(false)` that this waiter is not next,
    /// the mutex was busy, or the lock is held. Joins the index on the first
    /// round that gets the mutex, and rejoins at the tail after a forfeit.
    pub(super) fn admit(&self, try_lock: &mut dyn FnMut() -> io::Result<bool>) -> io::Result<bool> {
        // The lockless look: only a waiter that may be next takes the mutex.
        let lookahead = index(&self.dir)?;
        if let Some(mine) = lookahead.iter().position(|(_, token)| token == &self.token) {
            for (_, token) in &lookahead[..mine] {
                if place(&self.dir, token)? == Place::Fresh {
                    return Ok(false);
                }
            }
        }
        let Some(_admit) = Admit::try_take(&self.dir)? else {
            return Ok(false);
        };
        let mut index = settled_index(&self.dir)?;
        let mine = match index.iter().position(|(_, token)| token == &self.token) {
            Some(mine) => mine,
            None => {
                // First round under the mutex, or a forfeit: take the tail.
                // Arrival is this creation, so the turn is evaluated at once.
                let seq = index.last().map_or(0, |(seq, _)| seq + 1);
                create_ticket(&self.dir, seq, &self.token)?;
                index.push((seq, self.token.clone()));
                index.len() - 1
            }
        };
        let ahead = &index[..mine];
        for (_, token) in ahead {
            if place(&self.dir, token)? == Place::Fresh {
                return Ok(false);
            }
        }
        if !try_lock()? {
            return Ok(false);
        }
        for (seq, token) in &index[..=mine] {
            remove_ticket(&self.dir, *seq, token)?;
        }
        Ok(true)
    }
}

/// A non-blocking acquisition's admission: `try_lock` runs only when no live
/// waiter holds a fresh place. `Ok(Some(true))` it succeeded and every stale
/// place was forfeited, `Ok(Some(false))` the lock is held, `Ok(None)` a
/// waiter is ahead or the mutex is busy. The directory is created if missing:
/// an empty queue is decided under the mutex like any other, since a waiter
/// may be creating it at the same moment.
pub(super) fn admit_nonblocking(
    dir: &Path,
    try_lock: &mut dyn FnMut() -> io::Result<bool>,
) -> io::Result<Option<bool>> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let Some(_admit) = Admit::try_take(dir)? else {
        return Ok(None);
    };
    let index = settled_index(dir)?;
    for (_, token) in &index {
        if place(dir, token)? == Place::Fresh {
            return Ok(None);
        }
    }
    if !try_lock()? {
        return Ok(Some(false));
    }
    for (seq, token) in &index {
        remove_ticket(dir, *seq, token)?;
    }
    Ok(Some(true))
}

/// The held admission mutex; dropping it releases the flock.
struct Admit {
    _file: File,
}

impl Admit {
    fn try_take(dir: &Path) -> io::Result<Option<Self>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(ADMIT_LOCK))?;
        Ok(try_flock(&file, libc::LOCK_EX)?.then_some(Self { _file: file }))
    }
}

fn ticket_path(dir: &Path, seq: u64, token: &RunToken) -> PathBuf {
    dir.join(format!("{seq:020}-{}.{TICKET_EXT}", token.as_str()))
}

fn create_ticket(dir: &Path, seq: u64, token: &RunToken) -> io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(ticket_path(dir, seq, token))
        .map(drop)
}

fn remove_ticket(dir: &Path, seq: u64, token: &RunToken) -> io::Result<()> {
    match std::fs::remove_file(ticket_path(dir, seq, token)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Every ticket under `dir`, in seq order.
fn index(dir: &Path) -> io::Result<Vec<(u64, RunToken)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        let Some(stem) = name
            .to_str()
            .and_then(|n| n.strip_suffix(TICKET_EXT))
            .and_then(|n| n.strip_suffix('.'))
        else {
            continue;
        };
        let Some((seq, token)) = stem.split_once('-') else {
            continue;
        };
        if let (Ok(seq), Some(token)) = (seq.parse::<u64>(), RunToken::parse(token)) {
            out.push((seq, token));
        }
    }
    out.sort_by(|a, b| (a.0, a.1.as_str()).cmp(&(b.0, b.1.as_str())));
    Ok(out)
}

/// The index with every gone waiter's ticket removed. Called under the mutex.
fn settled_index(dir: &Path) -> io::Result<Vec<(u64, RunToken)>> {
    let mut out = Vec::new();
    for (seq, token) in index(dir)? {
        if place(dir, &token)? == Place::Gone {
            remove_ticket(dir, seq, &token)?;
        } else {
            out.push((seq, token));
        }
    }
    Ok(out)
}

/// Where a ticket's waiter stands, read from its stable record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Live, heartbeat within [`HEARTBEAT_GRACE`].
    Fresh,
    /// Live, heartbeat older than the grace: may be passed.
    Stale,
    /// Exited, cancelled or claimed: holds no place.
    Gone,
}

fn place(dir: &Path, token: &RunToken) -> io::Result<Place> {
    let file = match File::open(entry_path(dir, token, QUEUED_EXT)) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Place::Gone),
        Err(e) => return Err(e),
    };
    if try_flock(&file, libc::LOCK_SH)? {
        return Ok(Place::Gone);
    }
    let age = file
        .metadata()?
        .modified()?
        .elapsed()
        .unwrap_or(Duration::ZERO);
    Ok(if age >= HEARTBEAT_GRACE {
        Place::Stale
    } else {
        Place::Fresh
    })
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

/// Every live entry under `dir`, in admission order (unreadable ones last). Reads
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
    // Admission order: a waiter with a place by its seq, then those without
    // one (joining, forfeited, or just admitted) by when they joined. Best
    // effort: `brokkr kill` cancels from this list, and needs only the
    // records, so an unreadable index costs the order and nothing else.
    let seqs: std::collections::HashMap<String, u64> = index(dir)
        .unwrap_or_default()
        .into_iter()
        .map(|(seq, token)| (token.as_str().to_owned(), seq))
        .collect();
    out.sort_by_key(|e| {
        (
            seqs.get(e.token.as_str()).copied().unwrap_or(u64::MAX),
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

    /// A `try_lock` that records whether it was called and answers `answer`.
    fn try_lock(calls: &std::cell::Cell<usize>, answer: bool) -> impl FnMut() -> io::Result<bool> {
        move || {
            calls.set(calls.get() + 1);
            Ok(answer)
        }
    }

    fn tokens(dir: &Path) -> Vec<RunToken> {
        index(dir).unwrap().into_iter().map(|(_, t)| t).collect()
    }

    /// Set a ticket's heartbeat back past the grace, as a stopped waiter's.
    fn age(ticket: &Ticket) {
        let old = std::time::SystemTime::now() - HEARTBEAT_GRACE * 5;
        ticket.file.set_modified(old).unwrap();
    }

    /// The younger of two placed waiters never tries the lock while the older
    /// one is live, however the lock stands; the older one then takes it.
    #[test]
    fn waiters_are_admitted_in_arrival_order() {
        let dir = scratch("fifo");
        let calls = std::cell::Cell::new(0);
        let older = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!older.admit(&mut try_lock(&calls, false)).unwrap());
        let younger = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!younger.admit(&mut try_lock(&calls, false)).unwrap());
        assert_eq!(calls.get(), 1, "the younger waiter was not next");
        assert_eq!(tokens(&dir), [older.token.clone(), younger.token.clone()]);
        // The lock is free now; still only the older waiter may try.
        assert!(!younger.admit(&mut try_lock(&calls, true)).unwrap());
        assert_eq!(calls.get(), 1);
        assert!(older.admit(&mut try_lock(&calls, true)).unwrap());
        assert_eq!(tokens(&dir), std::slice::from_ref(&younger.token));
        assert!(younger.admit(&mut try_lock(&calls, true)).unwrap());
    }

    /// A stale head keeps its place through a failed try, loses it only to a
    /// successful one, and rejoins behind every waiter placed meanwhile.
    #[test]
    fn a_stale_head_is_passed_only_by_a_taken_lock_and_rejoins_at_the_tail() {
        let dir = scratch("stale-head");
        let calls = std::cell::Cell::new(0);
        let head = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!head.admit(&mut try_lock(&calls, false)).unwrap());
        let passer = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!passer.admit(&mut try_lock(&calls, false)).unwrap());
        age(&head);
        // The lock is held: the passer tries and fails, and nobody forfeits.
        assert!(!passer.admit(&mut try_lock(&calls, false)).unwrap());
        assert_eq!(tokens(&dir), [head.token.clone(), passer.token.clone()]);
        assert!(passer.admit(&mut try_lock(&calls, true)).unwrap());
        assert!(tokens(&dir).is_empty(), "the stale place was forfeited");
        let third = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!third.admit(&mut try_lock(&calls, false)).unwrap());
        // The head resumes: its place is gone, so it takes the tail.
        head.heartbeat().unwrap();
        let before = calls.get();
        assert!(!head.admit(&mut try_lock(&calls, true)).unwrap());
        assert_eq!(calls.get(), before, "the rejoined head is not next");
        assert_eq!(tokens(&dir), [third.token.clone(), head.token.clone()]);
    }

    /// A non-blocking try refuses behind a live place even with the lock
    /// free, and on an absent queue decides under the mutex and tries.
    #[test]
    fn a_nonblocking_try_does_not_pass_a_waiter() {
        let dir = scratch("nonblocking");
        let calls = std::cell::Cell::new(0);
        let absent = dir.join("absent");
        assert_eq!(
            admit_nonblocking(&absent, &mut try_lock(&calls, true)).unwrap(),
            Some(true)
        );
        assert!(absent.join(ADMIT_LOCK).exists());
        let waiter = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!waiter.admit(&mut try_lock(&calls, false)).unwrap());
        let before = calls.get();
        assert_eq!(
            admit_nonblocking(&dir, &mut try_lock(&calls, true)).unwrap(),
            None
        );
        assert_eq!(calls.get(), before, "it never tried the lock");
        // A stale waiter is passed, and only by a taken lock.
        age(&waiter);
        assert_eq!(
            admit_nonblocking(&dir, &mut try_lock(&calls, false)).unwrap(),
            Some(false)
        );
        assert_eq!(tokens(&dir), std::slice::from_ref(&waiter.token));
        assert_eq!(
            admit_nonblocking(&dir, &mut try_lock(&calls, true)).unwrap(),
            Some(true)
        );
        assert!(tokens(&dir).is_empty());
    }

    /// A dead or cancelled waiter's place is swept, not waited behind.
    #[test]
    fn a_gone_waiters_place_does_not_block() {
        let dir = scratch("gone");
        let calls = std::cell::Cell::new(0);
        let dead = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!dead.admit(&mut try_lock(&calls, false)).unwrap());
        let cancelled = Ticket::join(&dir, &record("p")).unwrap();
        assert!(!cancelled.admit(&mut try_lock(&calls, false)).unwrap());
        let me = Ticket::join(&dir, &record("p")).unwrap();
        drop(dead);
        assert!(matches!(
            cancel(&dir, &cancelled.token).unwrap(),
            Cancel::Committed(_)
        ));
        assert!(me.admit(&mut try_lock(&calls, true)).unwrap());
    }

    fn scratch(name: &str) -> PathBuf {
        crate::test_scratch::scratch("lock_queue", name)
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
