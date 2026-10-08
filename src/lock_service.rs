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
//!   no guard clears and which every acquisition and admission path checks) and
//!   then sends this process a `SIGTERM` for the handler's immediate effects.
//!   `accepted` means that commitment was made while this acquisition was live.
//!   It is not a promise of termination: a phase that neither polls the flag nor
//!   has a handler installed takes `SIGTERM`'s default action, exactly as a
//!   numeric `brokkr kill` always did there.
//!
//! Release never waits on the service. The service thread owns the listener and
//! holds no reference to the lock guard; release flips `live` (a mutex only ever
//! held for a check, an atomic store and one `kill(2)`), unlinks the path and
//! moves on. The thread notices within one poll and exits. Every connection is
//! served under absolute deadlines for the whole request and the whole reply.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::DevError;

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
    id: String,
    live: Mutex<bool>,
    identity: Identity,
    snapshot: Mutex<Snapshot>,
    started: Instant,
}

/// A running control socket, owned by one acquisition. Dropping it ends the
/// acquisition's service: no stop is accepted afterwards.
pub struct Service {
    shared: Arc<Shared>,
    path: PathBuf,
}

/// `~/.brokkr/lock`, created 0700.
fn socket_dir() -> Result<PathBuf, DevError> {
    let home = std::env::var("HOME")
        .map_err(|_| DevError::Lock("$HOME is not set - cannot place the lock socket".into()))?;
    let dir = PathBuf::from(home).join(".brokkr").join("lock");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Whether `id` has the exact shape `mint_id` produces: 32 lowercase hex
/// digits. Socket paths are only ever derived from an id that passes this,
/// never from path text read out of the lock file.
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A fresh public acquisition id. A separate draw from the capability nonce.
pub fn mint_id() -> Option<String> {
    crate::hold::mint_nonce()
}

fn socket_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.sock"))
}

/// Remove sockets left by acquisitions that are over. Called only while
/// holding the flock, which proves no other acquisition is live; strictly
/// limited to validated `<id>.sock` names that are sockets, never followed
/// through a symlink.
pub fn sweep_leftovers() {
    use std::os::unix::fs::FileTypeExt;
    let Ok(dir) = socket_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".sock")) else {
            continue;
        };
        if !valid_id(id) {
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
    pub fn start(id: &str, identity: Identity) -> Result<Self, DevError> {
        if !valid_id(id) {
            return Err(DevError::Lock("malformed lock acquisition id".into()));
        }
        let path = socket_path(&socket_dir()?, id);
        // `bind` refuses an existing path; a collision on a fresh 128-bit id is
        // a bug, not something to unlink over.
        let listener = UnixListener::bind(&path)
            .map_err(|e| DevError::Lock(format!("cannot bind lock socket {}: {e}", path.display())))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            id: id.to_owned(),
            live: Mutex::new(true),
            identity,
            snapshot: Mutex::new(Snapshot { draining: true, ..Snapshot::default() }),
            started: Instant::now(),
        });
        let serving = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name("brokkr-lock-service".into())
            .spawn(move || serve(&listener, &serving));
        if let Err(e) = spawned {
            std::fs::remove_file(&path).ok();
            return Err(DevError::Lock(format!("cannot start the lock service thread: {e}")));
        }
        Ok(Self { shared, path })
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
    *shared.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
    let reply = respond(&line, shared);
    write_all(stream, reply.as_bytes(), Instant::now() + IO_DEADLINE);
}

fn respond(line: &str, shared: &Shared) -> String {
    let (verb, id) = line.split_once(' ').unwrap_or((line, ""));
    if id != shared.id || !is_live(shared) {
        return "gone\n".into();
    }
    match verb {
        "status" => status_reply(shared),
        "stop" => stop_reply(shared),
        _ => "error unknown request\n".into(),
    }
}

fn stop_reply(shared: &Shared) -> String {
    let live = shared.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if !*live {
        return "gone\n".into();
    }
    crate::shutdown::commit_stop();
    // SAFETY: kill(2) on our own pid; no pointers.
    let rc = unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
    let signal_error = (rc != 0).then(io::Error::last_os_error);
    drop(live);
    match signal_error {
        None => "accepted\n".into(),
        Some(e) => format!("accepted signal_error={}\n", escape(&e.to_string())),
    }
}

fn status_reply(shared: &Shared) -> String {
    let Ok(snapshot) = shared.snapshot.try_lock().map(|s| *s) else {
        return "busy\n".into();
    };
    let id = &shared.identity;
    let (rss_kb, threads) = self_status();
    let mut out = String::from("ok\n");
    let mut field = |k: &str, v: &str| {
        out.push_str(k);
        out.push('=');
        out.push_str(&escape(v));
        out.push('\n');
    };
    field("project", &id.project);
    field("command", &id.command);
    field("args", &id.args);
    field("root", &id.root);
    field("pid_ns", &id.pid_ns);
    field("time_ns", &id.time_ns);
    field("codex_thread", &id.codex_thread);
    field("codex_session", &id.codex_session);
    field("held_secs", &shared.started.elapsed().as_secs().to_string());
    field("cpu_secs", &self_cpu_secs().map(|s| s.to_string()).unwrap_or_default());
    field("rss_kb", &rss_kb.map(|v| v.to_string()).unwrap_or_default());
    field("threads", &threads.map(|v| v.to_string()).unwrap_or_default());
    field("draining", if snapshot.draining { "1" } else { "0" });
    field(
        "progress",
        &snapshot.progress.map(|(r, t)| format!("{r}/{t}")).unwrap_or_default(),
    );
    field("child", if snapshot.has_child { "1" } else { "0" });
    field("mocks", &snapshot.mocks.to_string());
    out
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

/// User plus system CPU seconds of this process (`utime` + `stime`).
fn self_cpu_secs() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let post: Vec<&str> = stat.get(stat.rfind(')')? + 2..)?.split_whitespace().collect();
    let utime: u64 = post.get(11)?.parse().ok()?;
    let stime: u64 = post.get(12)?.parse().ok()?;
    // SAFETY: sysconf takes no pointers.
    let tck = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    (tck > 0).then(|| (utime + stime) / tck)
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

/// Percent-escape `%`, `\n` and `\r`, the bytes that would break the
/// line-oriented reply.
fn escape(s: &str) -> String {
    s.replace('%', "%25").replace('\n', "%0A").replace('\r', "%0D")
}

fn unescape(s: &str) -> String {
    s.replace("%0A", "\n").replace("%0D", "\r").replace("%25", "%")
}

// ---------------------------------------------------------------------------
// Client side.
// ---------------------------------------------------------------------------

/// The holder's answer to `status`.
pub struct HolderStatus {
    fields: Vec<(String, String)>,
}

impl HolderStatus {
    pub fn get(&self, key: &str) -> &str {
        self.fields.iter().find(|(k, _)| k == key).map_or("", |(_, v)| v.as_str())
    }
}

/// What came back from the holder.
pub enum Reply {
    Status(HolderStatus),
    /// A stop was committed. `signal_error` is set when the holder's
    /// self-`SIGTERM` failed; the sticky stop flag is set regardless.
    Accepted { signal_error: Option<String> },
    /// The id is not this holder's live acquisition (it released, or the
    /// metadata is stale).
    Gone,
    /// The holder was mid-update; ask again.
    Busy,
    /// No answer: the socket is missing, refused, or the holder did not reply
    /// in time (a SIGSTOPped holder, say). Says nothing about liveness - the
    /// flock does.
    NoAnswer(String),
}

/// Send one request to the holder of acquisition `id`.
pub fn request(id: &str, verb: &str) -> Reply {
    if !valid_id(id) {
        return Reply::NoAnswer("the lock file names no valid acquisition id".into());
    }
    let dir = match socket_dir() {
        Ok(d) => d,
        Err(e) => return Reply::NoAnswer(e.to_string()),
    };
    let path = socket_path(&dir, id);
    let stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => return Reply::NoAnswer(format!("cannot reach the holder's socket: {e}")),
    };
    let deadline = Instant::now() + CLIENT_DEADLINE;
    if !write_all(&stream, format!("{verb} {id}\n").as_bytes(), deadline) {
        return Reply::NoAnswer("the holder did not take the request in time".into());
    }
    let Some(text) = read_reply(&stream, deadline) else {
        // For a stop, a lost reply means the outcome is unknown, not refused.
        return Reply::NoAnswer("no reply from the holder (outcome unknown)".into());
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
        "ok" => Reply::Status(HolderStatus {
            fields: lines
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_owned(), unescape(v)))
                .collect(),
        }),
        "accepted" => Reply::Accepted {
            signal_error: head
                .split_once("signal_error=")
                .map(|(_, e)| unescape(e)),
        },
        "gone" => Reply::Gone,
        "busy" => Reply::Busy,
        other => Reply::NoAnswer(format!("unexpected reply from the holder: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ids_are_validated_strictly() {
        assert!(valid_id(&mint_id().unwrap()));
        assert!(!valid_id(""));
        assert!(!valid_id("../../etc/passwd"));
        assert!(!valid_id(&"A".repeat(32)));
        assert!(!valid_id(&"a".repeat(31)));
    }

    #[test]
    fn replies_round_trip() {
        let Reply::Status(s) = parse_reply("ok\nargs=a%0Ab%25\nmocks=2\n") else {
            panic!("status");
        };
        assert_eq!(s.get("args"), "a\nb%");
        assert_eq!(s.get("mocks"), "2");
        assert_eq!(s.get("absent"), "");
        assert!(matches!(parse_reply("accepted\n"), Reply::Accepted { signal_error: None }));
        assert!(matches!(
            parse_reply("accepted signal_error=EPERM\n"),
            Reply::Accepted { signal_error: Some(e) } if e == "EPERM"
        ));
        assert!(matches!(parse_reply("gone\n"), Reply::Gone));
        assert!(matches!(parse_reply("busy\n"), Reply::Busy));
        assert!(matches!(parse_reply("nonsense\n"), Reply::NoAnswer(_)));
    }

    /// A status request is answered over a real socket, a request naming
    /// another acquisition is told `gone`, and once the service drops the
    /// same id is no longer reachable. Uses the real socket dir; the id is
    /// fresh, so it cannot collide with a live hold's.
    #[test]
    fn service_answers_status_and_stops_answering_after_drop() {
        let id = mint_id().unwrap();
        let identity = Identity { project: "p".into(), args: "x y".into(), ..Identity::default() };
        let service = Service::start(&id, identity).unwrap();
        service.update(Snapshot { draining: false, progress: Some((2, 5)), has_child: true, mocks: 1 });
        let Reply::Status(s) = request(&id, "status") else { panic!("status") };
        assert_eq!(s.get("project"), "p");
        assert_eq!(s.get("args"), "x y");
        assert_eq!(s.get("progress"), "2/5");
        assert_eq!(s.get("draining"), "0");
        let other = mint_id().unwrap();
        assert!(matches!(request(&other, "status"), Reply::NoAnswer(_)));
        drop(service);
        assert!(matches!(request(&id, "status"), Reply::NoAnswer(_) | Reply::Gone));
    }
}
