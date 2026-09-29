//! Cargo runner shim for isolated harness and rustdoc output.
//!
//! brokkr installs itself as cargo's host target runner and as rustdoc. Each
//! harness cargo launches is then this binary first: it connects to the run's
//! unix socket, receives the write end of a fresh pipe over `SCM_RIGHTS`, puts
//! it on fd 1 and execs the real program - same pid, same parent, no extra
//! process. brokkr drains every pipe into its own reconstructor and tracker, so
//! a harness that dies mid-test ends its own stream instead of leaving the one
//! stream every harness shares with an open suite that bills the next harness
//! for the dead test.
//!
//! The listener authenticates from the kernel, not from anything a test can
//! print: `SO_PEERCRED`'s pid, whose parent must be the root cargo and whose
//! executable must be this binary at connect time, once per process identity.
//! The threat model is accidents and misbehaving tests, not project code that
//! deliberately attacks this IPC. A connection that fails those checks gets a
//! passthrough verdict and runs unisolated: that is the doctest runtool (a
//! child of rustdoc, whose output rustdoc captures itself) and anything nested.
//!
//! A pidfd per harness decides liveness, so a dead harness stops being billed
//! even when a descendant inherited its stdout and holds the pipe open, while a
//! live test that merely closed stdout is still billed.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::symlink;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{
    Ceilings, HungTest, IDLE_TIMEOUT, JsonReconstructor, TestTracker, TimeoutReason, WATCHDOG_POLL,
};
use crate::error::DevError;

pub(super) struct Harness {
    pidfd: OwnedFd,
    tracker: Arc<Mutex<TestTracker>>,
    eof: Arc<AtomicBool>,
}

fn live_harness(harness: &Harness) -> bool {
    // EOF alone is not a process boundary: a live test can close stdout.
    !pidfd_exited(&harness.pidfd)
}

/// The environment the shim is driven by. Scrubbed from the program it execs,
/// so a test that runs cargo or rustdoc itself gets the toolchain's own, not
/// this run's shim.
const SOCKET_ENV: &str = "BROKKR_HARNESS_SOCKET";

/// The argv marker cargo's runner invocation carries.
pub(super) const RUNNER_ARG: &str = "__brokkr_harness_shim";

/// The name the rustdoc entry point is linked under; `argv[0]` selects it.
const RUSTDOC_LINK: &str = "rustdoc-shim";

/// `size_of::<RawFd>()` in the `u32` the `CMSG_*` macros take. Four bytes on
/// every target; the cast cannot truncate.
#[allow(clippy::cast_possible_truncation)]
const FD_SIZE: u32 = std::mem::size_of::<RawFd>() as u32;

/// `size_of::<ucred>()` as the `socklen_t` `getsockopt` takes; twelve bytes.
#[allow(clippy::cast_possible_truncation)]
const UCRED_SIZE: libc::socklen_t = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

/// The one-byte verdict the listener sends: a pipe rides along with ACCEPT.
const ACCEPT: u8 = 1;
const PASSTHROUGH: u8 = 0;

pub(super) struct Session {
    dir: PathBuf,
    listener: UnixListener,
    cargo_pid: Arc<AtomicU32>,
    harnesses: Arc<Mutex<Vec<Harness>>>,
    /// (pid, starttime) of every accepted peer: once per process identity,
    /// never refusing a recycled pid.
    seen: Arc<Mutex<HashSet<(u32, String)>>>,
    drains: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    done: Arc<AtomicBool>,
    watchdog_done: Arc<AtomicBool>,
    stdout: Arc<Mutex<Vec<u8>>>,
}

impl Session {
    pub(super) fn new(stdout: Arc<Mutex<Vec<u8>>>) -> Result<Self, DevError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Beside the other per-run state in ~/.brokkr, and short enough for a
        // unix socket path.
        let base = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".brokkr").join("harness"))
            .ok_or_else(|| DevError::Config("$HOME is not set; no place for the harness socket".into()))?;
        fs::create_dir_all(&base).map_err(DevError::Io)?;
        let dir = base.join(format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&dir).map_err(DevError::Io)?;
        symlink(
            std::env::current_exe().map_err(DevError::Io)?,
            dir.join(RUSTDOC_LINK),
        )
        .map_err(DevError::Io)?;
        let listener = UnixListener::bind(dir.join("socket")).map_err(DevError::Io)?;
        listener.set_nonblocking(true).map_err(DevError::Io)?;
        Ok(Self {
            dir,
            listener,
            cargo_pid: Arc::new(AtomicU32::new(0)),
            harnesses: Arc::new(Mutex::new(Vec::new())),
            seen: Arc::new(Mutex::new(HashSet::new())),
            drains: Arc::new(Mutex::new(Vec::new())),
            done: Arc::new(AtomicBool::new(false)),
            watchdog_done: Arc::new(AtomicBool::new(false)),
            stdout,
        })
    }

    pub(super) fn socket(&self) -> String {
        self.dir.join("socket").to_string_lossy().into_owned()
    }
    pub(super) fn rustdoc(&self) -> String {
        self.dir.join(RUSTDOC_LINK).to_string_lossy().into_owned()
    }
    pub(super) fn set_cargo_pid(&self, pid: u32) {
        self.cargo_pid.store(pid, Ordering::Release);
    }

    /// Stop accepting and draining, and wait for every drain to hand its
    /// block to the stdout buffer. A drain whose pipe a leaked descendant
    /// still holds notices `done` within one poll and flushes what it has, so
    /// the join is bounded.
    pub(super) fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.stop_watchdog();
        let drains = self.drains.lock().map(|mut d| std::mem::take(&mut *d)).unwrap_or_default();
        for d in drains {
            d.join().ok();
        }
    }

    pub(super) fn stop_watchdog(&self) {
        self.watchdog_done.store(true, Ordering::Release);
    }

    pub(super) fn settle(&self) {
        let until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < until {
            if self
                .harnesses
                .lock()
                .is_ok_and(|all| all.iter().all(|h| h.eof.load(Ordering::Acquire)))
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn run_acceptor(&self) -> Result<thread::JoinHandle<()>, DevError> {
        let listener = self.listener.try_clone().map_err(DevError::Io)?;
        let cargo_pid = Arc::clone(&self.cargo_pid);
        let harnesses = Arc::clone(&self.harnesses);
        let seen = Arc::clone(&self.seen);
        let drains = Arc::clone(&self.drains);
        let done = Arc::clone(&self.done);
        let stdout = Arc::clone(&self.stdout);
        Ok(thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = Shared {
                            cargo_pid: &cargo_pid,
                            seen: &seen,
                            harnesses: &harnesses,
                            drains: &drains,
                            stdout: &stdout,
                            done: &done,
                        };
                        // Refused peers run unisolated rather than failing:
                        // the doctest runtool is one, by design. Only a refusal
                        // of a peer that IS cargo's child is worth a word.
                        if let Err(refusal) = accept_one(&stream, &shared) {
                            send_verdict(&stream, PASSTHROUGH, None).ok();
                            if let Refusal::Unexpected(e) = refusal {
                                crate::output::warn(&format!(
                                    "a test harness could not be isolated and runs on the shared \
                                     stream: {e}"
                                ));
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        }))
    }

    pub(super) fn totals(&self) -> (Vec<(String, Duration)>, Vec<String>) {
        let mut completed = Vec::new();
        let mut in_flight = Vec::new();
        if let Ok(harnesses) = self.harnesses.lock() {
            for h in harnesses.iter() {
                if let Ok(mut t) = h.tracker.lock() {
                    completed.append(&mut t.completed);
                    in_flight.extend(t.current.keys().cloned());
                }
            }
        }
        in_flight.sort();
        (completed, in_flight)
    }

    pub(super) fn watchdog(
        &self,
        state_root: PathBuf,
        cargo_pid: u32,
        cargo_tracker: Arc<Mutex<TestTracker>>,
        hung: Arc<Mutex<Option<HungTest>>>,
        ceilings: &Ceilings,
        started: Instant,
    ) -> thread::JoinHandle<()> {
        let harnesses = Arc::clone(&self.harnesses);
        let done = Arc::clone(&self.watchdog_done);
        let (timeout, wall) = (ceilings.per_test, ceilings.wall);
        thread::spawn(move || {
            let mut idle_since = started;
            loop {
                if done.load(Ordering::Acquire) {
                    return;
                }
                let mut wait = WATCHDOG_POLL;
                let mut verdict = None;
                let mut executing = false;
                if let Ok(harnesses) = harnesses.lock() {
                    for h in harnesses.iter() {
                        if !live_harness(h) {
                            continue;
                        }
                        if let Ok(t) = h.tracker.lock() {
                            executing |= t.executing;
                            if let Some(d) = t.next_deadline(timeout) {
                                wait = wait.min(d);
                            }
                            // A tracker's own idle reading is not a verdict
                            // here: idleness is judged run-wide below, since
                            // the gap between harnesses belongs to no tracker.
                            if let Some((name, elapsed)) = t.timed_out(timeout)
                                && name != super::IDLE_WEDGE
                            {
                                verdict = Some((TimeoutReason::PerTest { name }, elapsed, timeout));
                                break;
                            }
                        }
                    }
                }
                // The shared stream: whatever ran unisolated (a passthrough).
                if verdict.is_none()
                    && let Ok(t) = cargo_tracker.lock()
                    && t.executing
                {
                    executing = true;
                    if let Some(d) = t.next_deadline(timeout) {
                        wait = wait.min(d);
                    }
                    if let Some((name, elapsed)) = t.timed_out(timeout) {
                        verdict = Some((TimeoutReason::PerTest { name }, elapsed, timeout));
                    }
                }
                if executing {
                    idle_since = Instant::now();
                }
                if verdict.is_none() && !executing && idle_since.elapsed() >= IDLE_TIMEOUT {
                    verdict = Some((TimeoutReason::Idle, idle_since.elapsed(), IDLE_TIMEOUT));
                }
                if verdict.is_none()
                    && let Some(wall) = wall.filter(|w| started.elapsed() >= *w)
                {
                    verdict = Some((
                        TimeoutReason::SweepWall { blamed: Vec::new() },
                        started.elapsed(),
                        wall,
                    ));
                }
                if let Some((reason, elapsed, ceiling)) = verdict {
                    if done.load(Ordering::Acquire) {
                        return;
                    }
                    super::stop_process_group(cargo_pid).ok();
                    let report =
                        super::capture_hung_test(&state_root, cargo_pid, reason, elapsed, ceiling);
                    if let Ok(mut slot) = hung.lock() {
                        *slot = Some(report);
                    }
                    super::kill_process_group(cargo_pid).ok();
                    return;
                }
                thread::sleep(wait);
            }
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.finish();
        fs::remove_dir_all(&self.dir).ok();
    }
}

/// The session state an accepted connection is registered into.
struct Shared<'a> {
    cargo_pid: &'a AtomicU32,
    seen: &'a Mutex<HashSet<(u32, String)>>,
    harnesses: &'a Mutex<Vec<Harness>>,
    drains: &'a Mutex<Vec<thread::JoinHandle<()>>>,
    stdout: &'a Arc<Mutex<Vec<u8>>>,
    done: &'a Arc<AtomicBool>,
}

/// Why a connection runs unisolated.
enum Refusal {
    /// Not cargo's direct child: the doctest runtool under rustdoc, or a nested
    /// run. Expected, and silent.
    NotCargoChild,
    /// Cargo's child, but the handshake failed. Worth saying.
    Unexpected(io::Error),
}

impl From<io::Error> for Refusal {
    fn from(e: io::Error) -> Self {
        Self::Unexpected(e)
    }
}

fn accept_one(stream: &UnixStream, s: &Shared<'_>) -> Result<(), Refusal> {
    // SO_PEERCRED supplies the kernel's PID at connect time. Check the direct
    // cargo parent and executable before giving this peer a pipe.
    let pid = peer_pid(stream)?;
    let root = s.cargo_pid.load(Ordering::Acquire);
    if root == 0 || proc_parent(pid)? != root {
        return Err(Refusal::NotCargoChild);
    }
    if fs::read_link(format!("/proc/{pid}/exe"))? != std::env::current_exe()? {
        return Err(Refusal::Unexpected(io::Error::other(
            "the runner is not this brokkr binary",
        )));
    }
    let pidfd = open_pidfd(pid)?;
    // A pidfd names this exact process, so PID reuse cannot retire or revive
    // its tracker. The second parent check closes the connect/open race.
    if pidfd_exited(&pidfd) || proc_parent(pid)? != root {
        return Err(Refusal::Unexpected(io::Error::other("runner exited before handshake")));
    }
    let identity = (
        pid,
        crate::lockfile::proc_starttime(pid).ok_or_else(|| io::Error::other("no starttime"))?,
    );
    let fresh = s
        .seen
        .lock()
        .map_err(|_| io::Error::other("seen mutex poisoned"))?
        .insert(identity);
    if !fresh {
        return Err(Refusal::Unexpected(io::Error::other("second connection from one runner")));
    }
    let (reader, writer) = pipe()?;
    send_verdict(stream, ACCEPT, Some(writer.as_raw_fd()))?;
    drop(writer);
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let eof = Arc::new(AtomicBool::new(false));
    s.harnesses
        .lock()
        .map_err(|_| io::Error::other("harness mutex poisoned"))?
        .push(Harness {
            pidfd,
            tracker: Arc::clone(&tracker),
            eof: Arc::clone(&eof),
        });
    let stdout = Arc::clone(s.stdout);
    let done = Arc::clone(s.done);
    let handle = thread::spawn(move || drain_harness(reader, &stdout, &tracker, &eof, &done));
    if let Ok(mut d) = s.drains.lock() {
        d.push(handle);
    }
    Ok(())
}

/// Drain one harness's pipe through its own reconstructor, and hand the
/// rendered text to the shared stdout buffer as ONE block when the stream ends.
///
/// One block, not line by line: harnesses run one after another, but a
/// lagging drain's tail could otherwise land after the next harness's
/// `running N tests`, and the parser resets its section state there - the
/// shape that used to drop failures from the roster.
fn drain_harness(
    reader: OwnedFd,
    stdout: &Mutex<Vec<u8>>,
    tracker: &Mutex<TestTracker>,
    eof: &AtomicBool,
    done: &AtomicBool,
) {
    let mut reader = fs::File::from(reader);
    let mut recon = JsonReconstructor::default();
    let mut pending = Vec::new();
    let mut block: Vec<String> = Vec::new();
    let mut bytes = [0_u8; 4096];
    while !done.load(Ordering::Acquire) {
        let mut pollfd = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for an fd we own, for the call's duration.
        if unsafe { libc::poll(&mut pollfd, 1, 100) } <= 0 {
            continue;
        }
        match reader.read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for &byte in &bytes[..n] {
                    if byte == b'\n' {
                        block.extend(render_line(&mut pending, &mut recon, tracker));
                    } else {
                        pending.push(byte);
                    }
                }
            }
        }
    }
    if !pending.is_empty() {
        block.extend(render_line(&mut pending, &mut recon, tracker));
    }
    block.extend(close_unfinished_suite(&mut recon, tracker));
    if let Ok(mut buf) = stdout.lock() {
        for line in block {
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
    }
    eof.store(true, Ordering::Release);
}

fn render_line(
    pending: &mut Vec<u8>,
    recon: &mut JsonReconstructor,
    tracker: &Mutex<TestTracker>,
) -> Vec<String> {
    if pending.last() == Some(&b'\r') {
        pending.pop();
    }
    let rendered = recon.observe(&String::from_utf8_lossy(pending), tracker);
    pending.clear();
    rendered
}

/// Close a suite this stream opened and never summarised: its harness died
/// mid-suite, and nothing more can arrive on the stream.
///
/// Without it the block ends with an open suite, the failures the harness DID
/// report before dying are never rendered (the reconstructor holds them for a
/// summary), and the whole run's completeness count goes inconsistent. The
/// closure renders those real failures and a FAILED summary. It deliberately
/// does not file the in-flight test as a failure: that name comes from the
/// harness's own output, so it is printed as a suspect line and nothing more.
/// Its passed count is zero - the dead suite's passes never reached a summary -
/// which only matters on a run that is red anyway.
fn close_unfinished_suite(recon: &mut JsonReconstructor, tracker: &Mutex<TestTracker>) -> Vec<String> {
    let Ok(t) = tracker.lock() else {
        return Vec::new();
    };
    if !t.in_suite {
        return Vec::new();
    }
    let mut suspects: Vec<String> = t.current.keys().cloned().collect();
    drop(t);
    suspects.sort();
    let mut out = Vec::new();
    if !suspects.is_empty() {
        out.push(format!(
            "brokkr: this test harness ended with its suite still open; last test seen starting: {} \
             (a suspect - named by the harness's own output)",
            suspects.join(", ")
        ));
    }
    let failed = recon.failures.len();
    let summary = serde_json::json!({ "passed": 0, "failed": failed, "ignored": 0 });
    out.extend(recon.render_suite_summary(&summary, "failed"));
    out
}

fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = UCRED_SIZE;
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if cred.uid != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runner uid differs",
        ));
    }
    u32::try_from(cred.pid).map_err(|_| io::Error::other("invalid peer pid"))
}

fn proc_parent(pid: u32) -> io::Result<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let rest = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("invalid proc stat"))?
        .1;
    rest.split_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::other("missing parent pid"))?
        .parse()
        .map_err(|_| io::Error::other("invalid parent pid"))
}

fn open_pidfd(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: a plain syscall with no pointer arguments.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.cast_signed(), 0_u32) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = RawFd::try_from(ret).map_err(|_| io::Error::other("pidfd out of range"))?;
    // SAFETY: a successful pidfd_open returns a fresh fd we uniquely own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn pidfd_exited(fd: &OwnedFd) -> bool {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pfd, 1, 0) > 0 }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
        OwnedFd::from_raw_fd(fds[1])
    }))
}

/// Send the one-byte verdict, with `fd` attached as `SCM_RIGHTS` when given.
fn send_verdict(stream: &UnixStream, verdict: u8, fd: Option<RawFd>) -> io::Result<()> {
    let Some(fd) = fd else {
        return (&*stream).write_all(&[verdict]);
    };
    let mut byte = [verdict];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    // cmsghdr needs native alignment; a byte array would make CMSG_DATA UB.
    let mut control = [0_usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(FD_SIZE) as usize };
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no fd control space"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(FD_SIZE) as usize;
        std::ptr::write(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Receive the verdict: the pipe to put on fd 1, or `None` to run unisolated.
fn recv_verdict(stream: &UnixStream) -> io::Result<Option<OwnedFd>> {
    let mut byte = [0_u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control = [0_usize; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control);
    if unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) } != 1 {
        return Err(io::Error::last_os_error());
    }
    if byte[0] == PASSTHROUGH {
        return Ok(None);
    }
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null()
        || unsafe {
            (*cmsg).cmsg_level != libc::SOL_SOCKET
                || (*cmsg).cmsg_type != libc::SCM_RIGHTS
                || (*cmsg).cmsg_len < libc::CMSG_LEN(FD_SIZE) as usize
        }
    {
        return Err(io::Error::other("missing stdout fd"));
    }
    let fd = unsafe { std::ptr::read(libc::CMSG_DATA(cmsg).cast::<RawFd>()) };
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Run as the shim when invoked as one: `argv[1]` is [`RUNNER_ARG`] (cargo's
/// runner) or `argv[0]` is the rustdoc link. `None` means this is an ordinary
/// brokkr invocation. Only returns at all when the exec failed.
pub(crate) fn maybe_run() -> Option<i32> {
    let mut args = std::env::args_os();
    let invoked = args.next()?;
    let command = if Path::new(&invoked)
        .file_name()
        .is_some_and(|n| n.to_str() == Some(RUSTDOC_LINK))
    {
        let mut all = vec!["rustdoc".into()];
        all.extend(args);
        all
    } else {
        if args.next()?.to_str()? != RUNNER_ARG {
            return None;
        }
        args.collect()
    };
    let error = run_shim(&command);
    eprintln!("brokkr harness shim: {error}");
    Some(1)
}

/// Obtain this harness's pipe if the run offers one, then exec the program.
///
/// Every way isolation can fail short of a handshake that returned a pipe -
/// no socket in the environment, a socket nobody answers, a passthrough
/// verdict - execs the program unisolated on the stdout it already has. That is
/// the doctest runtool (rustdoc captures its output) and any nested cargo.
fn run_shim(args: &[std::ffi::OsString]) -> io::Error {
    let Some((program, rest)) = args.split_first() else {
        return io::Error::other("missing harness program");
    };
    let pipe = std::env::var_os(SOCKET_ENV)
        .and_then(|socket| UnixStream::connect(Path::new(&socket)).ok())
        .and_then(|stream| recv_verdict(&stream).ok().flatten());
    if let Some(pipe) = pipe {
        // SAFETY: both fds are valid; dup2 onto fd 1 is the documented use.
        if unsafe { libc::dup2(pipe.as_raw_fd(), libc::STDOUT_FILENO) } < 0 {
            return io::Error::last_os_error();
        }
    }
    let mut cmd = Command::new(program);
    cmd.args(rest);
    // What steers cargo and rustdoc at this run's shim goes no further than
    // the program cargo meant to run: a test that runs cargo or rustdoc gets
    // the toolchain's own.
    cmd.env_remove(SOCKET_ENV).env_remove("RUSTDOC");
    for (key, value) in std::env::vars_os() {
        let is_runner = key.to_str().is_some_and(|k| k.starts_with("CARGO_TARGET_") && k.ends_with("_RUNNER"));
        if is_runner && value.to_string_lossy().contains(RUNNER_ARG) {
            cmd.env_remove(key);
        }
    }
    cmd.exec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_runner::TEST_TIMEOUT;

    #[test]
    fn session_has_distinct_socket_and_rustdoc_entrypoint() {
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let first = Session::new(Arc::clone(&stdout)).expect("first session");
        let second = Session::new(stdout).expect("second session");
        assert_ne!(first.socket(), second.socket());
        assert_eq!(
            fs::read_link(first.rustdoc()).expect("rustdoc symlink"),
            std::env::current_exe().expect("current exe")
        );
    }

    #[test]
    fn fd_handoff_preserves_pipe_eof() {
        let (a, b) = UnixStream::pair().expect("socket pair");
        let (reader, writer) = pipe().expect("pipe");
        let mut reader = fs::File::from(reader);
        send_verdict(&a, ACCEPT, Some(writer.as_raw_fd())).expect("send fd");
        let mut received =
            fs::File::from(recv_verdict(&b).expect("recv").expect("an accept carries a pipe"));
        drop(writer);
        received.write_all(b"ok").expect("write");
        drop(received);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read");
        assert_eq!(out, b"ok");
    }

    /// A refused peer is told to run unisolated, not left to fail: the doctest
    /// runtool under rustdoc is refused by design, and a hard failure there
    /// failed every doctest.
    #[test]
    fn a_passthrough_verdict_carries_no_pipe() {
        let (a, b) = UnixStream::pair().expect("socket pair");
        send_verdict(&a, PASSTHROUGH, None).expect("send");
        assert!(recv_verdict(&b).expect("recv").is_none());
    }

    /// A harness that dies mid-suite has its real failures rendered and its
    /// suite closed, and the in-flight test appears only as a suspect line -
    /// never on the failure roster.
    #[test]
    fn a_dead_suite_is_closed_with_its_real_failures_and_a_suspect() {
        let tracker = Mutex::new(TestTracker::default());
        let mut recon = JsonReconstructor::default();
        for ev in [
            r#"{"type":"suite","event":"started","test_count":2}"#,
            r#"{"type":"test","event":"started","name":"a::fails"}"#,
            r#"{"type":"test","name":"a::fails","event":"failed","stdout":"boom\n"}"#,
            r#"{"type":"test","event":"started","name":"a::aborts"}"#,
        ] {
            recon.observe(ev, &tracker);
        }
        let closing = close_unfinished_suite(&mut recon, &tracker);
        let text = closing.join("\n");
        assert!(text.contains("suspect") && text.contains("a::aborts"), "{text}");
        assert!(text.contains("    a::fails"), "the real failure is on the roster: {text}");
        assert!(!text.contains("    a::aborts"), "the suspect is not on the roster: {text}");
        assert!(closing.last().is_some_and(|l| l.starts_with("test result: FAILED.")), "{text}");

        // A suite that summarised is left alone.
        let closed = Mutex::new(TestTracker::default());
        let mut recon = JsonReconstructor::default();
        recon.observe(r#"{"type":"suite","event":"started","test_count":0}"#, &closed);
        recon.observe(
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.0}"#,
            &closed,
        );
        assert!(close_unfinished_suite(&mut recon, &closed).is_empty());
    }

    #[test]
    fn closing_stdout_does_not_retire_a_live_harness() {
        let mut healthy = TestTracker::default();
        healthy.observe_suite_start();
        healthy.observe_start("healthy".into());
        let healthy = Harness {
            pidfd: open_pidfd(std::process::id()).expect("pidfd"),
            tracker: Arc::new(Mutex::new(healthy)),
            eof: Arc::new(AtomicBool::new(false)),
        };
        assert!(live_harness(&healthy));
        healthy.eof.store(true, Ordering::Release);
        assert!(
            live_harness(&healthy),
            "closing stdout cannot retire a live test"
        );
        assert!(
            healthy
                .tracker
                .lock()
                .expect("tracker")
                .timed_out(TEST_TIMEOUT)
                .is_none()
        );
    }

    #[test]
    fn exited_harness_is_retired_even_if_stdout_is_held_open() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("child");
        let pidfd = open_pidfd(child.id()).expect("pidfd");
        child.wait().expect("reap child");
        let mut tracker = TestTracker::default();
        tracker.observe_suite_start();
        tracker.observe_start("crashed".into());
        tracker.last_progress = Instant::now() - TEST_TIMEOUT;
        let harness = Harness {
            pidfd,
            tracker: Arc::new(Mutex::new(tracker)),
            eof: Arc::new(AtomicBool::new(false)),
        };
        assert!(!live_harness(&harness));
        assert!(!harness.eof.load(Ordering::Acquire));
    }
}
