//! The lock holder's control socket: how another brokkr asks the holder about
//! itself, and asks it to stop, without interpreting a single PID.
//!
//! Every PID in the lock file is namespace-local. A holder running in a
//! sandbox that gives each command its own PID namespace (codex does) records
//! `pid=2`, and no reader outside that namespace - a sibling sandbox or the
//! host - can verify or signal it. Translating the number from the reader's
//! side does not work in general: `/proc` starttimes are offset by the
//! *reader's* time namespace, a procfs mount may number processes for a
//! namespace other than the caller's, and a namespace inode is not a lasting
//! identity. So the holder answers for itself instead.
//!
//! Each fresh acquisition binds `~/.brokkr/lock/<id>.sock`, where `<id>` is a
//! random public acquisition id recorded in the lock file. It is deliberately
//! NOT the compilation capability (`crate::hold`), whose raw value must stay
//! unpublished. A pathname unix socket is reachable across PID namespaces
//! wherever the file is: measured from a codex sandbox to the host, and between
//! sibling codex sandboxes. Peer credentials cannot carry a usable pid across
//! sibling namespaces (`SO_PEERCRED` reports pid 0), so the socket is gated by
//! file permissions (directory 0700, socket 0600) and the id; the threat model
//! is accidents, not a hostile process running as the same user.
//!
//! Two requests, one line each: `status <id>` and `stop <id>`.
//!
//! - **status** answers from `/proc/self` and from a snapshot the holder
//!   publishes whenever it rewrites the lock file. It never reads another
//!   process's `/proc` entry, so procfs numbering never enters into it. The
//!   snapshot is taken with `try_lock`; contention answers `busy`. A status
//!   reply is not proof the hold is still live - the flock probe is.
//! - **stop** commits the process to stopping: under the service's `live`
//!   mutex it sets the sticky stop flag (`crate::shutdown::commit_stop`, which
//!   no guard clears and which every acquisition and admission path checks),
//!   writes `accepted`, and then sends this process a `SIGTERM` for the
//!   handler's immediate effects. `accepted` means that commitment was made
//!   while this acquisition was live. It is not a promise of termination: a
//!   phase that neither polls the flag nor has a handler installed takes
//!   `SIGTERM`'s default action, exactly as a numeric `brokkr kill` always did
//!   there - which is why the answer is written before the signal.
//!
//! Release never waits long on the service. The service thread owns the
//! listener and holds no reference to the lock guard; release flips `live` (a
//! mutex only ever held for a check, an atomic store, one short reply write
//! under the connection's deadline and one `kill(2)`), unlinks the path and
//! moves on. The thread notices within one poll and exits. Every connection is
//! served under absolute deadlines for the whole request and the whole reply.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{AcquireRefusal, DevError, LockError};
use crate::lockfile::AcqId;
use crate::lockfile::collector::Collector;
use crate::lockfile::observe::Observed;
use crate::lockfile::report::Extension;

/// Longest request line accepted. `stop <32 hex>` is 37 bytes.
const MAX_REQUEST: usize = 256;
/// Whole-request and whole-reply budgets for one connection.
const IO_DEADLINE: Duration = Duration::from_secs(1);
/// How often the service rechecks `live` while idle.
const POLL: Duration = Duration::from_millis(50);
/// A client's budget for one exchange.
const CLIENT_DEADLINE: Duration = Duration::from_secs(2);

/// What the holder states about itself that never changes over the hold.
#[derive(Clone, Default)]
pub struct Identity {
    pub project: String,
    pub command: String,
    pub args: String,
    pub root: String,
    pub pid_ns: String,
    pub time_ns: String,
    pub codex_thread: String,
    pub codex_session: String,
}

/// The mutable part, republished by the holder whenever it rewrites the lock
/// file.
#[derive(Clone, Copy, Default)]
pub struct Snapshot {
    pub draining: bool,
    pub progress: Option<(u32, u32)>,
    pub has_child: bool,
    pub mocks: usize,
}

struct Shared {
    id: AcqId,
    live: Mutex<bool>,
    identity: Identity,
    snapshot: Mutex<Snapshot>,
    started: Instant,
    /// The hold's activities, launch cohorts and marker.
    observed: Arc<Observed>,
    /// The process sampler; status replies serve its cache and kick it.
    collector: Arc<Collector>,
}

/// A running control socket, owned by one acquisition. Dropping it ends the
/// acquisition's service: no stop is accepted afterwards.
pub struct Service {
    shared: Arc<Shared>,
    path: PathBuf,
}

/// `~/.brokkr/lock`, created 0700.
fn socket_dir() -> Result<PathBuf, DevError> {
    let dir = crate::user_dirs::socket_dir().ok_or_else(|| {
        DevError::Lock(LockError::acquisition(AcquireRefusal::SocketDirectory {
            reason: "$HOME is not set or is empty - cannot place the lock socket".into(),
        }))
    })?;
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// A fresh public acquisition id. A separate draw from the capability nonce.
pub fn mint_id() -> Option<AcqId> {
    AcqId::mint()
}

fn socket_path(dir: &Path, id: &AcqId) -> PathBuf {
    dir.join(format!("{}.sock", id.as_str()))
}

/// Remove sockets left by acquisitions that are over. Called only while
/// holding the flock, which proves no other acquisition is live; strictly
/// limited to validated `<id>.sock` names that are sockets, never followed
/// through a symlink.
pub fn sweep_leftovers() {
    use std::os::unix::fs::FileTypeExt;
    let Ok(dir) = socket_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".sock")) else {
            continue;
        };
        if AcqId::parse(id).is_none() {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_socket()) {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

impl Service {
    /// Bind and start serving. The socket is only reachable once this returns,
    /// so it is never published before it can answer.
    pub fn start(id: &AcqId, identity: Identity) -> Result<Self, DevError> {
        let path = socket_path(&socket_dir()?, id);
        // `bind` refuses an existing path; a collision on a fresh 128-bit id is
        // a bug, not something to unlink over.
        let listener = UnixListener::bind(&path).map_err(|source| {
            DevError::Lock(LockError::acquisition(AcquireRefusal::SocketBind {
                path: path.clone(),
                source,
            }))
        })?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
        listener.set_nonblocking(true)?;
        let observed = Arc::new(Observed::new(id.clone()));
        let shared = Arc::new(Shared {
            id: id.clone(),
            live: Mutex::new(true),
            identity,
            snapshot: Mutex::new(Snapshot {
                draining: true,
                ..Snapshot::default()
            }),
            started: Instant::now(),
            collector: Arc::new(Collector::new(Arc::clone(&observed))),
            observed,
        });
        let serving = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name("brokkr-lock-service".into())
            .spawn(move || serve(&listener, &serving));
        if let Err(e) = spawned {
            std::fs::remove_file(&path).ok();
            return Err(DevError::Lock(LockError::acquisition(
                AcquireRefusal::ServiceThread { source: e },
            )));
        }
        Ok(Self { shared, path })
    }

    /// The acquisition's observation table, shared with the `LockInner`.
    pub(crate) fn observed(&self) -> Arc<Observed> {
        Arc::clone(&self.shared.observed)
    }

    /// Republish the mutable snapshot status replies are built from.
    pub fn update(&self, snapshot: Snapshot) {
        if let Ok(mut slot) = self.shared.snapshot.lock() {
            *slot = snapshot;
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // Ordered against a concurrent stop by the `live` mutex: either the
        // stop committed first, or it finds the service dead and refuses.
        *self
            .shared
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        std::fs::remove_file(&self.path).ok();
    }
}

fn is_live(shared: &Shared) -> bool {
    *shared
        .live
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The service loop. Exits as soon as `live` is false; owns the listener, so
/// the socket closes with it.
fn serve(listener: &UnixListener, shared: &Shared) {
    unblock_sigterm();
    while is_live(shared) {
        match listener.accept() {
            Ok((stream, _)) => handle(&stream, shared),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => thread::sleep(POLL),
        }
    }
}

/// This thread must be able to take the `SIGTERM` it sends, whatever mask it
/// inherited from the thread that started it.
fn unblock_sigterm() {
    // SAFETY: a zeroed sigset initialised by sigemptyset, used for one call.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

fn handle(stream: &UnixStream, shared: &Shared) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let Some(line) = read_line(stream, Instant::now() + IO_DEADLINE) else {
        return;
    };
    let deadline = Instant::now() + IO_DEADLINE;
    let (verb, id) = line.split_once(' ').unwrap_or((&line, ""));
    if AcqId::parse(id).as_ref() != Some(&shared.id) || !is_live(shared) {
        write_all(stream, b"gone\n", deadline);
        return;
    }
    match verb {
        "status" => {
            write_all(stream, status_reply(shared).as_bytes(), deadline);
        }
        "stop" => stop(stream, shared, deadline),
        _ => {
            write_all(stream, b"error unknown request\n", deadline);
        }
    }
}

/// Commit the stop, answer `accepted`, then signal this process.
///
/// The answer goes out BEFORE the `SIGTERM`: where no `SigtermGuard` is in
/// scope the signal takes its default action and the process is gone before
/// any later write, which left the client reading an empty reply and
/// reporting that nothing was stopped. Bytes already written stay readable
/// after the process dies. A failed self-signal is reported on a second
/// `signal_error=` line.
fn stop(stream: &UnixStream, shared: &Shared, deadline: Instant) {
    stop_with(
        stream,
        shared,
        deadline,
        crate::shutdown::commit_stop,
        || match crate::process::signal_pid(std::process::id(), libc::SIGTERM) {
            Ok(true) => None,
            Ok(false) => Some(io::Error::from_raw_os_error(libc::ESRCH)),
            Err(error) => Some(error),
        },
    );
}

/// [`stop`] with the commit and the self-signal supplied, so the ordering can
/// be tested without stopping the test process.
fn stop_with(
    stream: &UnixStream,
    shared: &Shared,
    deadline: Instant,
    commit: impl FnOnce(),
    signal_self: impl FnOnce() -> Option<io::Error>,
) {
    let live = shared
        .live
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !*live {
        drop(live);
        write_all(stream, b"gone\n", deadline);
        return;
    }
    commit();
    write_all(stream, b"accepted\n", deadline);
    let signal_error = signal_self();
    drop(live);
    if let Some(e) = signal_error {
        let line = format!(
            "signal_error={}\n",
            crate::lockfile::escape_value(&e.to_string())
        );
        write_all(stream, line.as_bytes(), deadline);
    }
}

fn status_reply(shared: &Shared) -> String {
    let Ok(snapshot) = shared.snapshot.try_lock().map(|s| *s) else {
        return "busy\n".into();
    };
    let (rss_kb, threads) = self_status();
    HolderStatus {
        identity: shared.identity.clone(),
        snapshot,
        held: shared.started.elapsed(),
        cpu: self_cpu().map(|cpu| Duration::from_secs(cpu.as_secs())),
        rss_kb,
        threads,
        own_cpu: self_cpu(),
        ext: Some(extension(shared)),
    }
    .encode()
}

/// The report extension: activities and marker from memory, the cached
/// process sample (a fresh one is kicked off, never waited for).
fn extension(shared: &Shared) -> Extension {
    let snap = shared.observed.snapshot();
    let ms = |at: Instant| u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let activities = snap.activities.map(|open| {
        open.into_iter()
            .map(|a| crate::lockfile::report::ActivityRow {
                id: a.id,
                parent: a.parent,
                elapsed_ms: ms(a.started),
                progress: a.progress,
                label: a.label,
                narration: a.narration,
            })
            .collect()
    });
    shared.collector.kick();
    Extension {
        activities,
        activities_omitted: 0,
        narration: snap.narration.map(|(text, at)| (text, ms(at))),
        marker: snap.marker.map(|(name, at)| (name, ms(at))),
        sample: shared.collector.cached(),
        collecting: shared.collector.collecting(),
    }
}

/// `VmRSS` (kB) and `Threads` of this process.
fn self_status() -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return (None, None);
    };
    let mut rss = None;
    let mut threads = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            rss = v.trim().trim_end_matches(" kB").trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("Threads:") {
            threads = v.trim().parse().ok();
        }
    }
    (rss, threads)
}

/// User plus system CPU of this process (`utime` + `stime`), at tick
/// resolution.
fn self_cpu() -> Option<Duration> {
    let stat = crate::process::ProcStat::read_self().ok()?;
    let ticks = stat.user_ticks().checked_add(stat.system_ticks())?;
    let tick_rate = crate::process::clock_ticks_per_second()?;
    Some(Duration::from_millis(
        ticks.saturating_mul(1000) / tick_rate,
    ))
}

/// Read one `\n`-terminated line under an absolute deadline.
fn read_line(mut stream: &UnixStream, deadline: Instant) -> Option<String> {
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0_u8; 64];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        if left.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&byte[..n]);
                if buf.contains(&b'\n') || buf.len() >= MAX_REQUEST {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    let text = String::from_utf8(buf).ok()?;
    Some(text.lines().next().unwrap_or("").trim().to_owned())
}

/// Write everything under an absolute deadline; give up silently past it.
fn write_all(mut stream: &UnixStream, mut bytes: &[u8], deadline: Instant) -> bool {
    while !bytes.is_empty() {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        if left.is_zero() || stream.set_write_timeout(Some(left)).is_err() {
            return false;
        }
        match stream.write(bytes) {
            Ok(0) => return false,
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Client side.
// ---------------------------------------------------------------------------

const STATUS_PROJECT: &str = "project";
const STATUS_COMMAND: &str = "command";
const STATUS_ARGS: &str = "args";
const STATUS_ROOT: &str = "root";
const STATUS_PID_NS: &str = "pid_ns";
const STATUS_TIME_NS: &str = "time_ns";
const STATUS_CODEX_THREAD: &str = "codex_thread";
const STATUS_CODEX_SESSION: &str = "codex_session";
const STATUS_HELD_SECS: &str = "held_secs";
const STATUS_CPU_SECS: &str = "cpu_secs";
const STATUS_RSS_KB: &str = "rss_kb";
const STATUS_THREADS: &str = "threads";
const STATUS_DRAINING: &str = "draining";
const STATUS_PROGRESS: &str = "progress";
const STATUS_CHILD: &str = "child";
const STATUS_MOCKS: &str = "mocks";
const STATUS_OWN_CPU_MS: &str = "own_cpu_ms";

/// The typed answer the holder returns to `status`.
pub struct HolderStatus {
    pub identity: Identity,
    pub snapshot: Snapshot,
    pub held: Duration,
    pub cpu: Option<Duration>,
    pub rss_kb: Option<u64>,
    pub threads: Option<u64>,
    /// The holder's own CPU at millisecond resolution (`own_cpu_ms`, absent
    /// from older holders, which send whole seconds only).
    pub own_cpu: Option<Duration>,
    /// The report extension (`crate::lockfile::report`), absent from older
    /// holders or dropped when malformed.
    pub(crate) ext: Option<Extension>,
}

impl HolderStatus {
    fn encode(&self) -> String {
        let mut out = String::from("ok\n");
        let mut field = |key: &str, value: &str| {
            out.push_str(key);
            out.push('=');
            out.push_str(&crate::lockfile::escape_value(value));
            out.push('\n');
        };
        field(STATUS_PROJECT, &self.identity.project);
        field(STATUS_COMMAND, &self.identity.command);
        field(STATUS_ARGS, &self.identity.args);
        field(STATUS_ROOT, &self.identity.root);
        field(STATUS_PID_NS, &self.identity.pid_ns);
        field(STATUS_TIME_NS, &self.identity.time_ns);
        field(STATUS_CODEX_THREAD, &self.identity.codex_thread);
        field(STATUS_CODEX_SESSION, &self.identity.codex_session);
        field(STATUS_HELD_SECS, &self.held.as_secs().to_string());
        field(
            STATUS_CPU_SECS,
            &self
                .cpu
                .map_or_else(String::new, |cpu| cpu.as_secs().to_string()),
        );
        field(
            STATUS_RSS_KB,
            &self
                .rss_kb
                .map_or_else(String::new, |value| value.to_string()),
        );
        field(
            STATUS_THREADS,
            &self
                .threads
                .map_or_else(String::new, |value| value.to_string()),
        );
        field(
            STATUS_DRAINING,
            if self.snapshot.draining { "1" } else { "0" },
        );
        field(
            STATUS_PROGRESS,
            &self
                .snapshot
                .progress
                .map(|(run, total)| format!("{run}/{total}"))
                .unwrap_or_default(),
        );
        field(
            STATUS_CHILD,
            if self.snapshot.has_child { "1" } else { "0" },
        );
        field(STATUS_MOCKS, &self.snapshot.mocks.to_string());
        field(
            STATUS_OWN_CPU_MS,
            &self
                .own_cpu
                .map_or_else(String::new, |cpu| cpu.as_millis().to_string()),
        );
        if let Some(ext) = &self.ext {
            out.push_str(&ext.encode());
        }
        out
    }

    fn decode(lines: &[&str]) -> Option<Self> {
        let mut fields: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
        for line in lines {
            if let Some((key, value)) = line.split_once('=') {
                fields
                    .entry(key)
                    .or_insert_with(|| crate::lockfile::unescape_value(value));
            }
        }
        let value = |key: &str| fields.get(key).map(String::as_str);
        let draining = parse_bool(value(STATUS_DRAINING)?)?;
        let has_child = parse_bool(value(STATUS_CHILD)?)?;
        let progress = parse_optional(value(STATUS_PROGRESS)?, |raw| {
            let (run, total) = raw.split_once('/')?;
            Some((run.parse::<u32>().ok()?, total.parse::<u32>().ok()?))
        })?;
        let cpu = parse_optional(value(STATUS_CPU_SECS)?, |raw| {
            raw.parse::<u64>().ok().map(Duration::from_secs)
        })?;
        let rss_kb = parse_optional(value(STATUS_RSS_KB)?, |raw| raw.parse::<u64>().ok())?;
        let threads = parse_optional(value(STATUS_THREADS)?, |raw| raw.parse::<u64>().ok())?;
        let held = Duration::from_secs(value(STATUS_HELD_SECS)?.parse().ok()?);
        let mocks = value(STATUS_MOCKS)?.parse().ok()?;
        // Optional: an older holder sends neither.
        let own_cpu = value(STATUS_OWN_CPU_MS)
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_millis);
        let ext = Extension::decode(lines);

        Some(Self {
            identity: Identity {
                project: value(STATUS_PROJECT)?.to_owned(),
                command: value(STATUS_COMMAND)?.to_owned(),
                args: value(STATUS_ARGS)?.to_owned(),
                root: value(STATUS_ROOT)?.to_owned(),
                pid_ns: value(STATUS_PID_NS)?.to_owned(),
                time_ns: value(STATUS_TIME_NS)?.to_owned(),
                codex_thread: value(STATUS_CODEX_THREAD)?.to_owned(),
                codex_session: value(STATUS_CODEX_SESSION)?.to_owned(),
            },
            snapshot: Snapshot {
                draining,
                progress,
                has_child,
                mocks,
            },
            held,
            cpu,
            rss_kb,
            threads,
            own_cpu,
            ext,
        })
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn parse_optional<T>(value: &str, parse: impl FnOnce(&str) -> Option<T>) -> Option<Option<T>> {
    if value.is_empty() {
        Some(None)
    } else {
        parse(value).map(Some)
    }
}

/// What came back from the holder.
pub enum Reply {
    Status(Box<HolderStatus>),
    /// A stop was committed. `signal_error` is set when the holder's
    /// self-`SIGTERM` failed; the sticky stop flag is set regardless.
    Accepted {
        signal_error: Option<String>,
    },
    /// The id is not this holder's live acquisition (it released, or the
    /// metadata is stale).
    Gone,
    /// The holder was mid-update; ask again.
    Busy,
    /// The holder answered `status`, but its fields did not match the status
    /// schema this client understands.
    InvalidStatus(String),
    /// The request never reached the holder: the socket is missing or
    /// refused, or the holder did not take the request in time. Says nothing
    /// about liveness - the flock does.
    NoAnswer(String),
    /// The request was delivered, but no well-formed answer came back: the
    /// connection closed empty, the reply timed out (a SIGSTOPped holder,
    /// say) or was not one this client knows. For a `stop` the outcome is
    /// unknown - the holder may have committed it and died before answering.
    Unconfirmed(String),
}

/// Send one request to the holder of acquisition `id`.
pub fn request(id: &str, verb: &str) -> Reply {
    let Some(id) = AcqId::parse(id) else {
        return Reply::NoAnswer("the lock file names no valid acquisition id".into());
    };
    let dir = match socket_dir() {
        Ok(d) => d,
        Err(e) => return Reply::NoAnswer(e.to_string()),
    };
    let path = socket_path(&dir, &id);
    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => return Reply::NoAnswer(format!("cannot reach the holder's socket: {e}")),
    };
    let deadline = Instant::now() + CLIENT_DEADLINE;
    if !write_all(
        &stream,
        format!("{verb} {}\n", id.as_str()).as_bytes(),
        deadline,
    ) {
        return Reply::NoAnswer("the holder did not take the request in time".into());
    }
    let Some(text) = read_reply(&stream, deadline) else {
        // For a stop, a lost reply means the outcome is unknown, not refused.
        return Reply::Unconfirmed("no reply from the holder in time".into());
    };
    parse_reply(&text)
}

fn read_reply(mut stream: &UnixStream, deadline: Instant) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        if left.is_zero() {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 64 * 1024 {
                    return None;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
}

fn parse_reply(text: &str) -> Reply {
    let mut lines = text.lines();
    let head = lines.next().unwrap_or("");
    match head.split_once(' ').map_or(head, |(h, _)| h) {
        "ok" => match HolderStatus::decode(&lines.collect::<Vec<_>>()) {
            Some(status) => Reply::Status(Box::new(status)),
            None => {
                Reply::InvalidStatus("the holder returned a missing or invalid status field".into())
            }
        },
        // A current holder reports a failed self-signal on its own line, after
        // `accepted`; an older one appended it to the head.
        "accepted" => Reply::Accepted {
            signal_error: std::iter::once(head)
                .chain(lines)
                .find_map(|line| line.split_once("signal_error="))
                .map(|(_, e)| crate::lockfile::unescape_value(e)),
        },
        "gone" => Reply::Gone,
        "busy" => Reply::Busy,
        "" => Reply::Unconfirmed("the holder closed the connection without replying".into()),
        other => Reply::Unconfirmed(format!("unexpected reply from the holder: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ids_are_validated_strictly() {
        assert!(AcqId::parse(mint_id().unwrap().as_str()).is_some());
        assert!(AcqId::parse("").is_none());
        assert!(AcqId::parse("../../etc/passwd").is_none());
        assert!(AcqId::parse(&"A".repeat(32)).is_none());
        assert!(AcqId::parse(&"a".repeat(31)).is_none());
    }

    #[test]
    fn replies_round_trip() {
        let Reply::Status(s) = parse_reply(
            "ok\nproject=p\ncommand=run\nargs=a%0Ab%25\nroot=/r\npid_ns=1\ntime_ns=2\n\
             codex_thread=t\ncodex_session=s\nheld_secs=3\ncpu_secs=4\nrss_kb=5\nthreads=6\n\
             draining=1\nprogress=2/5\nchild=1\nmocks=2\n",
        ) else {
            panic!("status");
        };
        assert_eq!(s.identity.args, "a\nb%");
        assert_eq!(s.identity.project, "p");
        assert_eq!(s.held, Duration::from_secs(3));
        assert_eq!(s.cpu, Some(Duration::from_secs(4)));
        assert_eq!(s.rss_kb, Some(5));
        assert_eq!(s.threads, Some(6));
        assert!(s.snapshot.draining);
        assert_eq!(s.snapshot.progress, Some((2, 5)));
        assert!(s.snapshot.has_child);
        assert_eq!(s.snapshot.mocks, 2);
        assert!(matches!(
            parse_reply("ok\nproject=p\n"),
            Reply::InvalidStatus(_)
        ));
        assert!(matches!(
            parse_reply("accepted\n"),
            Reply::Accepted { signal_error: None }
        ));
        assert!(matches!(
            parse_reply("accepted signal_error=EPERM\n"),
            Reply::Accepted { signal_error: Some(e) } if e == "EPERM"
        ));
        assert!(matches!(parse_reply("gone\n"), Reply::Gone));
        assert!(matches!(parse_reply("busy\n"), Reply::Busy));
    }

    #[test]
    fn stop_replies_carry_signal_errors_and_unknowns_are_unconfirmed() {
        assert!(matches!(
            parse_reply("accepted\nsignal_error=EPERM\n"),
            Reply::Accepted { signal_error: Some(e) } if e == "EPERM"
        ));
        assert!(matches!(parse_reply("nonsense\n"), Reply::Unconfirmed(_)));
        // The bug: a holder killed by its own SIGTERM before answering left
        // an empty reply, which read as "no answer" and "nothing was stopped".
        assert!(matches!(parse_reply(""), Reply::Unconfirmed(_)));
    }

    fn shared() -> Shared {
        let id = mint_id().unwrap();
        let observed = Arc::new(Observed::new(id.clone()));
        Shared {
            id,
            live: Mutex::new(true),
            identity: Identity::default(),
            snapshot: Mutex::new(Snapshot::default()),
            started: Instant::now(),
            collector: Arc::new(Collector::new(Arc::clone(&observed))),
            observed,
        }
    }

    /// A reply from a holder older than the report extension still decodes,
    /// with no extension; a newer holder's extension rides the same reply,
    /// and the legacy keys an older client requires are all still present.
    #[test]
    fn the_extension_is_optional_and_additive() {
        let Reply::Status(old) = parse_reply(
            "ok\nproject=p\ncommand=run\nargs=\nroot=/r\npid_ns=1\ntime_ns=2\ncodex_thread=\n\
             codex_session=\nheld_secs=3\ncpu_secs=4\nrss_kb=5\nthreads=6\ndraining=0\n\
             progress=\nchild=0\nmocks=0\n",
        ) else {
            panic!("status");
        };
        assert!(old.ext.is_none() && old.own_cpu.is_none());

        let shared = shared();
        let activity = shared.observed.open(None, "verify merge").unwrap();
        shared.observed.set_progress(activity, 2, 9);
        let text = status_reply(&shared);
        let Reply::Status(new) = parse_reply(&text) else {
            panic!("status");
        };
        let ext = new.ext.unwrap();
        let rows = ext.activities.unwrap();
        assert_eq!(rows[0].label, "verify merge");
        assert_eq!(rows[0].progress, Some((2, 9)));
        assert!(new.own_cpu.is_some());
        for key in [
            "project=",
            "command=",
            "args=",
            "root=",
            "pid_ns=",
            "time_ns=",
            "codex_thread=",
            "codex_session=",
            "held_secs=",
            "cpu_secs=",
            "rss_kb=",
            "threads=",
            "draining=",
            "progress=",
            "child=",
            "mocks=",
        ] {
            assert!(
                text.contains(&format!("\n{key}")),
                "legacy key {key} missing"
            );
        }
    }

    /// `accepted` is on the wire before the self-signal, so a holder that the
    /// signal kills outright has still answered: at the moment of the signal
    /// the client can already read it.
    #[test]
    fn stop_answers_before_signalling() {
        let shared = shared();
        let (server, client) = UnixStream::pair().unwrap();
        let mut committed = false;
        stop_with(
            &server,
            &shared,
            Instant::now() + IO_DEADLINE,
            || committed = true,
            || {
                let mut buf = [0_u8; 16];
                client.set_nonblocking(true).unwrap();
                let n = (&client).read(&mut buf).unwrap();
                assert_eq!(
                    &buf[..n],
                    b"accepted\n",
                    "the answer must precede the signal"
                );
                client.set_nonblocking(false).unwrap();
                None
            },
        );
        assert!(committed);
    }

    /// A failed self-signal still answers `accepted`, with the error on its
    /// own line; a dead service answers `gone` and commits nothing.
    #[test]
    fn stop_reports_signal_error_and_refuses_when_not_live() {
        let shared = shared();
        let (server, client) = UnixStream::pair().unwrap();
        stop_with(
            &server,
            &shared,
            Instant::now() + IO_DEADLINE,
            || {},
            || Some(io::Error::from_raw_os_error(libc::EPERM)),
        );
        drop(server);
        let text = read_reply(&client, Instant::now() + IO_DEADLINE).unwrap();
        assert!(matches!(
            parse_reply(&text),
            Reply::Accepted {
                signal_error: Some(_)
            }
        ));

        *shared.live.lock().unwrap() = false;
        let (server, client) = UnixStream::pair().unwrap();
        stop_with(
            &server,
            &shared,
            Instant::now() + IO_DEADLINE,
            || panic!("a dead service must not commit a stop"),
            || panic!("a dead service must not signal"),
        );
        drop(server);
        let text = read_reply(&client, Instant::now() + IO_DEADLINE).unwrap();
        assert!(matches!(parse_reply(&text), Reply::Gone));
    }

    /// A status request is answered over a real socket, a request naming
    /// another acquisition is told `gone`, and once the service drops the
    /// same id is no longer reachable. Uses the real socket dir; the id is
    /// fresh, so it cannot collide with a live hold's.
    #[test]
    fn service_answers_status_and_stops_answering_after_drop() {
        let id = mint_id().unwrap();
        let identity = Identity {
            project: "p".into(),
            args: "x y".into(),
            ..Identity::default()
        };
        let service = Service::start(&id, identity).unwrap();
        service.update(Snapshot {
            draining: false,
            progress: Some((2, 5)),
            has_child: true,
            mocks: 1,
        });
        let Reply::Status(s) = request(id.as_str(), "status") else {
            panic!("status")
        };
        assert_eq!(s.identity.project, "p");
        assert_eq!(s.identity.args, "x y");
        assert_eq!(s.snapshot.progress, Some((2, 5)));
        assert!(!s.snapshot.draining);
        let other = mint_id().unwrap();
        assert!(matches!(
            request(other.as_str(), "status"),
            Reply::NoAnswer(_)
        ));
        drop(service);
        assert!(matches!(
            request(id.as_str(), "status"),
            Reply::NoAnswer(_) | Reply::Gone
        ));
    }
}
