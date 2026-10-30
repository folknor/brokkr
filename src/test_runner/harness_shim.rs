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
//!
//! Before it gets a pipe, the shim says what it is: a one-message handshake
//! carrying its role (harness or rustdoc) and, for a harness, the executable
//! cargo asked it to run. The parent attributes the stream to that executable,
//! since the runner argv is the only place cargo states which test binary a
//! harness process is. A strict session (a `certifies = "complete"` gate)
//! refuses an executable it did not plan, and refuses any cargo child it cannot
//! authenticate, instead of letting it run on the shared stream. The shim fails
//! closed on a refusal: it exits rather than exec'ing a harness no one can
//! account for.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
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
    AttributionRefusal, BuildGate, Ceilings, Doorbell, GATE_PENDING_BOUND, GateState, HungTest,
    JsonReconstructor, ObsEvent, ObservationSink, RunContext, StreamEnd, StreamSource, StreamTap,
    StrictPlan, TestTracker, WATCHDOG_POLL, hash_file,
};
use crate::error::DevError;
use crate::process::{Pid, ProcStat, VerifiedProcess};

pub(super) struct Harness {
    pidfd: OwnedFd,
    tracker: Arc<Mutex<TestTracker>>,
    eof: Arc<AtomicBool>,
    /// This harness's stream on the caller's sink, when one was given: where a
    /// kill the watchdog charges to this harness is reported.
    tap: Option<StreamTap>,
}

fn live_harness(harness: &Harness) -> bool {
    // EOF alone is not a process boundary: a live test can close stdout.
    !pidfd_exited(&harness.pidfd)
}

/// The environment the shim is driven by. Scrubbed from the program it execs,
/// so a test that runs cargo or rustdoc itself gets the toolchain's own, not
/// this run's shim.
const SOCKET_ENV: &str = "BROKKR_HARNESS_SOCKET";

/// Set on a strict session: the shim then fails closed when it cannot get a
/// pipe, rather than running its program unisolated.
pub(super) const STRICT_ENV: &str = "BROKKR_HARNESS_STRICT";

/// The argv marker cargo's runner invocation carries.
pub(super) const RUNNER_ARG: &str = "__brokkr_harness_shim";

/// The handshake's role byte: a test harness cargo launched as its runner, or
/// the rustdoc entry point.
const ROLE_HARNESS: u8 = b'H';
const ROLE_RUSTDOC: u8 = b'D';

/// How long the listener waits for a connected shim's handshake. The shim
/// writes it before it reads anything, so a peer that sends nothing is not one.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// The name the rustdoc entry point is linked under; `argv[0]` selects it.
const RUSTDOC_LINK: &str = "rustdoc-shim";

/// This process's executable as cargo should exec it: `/proc/<pid>/exe`, not
/// `current_exe()`. Cargo splits a string-form runner on whitespace, and
/// `current_exe()` reads `<path> (deleted)` once the binary is replaced under a
/// running brokkr (a `brokkr install` from another session) - whitespace, and
/// a path that no longer exists. The magic link has neither problem: it never
/// contains whitespace and still execs the replaced inode. Children share
/// brokkr's PID namespace, so the pid resolves for them too.
pub(crate) fn own_exe() -> PathBuf {
    PathBuf::from(format!("/proc/{}/exe", std::process::id()))
}

/// Whether `pid` runs the same executable inode as this process. Compared by
/// device and inode rather than by link text, which reads `<path> (deleted)`
/// once the binary is replaced and so says nothing reliable about identity.
fn same_exe(pid: u32) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let peer = fs::metadata(format!("/proc/{pid}/exe"))?;
    let own = fs::metadata("/proc/self/exe")?;
    Ok(peer.dev() == own.dev() && peer.ino() == own.ino())
}

/// `size_of::<RawFd>()` in the `u32` the `CMSG_*` macros take. Four bytes on
/// every target; the cast cannot truncate.
#[allow(clippy::cast_possible_truncation)]
const FD_SIZE: u32 = std::mem::size_of::<RawFd>() as u32;

/// `size_of::<ucred>()` as the `socklen_t` `getsockopt` takes; twelve bytes.
#[allow(clippy::cast_possible_truncation)]
const UCRED_SIZE: libc::socklen_t = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

/// The one-byte verdict the listener sends: a pipe rides along with ACCEPT.
/// PASSTHROUGH sanctions running unisolated (the doctest runtool, anything
/// nested); REFUSE, sent only by a strict session, tells the shim to exit.
const ACCEPT: u8 = 1;
const PASSTHROUGH: u8 = 0;
const REFUSE: u8 = 2;
/// A derived doctest carrier's rustdoc for a library whose manifest sets
/// `doctest = false`: exit successfully without running it.
const DECLINE: u8 = 3;

/// State shared by the session owner, acceptor, watchdog, and drains.
struct SessionState {
    cargo_pid: AtomicU32,
    harnesses: Mutex<Vec<Harness>>,
    /// (pid, starttime) of every accepted peer: once per process identity,
    /// never refusing a recycled pid.
    seen: Mutex<HashSet<(u32, String)>>,
    drains: Mutex<Vec<thread::JoinHandle<()>>>,
    accepting: AtomicBool,
    drain_cancel: AtomicBool,
    watchdog_done: Doorbell,
    stdout: Arc<Mutex<Vec<u8>>>,
    sink: Option<ObservationSink>,
    context: RunContext,
    /// `Some` on a strict session: the harness executables (and content) this
    /// run may attribute, and whether a rustdoc may connect. Anything else,
    /// and any cargo child that fails the handshake, is refused.
    strict: Option<StrictPlan>,
    /// Set when the caller holds the run's own build to a plan: every
    /// connection waits for that comparison before it is answered. A strict
    /// session admits only on [`GateState::Clean`]; a permissive one admits
    /// whatever the verdict, having only waited so no test can touch the
    /// compared files before they are hashed.
    gate: Option<Arc<BuildGate>>,
}

pub(super) struct Session {
    dir: PathBuf,
    listener: UnixListener,
    state: Arc<SessionState>,
}

impl Session {
    pub(super) fn new(
        stdout: Arc<Mutex<Vec<u8>>>,
        sink: Option<ObservationSink>,
        strict: Option<StrictPlan>,
        gate: Option<Arc<BuildGate>>,
        context: RunContext,
    ) -> Result<Self, DevError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Beside the other per-run state in ~/.brokkr, and short enough for a
        // unix socket path.
        let base = crate::user_dirs::harness_dir().ok_or_else(|| {
            DevError::Config("$HOME is not set or is empty; no place for the harness socket".into())
        })?;
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
        symlink(own_exe(), dir.join(RUSTDOC_LINK)).map_err(DevError::Io)?;
        let listener = UnixListener::bind(dir.join("socket")).map_err(DevError::Io)?;
        listener.set_nonblocking(true).map_err(DevError::Io)?;
        Ok(Self {
            dir,
            listener,
            state: Arc::new(SessionState {
                cargo_pid: AtomicU32::new(0),
                harnesses: Mutex::new(Vec::new()),
                seen: Mutex::new(HashSet::new()),
                drains: Mutex::new(Vec::new()),
                accepting: AtomicBool::new(true),
                drain_cancel: AtomicBool::new(false),
                watchdog_done: Doorbell::default(),
                stdout,
                sink,
                context,
                strict,
                gate,
            }),
        })
    }

    pub(super) fn socket(&self) -> String {
        self.dir.join("socket").to_string_lossy().into_owned()
    }
    pub(super) fn rustdoc(&self) -> String {
        self.dir.join(RUSTDOC_LINK).to_string_lossy().into_owned()
    }
    pub(super) fn set_cargo_pid(&self, pid: u32) {
        self.state.cargo_pid.store(pid, Ordering::Release);
    }

    /// Stop accepting and give every output drain the shared grace interval
    /// to reach EOF. The common join helper cancels and joins any drain whose
    /// pipe a leaked descendant still holds open.
    pub(super) fn finish(&self) {
        self.state.accepting.store(false, Ordering::Release);
        self.stop_watchdog();
        let drains = match self.state.drains.lock() {
            Ok(mut drains) => std::mem::take(&mut *drains),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        super::procgroup::join_drains(drains, &self.state.drain_cancel, &self.state.context);
    }

    pub(super) fn stop_watchdog(&self) {
        self.state.watchdog_done.ring();
    }

    pub(super) fn settle(&self) {
        let until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < until {
            if self
                .state
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
        let state = Arc::clone(&self.state);
        Ok(thread::spawn(move || {
            while state.accepting.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(refusal) = accept_one(&stream, &state) {
                            refuse(&stream, &refusal, &state);
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
        if let Ok(harnesses) = self.state.harnesses.lock() {
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn watchdog(
        &self,
        state_root: PathBuf,
        cargo_pid: u32,
        cargo_tracker: Arc<Mutex<TestTracker>>,
        hung: Arc<Mutex<Option<HungTest>>>,
        ceilings: &Ceilings,
        started: Instant,
        primary: Option<StreamTap>,
    ) -> thread::JoinHandle<()> {
        let state = Arc::clone(&self.state);
        let context = self.state.context.clone();
        let ceilings = ceilings.clone();
        let timeout = ceilings.per_test;
        thread::spawn(move || {
            let mut idle_since = started;
            loop {
                if state.watchdog_done.is_rung() {
                    return;
                }
                let mut wait = WATCHDOG_POLL;
                let mut readings = Vec::new();
                let mut taps = Vec::new();
                let mut executing = false;
                if let Ok(harnesses) = state.harnesses.lock() {
                    for h in harnesses.iter() {
                        if !live_harness(h) {
                            continue;
                        }
                        if let Ok(t) = h.tracker.lock() {
                            executing |= t.executing;
                            if let Some(d) = t.next_deadline(timeout) {
                                wait = wait.min(d);
                            }
                            readings.push(super::DeadlineReading::from_tracker(&t, timeout));
                            taps.push(h.tap.clone());
                        }
                    }
                }
                // The shared stream covers unisolated passthroughs. Its idle
                // reading cannot own the gap between successive harnesses.
                if let Ok(t) = cargo_tracker.lock()
                    && t.executing
                {
                    executing = true;
                    if let Some(d) = t.next_deadline(timeout) {
                        wait = wait.min(d);
                    }
                    readings.push(super::DeadlineReading::from_tracker(&t, timeout));
                    taps.push(primary.clone());
                }
                if executing {
                    idle_since = Instant::now();
                }
                let idle = (!executing).then(|| idle_since.elapsed());
                if let Some(verdict) = ceilings.verdict(&readings, idle, started.elapsed()) {
                    if state.watchdog_done.is_rung() {
                        return;
                    }
                    let charged = verdict
                        .charged
                        .and_then(|index| taps.get(index))
                        .and_then(Option::as_ref)
                        .or(primary.as_ref());
                    super::enforce_deadline(
                        &state_root,
                        cargo_pid,
                        super::Leader::Cargo,
                        verdict,
                        &hung,
                        charged,
                        &context,
                    );
                    return;
                }
                if let Some(wall) = ceilings.wall {
                    wait = wait.min(wall.saturating_sub(started.elapsed()));
                }
                if let Some(idle) = idle {
                    wait = wait.min(super::IDLE_TIMEOUT.saturating_sub(idle));
                }
                if state.watchdog_done.sleep(wait) {
                    return;
                }
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

/// Why a connection does not get a pipe.
enum Refusal {
    /// Not cargo's direct child: the doctest runtool under rustdoc, or a nested
    /// run. Expected, and silent - and sanctioned to run unisolated even on a
    /// strict session, since its output is captured by its own parent.
    NotCargoChild,
    HandshakeIo(io::Error),
    AuthenticationIo(io::Error),
    SessionIo(io::Error),
    UnplannedExecutable {
        executable: String,
    },
    RebuiltExecutable {
        executable: String,
    },
    /// A strict session's build did not verify against the plan.
    UnverifiedBuild {
        detail: String,
    },
    RustdocWithoutCarrier,
    UnidentifiedRustdoc {
        package_dir: String,
    },
    UnknownRole {
        role: u8,
    },
    ExecutableHashFailed {
        executable: String,
        detail: String,
    },
    RunnerMismatch,
    RunnerExitedDuringHandshake,
    ProcessIdentityUnavailable,
    DuplicateConnection,
    SessionClosing,
    StatePoisoned(&'static str),
}

impl Refusal {
    fn observation(&self) -> Option<AttributionRefusal> {
        match self {
            Self::NotCargoChild => None,
            Self::HandshakeIo(error) => Some(AttributionRefusal::HandshakeIo {
                detail: error.to_string(),
            }),
            Self::AuthenticationIo(error) => Some(AttributionRefusal::AuthenticationIo {
                detail: error.to_string(),
            }),
            Self::SessionIo(error) => Some(AttributionRefusal::SessionIo {
                detail: error.to_string(),
            }),
            Self::UnplannedExecutable { executable } => {
                Some(AttributionRefusal::UnplannedExecutable {
                    executable: executable.clone(),
                })
            }
            Self::RebuiltExecutable { executable } => Some(AttributionRefusal::RebuiltExecutable {
                executable: executable.clone(),
            }),
            Self::UnverifiedBuild { detail } => Some(AttributionRefusal::UnverifiedBuild {
                detail: detail.clone(),
            }),
            Self::RustdocWithoutCarrier => Some(AttributionRefusal::RustdocWithoutCarrier),
            Self::UnidentifiedRustdoc { package_dir } => {
                Some(AttributionRefusal::UnidentifiedRustdoc {
                    package_dir: package_dir.clone(),
                })
            }
            Self::UnknownRole { role } => Some(AttributionRefusal::UnknownRole { role: *role }),
            Self::ExecutableHashFailed { executable, detail } => {
                Some(AttributionRefusal::ExecutableHashFailed {
                    executable: executable.clone(),
                    detail: detail.clone(),
                })
            }
            Self::RunnerMismatch => Some(AttributionRefusal::RunnerMismatch),
            Self::RunnerExitedDuringHandshake => {
                Some(AttributionRefusal::RunnerExitedDuringHandshake)
            }
            Self::ProcessIdentityUnavailable => {
                Some(AttributionRefusal::ProcessIdentityUnavailable)
            }
            Self::DuplicateConnection => Some(AttributionRefusal::DuplicateConnection),
            Self::SessionClosing => Some(AttributionRefusal::SessionClosing),
            Self::StatePoisoned(name) => Some(AttributionRefusal::StatePoisoned {
                name: (*name).into(),
            }),
        }
    }

    fn detail(&self) -> String {
        self.observation()
            .map_or_else(String::new, |refusal| refusal.detail())
    }
}

/// Answer a connection that gets no pipe. A permissive session lets it run on
/// the shared stream, as it always has; a strict one refuses it (the shim then
/// exits) and records why, because a harness that cannot be attributed cannot
/// be accounted for.
fn refuse(stream: &UnixStream, refusal: &Refusal, s: &SessionState) {
    if matches!(refusal, Refusal::NotCargoChild) {
        send_verdict(stream, PASSTHROUGH, None).ok();
        return;
    }
    let detail = refusal.detail();
    if s.strict.is_some() {
        send_verdict(stream, REFUSE, None).ok();
    } else {
        send_verdict(stream, PASSTHROUGH, None).ok();
        s.context.warn(&format!(
            "a test harness could not be isolated and runs on the shared stream: {detail}"
        ));
    }
    if let Some(sink) = s.sink.as_ref()
        && let Some(refusal) = refusal.observation()
    {
        StreamTap::new(sink, StreamSource::Process).emit(ObsEvent::AttributionRefused { refusal });
    }
}

/// A derived carrier admits rustdoc per library, by the package directory the
/// handshake names: `Some(package)` when that library's manifest opts out of
/// doctests and it is to be declined, `None` to run it. A directory the table
/// does not hold - or none at all - is refused: only a positively identified
/// library may be either run or declined. Sessions without a table (every
/// other lane) admit any rustdoc the carrier flag allows.
fn rustdoc_declined(s: &SessionState, package_dir: String) -> Result<Option<String>, Refusal> {
    let Some(table) = s
        .strict
        .as_ref()
        .and_then(|plan| plan.rustdoc_admission.as_ref())
    else {
        return Ok(None);
    };
    let library = (!package_dir.is_empty())
        .then(|| table.get(&super::canonical_package_dir(Path::new(&package_dir))))
        .flatten();
    match library {
        None => Err(Refusal::UnidentifiedRustdoc { package_dir }),
        Some(lib) if !lib.doctested => Ok(Some(lib.package.clone())),
        Some(_) => Ok(None),
    }
}

/// Wait for the session's build verdict, when the caller holds the build to a
/// plan. A strict session admits only a clean build; a permissive one admits
/// whatever the verdict - the caller records what it means for the lane's
/// inventory - having waited only so nothing runs before the compared files
/// were hashed. A closing session or a stop ends the wait as a refusal.
fn await_verified_build(s: &SessionState) -> Result<(), Refusal> {
    let Some(gate) = &s.gate else {
        return Ok(());
    };
    let closed = || !s.accepting.load(Ordering::Acquire) || s.context.stop.is_requested();
    match gate.wait(GATE_PENDING_BOUND, closed) {
        None => Err(Refusal::SessionClosing),
        Some(GateState::Clean) => Ok(()),
        Some(state) if s.strict.is_some() => Err(Refusal::UnverifiedBuild {
            detail: state.describe(),
        }),
        Some(_) => Ok(()),
    }
}

fn accept_one(stream: &UnixStream, s: &Arc<SessionState>) -> Result<(), Refusal> {
    // SO_PEERCRED supplies the kernel's PID at connect time. Check the direct
    // cargo parent and executable before giving this peer a pipe.
    let pid = peer_pid(stream).map_err(Refusal::AuthenticationIo)?;
    // The shim writes its handshake before reading anything, so reading it
    // first costs nothing and leaves no peer waiting on an unread message.
    let (role, executable) = recv_hello(stream).map_err(Refusal::HandshakeIo)?;
    let root = s.cargo_pid.load(Ordering::Acquire);
    if root == 0 || proc_parent(pid).map_err(Refusal::AuthenticationIo)? != root {
        return Err(Refusal::NotCargoChild);
    }
    // Nothing cargo runs is answered before its build has been held to the
    // plan. Every check below - the strict hash, the peer's liveness - runs
    // after this wait, however long it took.
    await_verified_build(s)?;
    // The library a derived carrier's rustdoc is for, when its manifest opts
    // out of doctests: decided here, acted on once the peer is authenticated.
    let mut declined: Option<String> = None;
    let source = match role {
        ROLE_HARNESS => {
            if let Some(plan) = &s.strict {
                let Some(planned) = plan.known.get(&executable) else {
                    return Err(Refusal::UnplannedExecutable { executable });
                };
                // The last check before this harness runs: the file cargo is
                // about to exec is the one the plan enumerated, byte for
                // byte. A path match alone admits whatever a later build
                // wrote there.
                match hash_file(Path::new(&executable)) {
                    Ok(now) if &now == planned => {}
                    Ok(_) => {
                        return Err(Refusal::RebuiltExecutable { executable });
                    }
                    Err(e) => {
                        return Err(Refusal::ExecutableHashFailed {
                            executable,
                            detail: e.to_string(),
                        });
                    }
                }
            }
            StreamSource::Harness { executable }
        }
        ROLE_RUSTDOC => {
            if s.strict.as_ref().is_some_and(|plan| !plan.rustdoc) {
                return Err(Refusal::RustdocWithoutCarrier);
            }
            declined = rustdoc_declined(s, executable)?;
            StreamSource::Rustdoc
        }
        role => return Err(Refusal::UnknownRole { role }),
    };
    if !same_exe(pid).map_err(Refusal::AuthenticationIo)? {
        return Err(Refusal::RunnerMismatch);
    }
    // Bind the peer's current process generation to a pidfd: the identity is
    // re-read around `pidfd_open`, and from then on the pidfd names this exact
    // process, so PID reuse cannot retire or revive its tracker. The second
    // parent check closes the connect/open race.
    let stat = Pid::new(pid)
        .and_then(|p| ProcStat::read(p).ok())
        .ok_or(Refusal::ProcessIdentityUnavailable)?;
    let pidfd = VerifiedProcess::open(stat.identity(), None)
        .map_err(|_| Refusal::RunnerExitedDuringHandshake)?
        .into_pidfd();
    if pidfd_exited(&pidfd) || proc_parent(pid).map_err(Refusal::AuthenticationIo)? != root {
        return Err(Refusal::RunnerExitedDuringHandshake);
    }
    let identity = (pid, stat.start().get().to_string());
    let fresh = s
        .seen
        .lock()
        .map_err(|_| Refusal::StatePoisoned("peer identity"))?
        .insert(identity);
    if !fresh {
        return Err(Refusal::DuplicateConnection);
    }
    // An authenticated rustdoc for an opted-out library: recorded first, then
    // told to exit successfully without running. A decline whose record could
    // not be written is never acknowledged - the shim then fails closed.
    if let Some(package) = declined {
        if !s.accepting.load(Ordering::Acquire) {
            return Err(Refusal::SessionClosing);
        }
        if let Some(sink) = s.sink.as_ref() {
            StreamTap::new(sink, StreamSource::Rustdoc).emit(ObsEvent::DoctestDeclined { package });
        }
        return send_verdict(stream, DECLINE, None).map_err(Refusal::SessionIo);
    }
    // The drain list is the admission barrier with `finish`: if this
    // connection began before shutdown, finish will collect its thread; after
    // shutdown begins, no untracked harness can receive a pipe.
    let mut drains = s
        .drains
        .lock()
        .map_err(|_| Refusal::StatePoisoned("drain list"))?;
    let mut harnesses = s
        .harnesses
        .lock()
        .map_err(|_| Refusal::StatePoisoned("harness list"))?;
    // A strict session admits nothing once a stop is requested, even after a
    // clean verdict: the run is ending and certifies nothing past this point.
    if !s.accepting.load(Ordering::Acquire) || (s.strict.is_some() && s.context.stop.is_requested())
    {
        return Err(Refusal::SessionClosing);
    }
    let (reader, writer) = pipe().map_err(Refusal::SessionIo)?;
    send_verdict(stream, ACCEPT, Some(writer.as_raw_fd())).map_err(Refusal::SessionIo)?;
    drop(writer);
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let eof = Arc::new(AtomicBool::new(false));
    let tap = s.sink.as_ref().map(|sink| StreamTap::new(sink, source));
    harnesses.push(Harness {
        pidfd,
        tracker: Arc::clone(&tracker),
        eof: Arc::clone(&eof),
        tap: tap.clone(),
    });
    let state = Arc::clone(s);
    let handle = thread::spawn(move || drain_harness(reader, &state, &tracker, &eof, tap));
    drains.push(handle);
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
    state: &SessionState,
    tracker: &Mutex<TestTracker>,
    eof: &AtomicBool,
    tap: Option<StreamTap>,
) {
    let mut reader = fs::File::from(reader);
    let mut recon = JsonReconstructor {
        tap,
        ..JsonReconstructor::default()
    };
    let mut pending = Vec::new();
    let mut block: Vec<String> = Vec::new();
    let mut bytes = [0_u8; 4096];
    let end = loop {
        match super::read_chunk(&mut reader, &mut bytes, &state.drain_cancel) {
            super::Chunk::Data(n) => {
                for &byte in &bytes[..n] {
                    if byte == b'\n' {
                        block.extend(render_line(&mut pending, &mut recon, tracker));
                    } else {
                        pending.push(byte);
                    }
                }
            }
            super::Chunk::Eof => break StreamEnd::Eof,
            super::Chunk::Error => break StreamEnd::ReadError,
            // Drain cancellation while the pipe is still open: something left
            // behind holds it, and whatever it has not written yet is lost.
            super::Chunk::Cancelled => break StreamEnd::Cancelled,
        }
    };
    if !pending.is_empty() {
        block.extend(render_line(&mut pending, &mut recon, tracker));
    }
    // Display only: the closure is synthesized text, so it goes to the
    // rendered block and never to the sink - the reconstructor reports only
    // records it parsed, and this one is detached from the tap.
    let tap = recon.tap.take();
    block.extend(close_unfinished_suite(&mut recon, tracker));
    if let Ok(mut buf) = state.stdout.lock() {
        for line in block {
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
    }
    if let Some(tap) = &tap {
        tap.emit(ObsEvent::StreamEnded { end });
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
fn close_unfinished_suite(
    recon: &mut JsonReconstructor,
    tracker: &Mutex<TestTracker>,
) -> Vec<String> {
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
    let pid = Pid::new(pid).ok_or_else(|| io::Error::other("invalid pid"))?;
    ProcStat::read(pid)?
        .parent()
        .map(Pid::get)
        .ok_or_else(|| io::Error::other("missing parent pid"))
}

/// An unverified pidfd for a process the caller itself owns: its own child,
/// not yet reaped, so the pid cannot have been recycled.
pub(super) fn open_pidfd(pid: u32) -> io::Result<OwnedFd> {
    crate::process::pidfd::open_own(pid)
}

fn pidfd_exited(fd: &OwnedFd) -> bool {
    pidfd_wait(fd, Duration::ZERO)
}

/// Wait up to `wait` for the process behind `fd` to exit; true once it has.
/// A pidfd turns readable at exit, so the waiter wakes then, not a poll later.
pub(super) fn pidfd_wait(fd: &OwnedFd, wait: Duration) -> bool {
    matches!(
        crate::process::pidfd::wait(fd, wait),
        crate::process::pidfd::PidfdWait::Exited
    )
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

/// The shim's handshake: its role, then the executable it is about to run
/// (length-prefixed, raw bytes - a path need not be UTF-8 on the wire, though
/// the parent attributes only paths that are).
fn send_hello(stream: &UnixStream, role: u8, executable: &[u8]) -> io::Result<()> {
    let len = u32::try_from(executable.len())
        .map_err(|_| io::Error::other("executable path too long"))?;
    let mut message = Vec::with_capacity(5 + executable.len());
    message.push(role);
    message.extend_from_slice(&len.to_le_bytes());
    message.extend_from_slice(executable);
    (&*stream).write_all(&message)
}

/// Read a shim's handshake, bounded in time and size: a peer that sends
/// nothing, or a length no path has, is not a shim.
fn recv_hello(stream: &UnixStream) -> io::Result<(u8, String)> {
    stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
    let mut head = [0_u8; 5];
    (&*stream).read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
    if len > 64 * 1024 {
        return Err(io::Error::other(format!(
            "handshake claims a {len}-byte path"
        )));
    }
    let mut path = vec![0_u8; len as usize];
    (&*stream).read_exact(&mut path)?;
    stream.set_read_timeout(None)?;
    let path =
        String::from_utf8(path).map_err(|_| io::Error::other("handshake path is not UTF-8"))?;
    Ok((head[0], path))
}

/// What a shim learned from the session it connected to.
enum Verdict {
    Accept(OwnedFd),
    Passthrough,
    Refuse,
    /// A derived doctest carrier declined this library: exit 0, run nothing.
    Decline,
}

/// Receive the verdict: the pipe to put on fd 1, leave to run unisolated, or a
/// refusal.
fn recv_verdict(stream: &UnixStream) -> io::Result<Verdict> {
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
    match byte[0] {
        PASSTHROUGH => return Ok(Verdict::Passthrough),
        REFUSE => return Ok(Verdict::Refuse),
        DECLINE => return Ok(Verdict::Decline),
        _ => {}
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
    Ok(Verdict::Accept(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Run as the shim when invoked as one: `argv[1]` is [`RUNNER_ARG`] (cargo's
/// runner) or `argv[0]` is the rustdoc link. `None` means this is an ordinary
/// brokkr invocation. Only returns at all when the exec failed.
#[allow(clippy::print_stderr)] // Subprocess protocol error; the parent owns operator rendering.
pub(crate) fn maybe_run() -> Option<i32> {
    let mut args = std::env::args_os();
    let invoked = args.next()?;
    let (role, command) = if Path::new(&invoked)
        .file_name()
        .is_some_and(|n| n.to_str() == Some(RUSTDOC_LINK))
    {
        let mut all = vec!["rustdoc".into()];
        all.extend(args);
        (ROLE_RUSTDOC, all)
    } else {
        if args.next()?.to_str()? != RUNNER_ARG {
            return None;
        }
        (ROLE_HARNESS, args.collect())
    };
    // A rustdoc invocation that runs no doctests (a version probe, say) has
    // no stream to isolate - its stdout is cargo's answer - so it never asks
    // for a pipe, and a strict session never sees it to refuse it.
    let runs_doctests = role != ROLE_RUSTDOC || rustdoc_runs_doctests(&command[1..]);
    match run_shim(role, &command, runs_doctests) {
        ShimEnd::Declined => Some(0),
        ShimEnd::Failed(error) => {
            eprintln!("brokkr harness shim: {error}");
            Some(1)
        }
    }
}

/// Whether a rustdoc argv runs doctests: `--test`, given directly or inside an
/// `@path` argument file (cargo moves a long rustdoc command line into one).
/// An argument file that cannot be read counts as running them: the
/// handshake then decides, so an unreadable file can never slip a doctest run
/// past a strict session.
fn rustdoc_runs_doctests(args: &[std::ffi::OsString]) -> bool {
    args.iter().any(|arg| {
        let bytes = arg.as_bytes();
        if bytes == b"--test" {
            return true;
        }
        let Some(path) = bytes.strip_prefix(b"@") else {
            return false;
        };
        fs::read(std::ffi::OsStr::from_bytes(path)).map_or(true, |content| {
            content.split(|b| *b == b'\n').any(|line| line == b"--test")
        })
    })
}

/// How a shim that did not exec ends.
enum ShimEnd {
    /// The session declined this rustdoc: exit successfully.
    Declined,
    Failed(io::Error),
}

/// Obtain this harness's pipe if the run offers one, then exec the program.
///
/// On a permissive session every way isolation can fail short of a pipe - no
/// socket in the environment, a socket nobody answers, a passthrough verdict -
/// execs the program unisolated on the stdout it already has. That is the
/// doctest runtool (rustdoc captures its output) and any nested cargo. On a
/// strict session only a passthrough verdict may do that; any failure to get a
/// verdict, and a refusal, end the shim here instead, without the exec.
fn run_shim(role: u8, args: &[std::ffi::OsString], handshake: bool) -> ShimEnd {
    let Some((program, rest)) = args.split_first() else {
        return ShimEnd::Failed(io::Error::other("missing harness program"));
    };
    let strict = std::env::var_os(STRICT_ENV).is_some();
    // A harness names the executable cargo asked it to run; a rustdoc names
    // the package directory cargo runs it for (cargo's own environment for
    // the doctest invocation), which a derived carrier admits it by.
    let named = if role == ROLE_RUSTDOC {
        std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default()
    } else {
        program.clone()
    };
    let verdict = if handshake {
        std::env::var_os(SOCKET_ENV)
            .ok_or_else(|| io::Error::other("no harness session in the environment"))
            .and_then(|socket| UnixStream::connect(Path::new(&socket)))
            .and_then(|stream| {
                send_hello(&stream, role, named.as_bytes())?;
                recv_verdict(&stream)
            })
    } else {
        Ok(Verdict::Passthrough)
    };
    match verdict {
        Ok(Verdict::Accept(pipe)) => {
            // SAFETY: both fds are valid; dup2 onto fd 1 is the documented use.
            if unsafe { libc::dup2(pipe.as_raw_fd(), libc::STDOUT_FILENO) } < 0 {
                return ShimEnd::Failed(io::Error::last_os_error());
            }
        }
        Ok(Verdict::Passthrough) => {}
        Ok(Verdict::Decline) => return ShimEnd::Declined,
        Ok(Verdict::Refuse) => {
            return ShimEnd::Failed(io::Error::other(format!(
                "refused by the strict harness session: {} cannot be attributed to a planned \
                 test binary or library, so it is not run",
                program.to_string_lossy()
            )));
        }
        Err(e) if strict => {
            return ShimEnd::Failed(io::Error::other(format!(
                "no verdict from the strict harness session ({e}); not running {} unisolated",
                program.to_string_lossy()
            )));
        }
        Err(_) => {}
    }
    let mut cmd = Command::new(program);
    cmd.args(rest);
    // What steers cargo and rustdoc at this run's shim goes no further than
    // the program cargo meant to run: a test that runs cargo or rustdoc gets
    // the toolchain's own.
    cmd.env_remove(SOCKET_ENV)
        .env_remove(STRICT_ENV)
        .env_remove("RUSTDOC");
    for (key, value) in std::env::vars_os() {
        let is_runner = key
            .to_str()
            .is_some_and(|k| k.starts_with("CARGO_TARGET_") && k.ends_with("_RUNNER"));
        if is_runner && value.to_string_lossy().contains(RUNNER_ARG) {
            cmd.env_remove(key);
        }
    }
    ShimEnd::Failed(cmd.exec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_runner::TEST_TIMEOUT;

    #[test]
    fn session_has_distinct_socket_and_rustdoc_entrypoint() {
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let context = RunContext::new(super::super::StopSignal::new(|| false), |_| {});
        let first = Session::new(Arc::clone(&stdout), None, None, None, context.clone())
            .expect("first session");
        let second = Session::new(stdout, None, None, None, context).expect("second session");
        assert_ne!(first.socket(), second.socket());
        assert_eq!(
            fs::read_link(first.rustdoc()).expect("rustdoc symlink"),
            own_exe()
        );
        assert!(!own_exe().to_string_lossy().contains(char::is_whitespace));
        assert!(same_exe(std::process::id()).expect("own exe readable"));
    }

    #[test]
    fn fd_handoff_preserves_pipe_eof() {
        let (a, b) = UnixStream::pair().expect("socket pair");
        let (reader, writer) = pipe().expect("pipe");
        let mut reader = fs::File::from(reader);
        send_verdict(&a, ACCEPT, Some(writer.as_raw_fd())).expect("send fd");
        let Verdict::Accept(pipe) = recv_verdict(&b).expect("recv") else {
            panic!("an accept carries a pipe");
        };
        let mut received = fs::File::from(pipe);
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
        assert!(matches!(
            recv_verdict(&b).expect("recv"),
            Verdict::Passthrough
        ));
        send_verdict(&a, REFUSE, None).expect("send");
        assert!(matches!(recv_verdict(&b).expect("recv"), Verdict::Refuse));
    }

    /// The handshake carries the executable cargo asked the runner to run,
    /// which is the only place cargo states which test binary a harness is.
    #[test]
    fn the_handshake_round_trips_role_and_executable() {
        let (a, b) = UnixStream::pair().expect("socket pair");
        send_hello(&a, ROLE_HARNESS, b"/t/debug/deps/suite-abc").expect("send");
        let (role, path) = recv_hello(&b).expect("recv");
        assert_eq!(role, ROLE_HARNESS);
        assert_eq!(path, "/t/debug/deps/suite-abc");
    }

    /// A session state whose "cargo" is this test process's parent,
    /// so a socket pair inside this process authenticates as cargo's child
    /// running this very binary - the shape a real runner presents.
    fn strict_state(
        known: &[(&str, &str)],
        rustdoc: bool,
        sink: Option<ObservationSink>,
    ) -> Arc<SessionState> {
        let parent = proc_parent(std::process::id()).expect("own parent");
        Arc::new(SessionState {
            cargo_pid: AtomicU32::new(parent),
            harnesses: Mutex::new(Vec::new()),
            seen: Mutex::new(HashSet::new()),
            drains: Mutex::new(Vec::new()),
            accepting: AtomicBool::new(true),
            drain_cancel: AtomicBool::new(false),
            watchdog_done: Doorbell::default(),
            stdout: Arc::new(Mutex::new(Vec::new())),
            sink,
            context: RunContext::new(super::super::StopSignal::new(|| false), |_| {}),
            strict: Some(StrictPlan {
                known: known
                    .iter()
                    .map(|(p, h)| ((*p).to_owned(), (*h).to_owned()))
                    .collect(),
                rustdoc,
                rustdoc_admission: None,
            }),
            gate: None,
        })
    }

    fn collecting_sink() -> (ObservationSink, Arc<Mutex<Vec<super::super::Observation>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_t = Arc::clone(&seen);
        let sink: ObservationSink = Arc::new(move |o| seen_t.lock().expect("sink").push(o));
        (sink, seen)
    }

    /// A planned "harness" on disk: its path and the hash the plan holds.
    fn planned_file(name: &str, body: &[u8]) -> (String, String) {
        let dir = crate::test_scratch::scratch("harness_shim", name);
        let path = dir.join("harness");
        fs::write(&path, body).expect("write harness");
        let hash = hash_file(&path).expect("hash");
        (path.to_string_lossy().into_owned(), hash)
    }

    /// A strict carrier session holding a derived carrier's admission table:
    /// `lib` doctested, `cli` opted out, both real directories.
    fn admission_state(
        name: &str,
        sink: ObservationSink,
    ) -> (Arc<SessionState>, std::path::PathBuf) {
        let root = crate::test_scratch::scratch("harness_shim", name);
        for pkg in ["lib", "cli"] {
            fs::create_dir_all(root.join(pkg)).expect("package dir");
        }
        let table = [("lib", true), ("cli", false)]
            .into_iter()
            .map(|(pkg, doctested)| {
                (
                    super::super::canonical_package_dir(&root.join(pkg)),
                    super::super::LibraryDoctest {
                        package: pkg.to_owned(),
                        doctested,
                    },
                )
            })
            .collect();
        let mut state = Arc::into_inner(strict_state(&[], true, Some(sink))).expect("sole owner");
        state.strict.as_mut().expect("strict").rustdoc_admission = Some(table);
        (Arc::new(state), root)
    }

    /// A derived carrier admits rustdoc per library, after authentication:
    /// an opted-out library is declined - recorded, then told to exit 0 - a
    /// doctested one gets its pipe, and a directory the table does not hold
    /// (or none) is refused, never run and never declined.
    #[test]
    fn a_derived_carrier_admits_rustdoc_per_library() {
        let (sink, observed) = collecting_sink();
        let (state, root) = admission_state("admission-decline", sink);
        let (parent, shim) = UnixStream::pair().expect("socket pair");
        send_hello(&shim, ROLE_RUSTDOC, root.join("cli").as_os_str().as_bytes()).expect("send");
        assert!(accept_one(&parent, &state).is_ok());
        assert!(matches!(
            recv_verdict(&shim).expect("verdict"),
            Verdict::Decline
        ));
        let events: Vec<ObsEvent> = observed
            .lock()
            .expect("sink")
            .iter()
            .map(|o| o.event.clone())
            .collect();
        assert_eq!(
            events,
            vec![ObsEvent::DoctestDeclined {
                package: "cli".into()
            }]
        );
        release(&state);

        let (sink, observed) = collecting_sink();
        let (state, root) = admission_state("admission-run", sink);
        let (parent, shim) = UnixStream::pair().expect("socket pair");
        send_hello(&shim, ROLE_RUSTDOC, root.join("lib").as_os_str().as_bytes()).expect("send");
        assert!(accept_one(&parent, &state).is_ok());
        assert!(matches!(
            recv_verdict(&shim).expect("verdict"),
            Verdict::Accept(_)
        ));
        assert!(observed.lock().expect("sink").is_empty());
        release(&state);

        for (case, named) in [
            ("admission-unknown", "elsewhere"),
            ("admission-unnamed", ""),
        ] {
            let (sink, _) = collecting_sink();
            let (state, root) = admission_state(case, sink);
            let dir = if named.is_empty() {
                Vec::new()
            } else {
                root.join(named).as_os_str().as_bytes().to_vec()
            };
            let (parent, shim) = UnixStream::pair().expect("socket pair");
            send_hello(&shim, ROLE_RUSTDOC, &dir).expect("send");
            assert!(
                matches!(
                    accept_one(&parent, &state),
                    Err(Refusal::UnidentifiedRustdoc { .. })
                ),
                "{case}"
            );
            release(&state);
        }
    }

    /// Cargo can move a long rustdoc command line into an `@argfile`; the
    /// doctest flag inside one still makes the invocation hand-shake, and an
    /// unreadable one is treated as a doctest run rather than waved through.
    #[test]
    fn a_rustdoc_argfile_is_read_for_the_doctest_flag() {
        let dir = crate::test_scratch::scratch("harness_shim", "argfile");
        let with = dir.join("with");
        let without = dir.join("without");
        fs::write(&with, "--crate-name\nlib\n--test\nsrc/lib.rs\n").expect("write");
        fs::write(&without, "--crate-name\nlib\nsrc/lib.rs\n").expect("write");
        let arg = |p: &Path| std::ffi::OsString::from(format!("@{}", p.display()));
        assert!(rustdoc_runs_doctests(&["--test".into()]));
        assert!(rustdoc_runs_doctests(&[arg(&with)]));
        assert!(!rustdoc_runs_doctests(&[arg(&without)]));
        assert!(rustdoc_runs_doctests(&[arg(&dir.join("missing"))]));
        assert!(!rustdoc_runs_doctests(&["--version".into()]));
    }

    fn release(state: &SessionState) {
        state.accepting.store(false, Ordering::Release);
        let drains = std::mem::take(&mut *state.drains.lock().expect("drains"));
        super::super::procgroup::join_drains(drains, &state.drain_cancel, &state.context);
    }

    /// Under a strict session a cargo child that fails the handshake is an
    /// attribution error, recorded in the parent, and it is refused rather
    /// than left to run on the shared stream.
    #[test]
    fn a_failed_handshake_on_a_strict_session_is_refused_and_recorded() {
        let (sink, observed) = collecting_sink();
        let state = strict_state(&[("/planned", "0")], false, Some(sink));
        // A peer that says nothing it should: an unknown role byte.
        let (parent, shim) = UnixStream::pair().expect("socket pair");
        send_hello(&shim, b'?', b"/planned").expect("send");
        let Err(refusal) = accept_one(&parent, &state) else {
            panic!("a bad handshake is refused");
        };
        assert!(matches!(&refusal, Refusal::UnknownRole { role: b'?' }));
        refuse(&parent, &refusal, &state);
        assert!(matches!(
            recv_verdict(&shim).expect("verdict"),
            Verdict::Refuse
        ));
        let events = observed.lock().expect("observed");
        assert!(
            events
                .iter()
                .any(|o| matches!(o.event, ObsEvent::AttributionRefused { .. })),
            "the refusal is recorded as an attribution error"
        );
    }

    #[test]
    fn a_missing_handshake_is_a_typed_io_refusal() {
        let (sink, observed) = collecting_sink();
        let state = strict_state(&[], false, Some(sink));
        let (peer, shim) = UnixStream::pair().expect("socket pair");
        drop(shim);
        let Err(refusal) = accept_one(&peer, &state) else {
            panic!("a missing handshake is refused");
        };
        assert!(matches!(&refusal, Refusal::HandshakeIo(_)));
        refuse(&peer, &refusal, &state);
        assert!(
            observed
                .lock()
                .expect("observed")
                .iter()
                .any(|observation| {
                    matches!(observation.event, ObsEvent::AttributionRefused { .. })
                })
        );
    }

    /// A strict session's harness must resolve to a planned executable: an
    /// unknown one is refused, a planned one gets its pipe.
    #[test]
    fn a_strict_session_admits_only_planned_executables() {
        let (path, hash) = planned_file("admits_planned", b"harness");
        let (sink, observed) = collecting_sink();
        let state = strict_state(&[(path.as_str(), hash.as_str())], false, Some(sink));
        let (parent, shim) = UnixStream::pair().expect("socket pair");
        send_hello(&shim, ROLE_HARNESS, b"/elsewhere").expect("send");
        let Err(refusal) = accept_one(&parent, &state) else {
            panic!("an unplanned executable is refused");
        };
        assert!(matches!(&refusal, Refusal::UnplannedExecutable { .. }));
        refuse(&parent, &refusal, &state);
        assert!(matches!(
            recv_verdict(&shim).expect("verdict"),
            Verdict::Refuse
        ));
        assert!(observed.lock().expect("observed").iter().any(|o| matches!(
            &o.event,
            ObsEvent::AttributionRefused {
                refusal: AttributionRefusal::UnplannedExecutable { executable },
            } if executable == "/elsewhere"
        )));

        let (parent, shim) = UnixStream::pair().expect("socket pair");
        send_hello(&shim, ROLE_HARNESS, path.as_bytes()).expect("send");
        assert!(
            accept_one(&parent, &state).is_ok(),
            "a planned harness is accepted"
        );
        assert!(matches!(
            recv_verdict(&shim).expect("verdict"),
            Verdict::Accept(_)
        ));
        release(&state);
    }

    /// The planned path is not enough: a harness whose file was rebuilt
    /// since the plan - same path, other content, the cross-shape in-place
    /// rebuild - is refused at its handshake, the last moment before it runs.
    #[test]
    fn a_strict_session_refuses_a_planned_path_with_other_content() {
        let (path, hash) = planned_file("rebuilt_in_place", b"the planned build");
        fs::write(&path, b"another shape's build").expect("rebuild");
        let (sink, observed) = collecting_sink();
        let state = strict_state(&[(path.as_str(), hash.as_str())], false, Some(sink));
        let (parent, shim) = UnixStream::pair().expect("socket pair");
        send_hello(&shim, ROLE_HARNESS, path.as_bytes()).expect("send");
        let Err(refusal) = accept_one(&parent, &state) else {
            panic!("a rebuilt harness is refused");
        };
        assert!(matches!(&refusal, Refusal::RebuiltExecutable { .. }));
        refuse(&parent, &refusal, &state);
        assert!(matches!(
            recv_verdict(&shim).expect("verdict"),
            Verdict::Refuse
        ));
        assert!(observed.lock().expect("observed").iter().any(|o| matches!(
            &o.event,
            ObsEvent::AttributionRefused {
                refusal: AttributionRefusal::RebuiltExecutable { .. },
            }
        )));
    }

    /// A session holding the run's build to a plan answers nothing before the
    /// comparison is settled. A strict one admits only a clean build; a
    /// permissive one admits whatever the verdict, since the caller records
    /// what a drift means for the lane's inventory.
    #[test]
    fn a_gated_session_answers_only_once_the_build_is_settled() {
        let (path, hash) = planned_file("gated", b"harness");
        for (strict, verdict, admitted) in [
            (true, GateState::Clean, true),
            (true, GateState::Drift(vec!["x was rebuilt".into()]), false),
            (true, GateState::BuildFailed, false),
            (false, GateState::Drift(vec!["x was rebuilt".into()]), true),
        ] {
            let (sink, observed) = collecting_sink();
            let mut state = Arc::into_inner(strict_state(
                &[(path.as_str(), hash.as_str())],
                false,
                Some(sink),
            ))
            .expect("sole owner");
            if !strict {
                state.strict = None;
            }
            let gate = Arc::new(BuildGate::default());
            state.gate = Some(Arc::clone(&gate));
            let state = Arc::new(state);
            let settler = {
                let gate = Arc::clone(&gate);
                let verdict = verdict.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(30));
                    gate.receive();
                    gate.settle(verdict);
                })
            };
            let (parent, shim) = UnixStream::pair().expect("socket pair");
            send_hello(&shim, ROLE_HARNESS, path.as_bytes()).expect("send");
            let answer = accept_one(&parent, &state);
            settler.join().expect("settler");
            if admitted {
                assert!(answer.is_ok(), "strict={strict} {verdict:?}");
                assert!(matches!(
                    recv_verdict(&shim).expect("verdict"),
                    Verdict::Accept(_)
                ));
                release(&state);
            } else {
                let Err(refusal) = answer else {
                    panic!("strict={strict} {verdict:?} is refused");
                };
                assert!(matches!(&refusal, Refusal::UnverifiedBuild { .. }));
                refuse(&parent, &refusal, &state);
                assert!(matches!(
                    recv_verdict(&shim).expect("verdict"),
                    Verdict::Refuse
                ));
                assert!(observed.lock().expect("observed").iter().any(|o| matches!(
                    &o.event,
                    ObsEvent::AttributionRefused {
                        refusal: AttributionRefusal::UnverifiedBuild { .. }
                    }
                )));
            }
        }
    }

    /// A rustdoc stream is admitted only on a run the plan holds a doctest
    /// carrier for; elsewhere it is refused and recorded, never attributed.
    #[test]
    fn a_strict_session_admits_rustdoc_only_on_a_planned_carrier() {
        for carrier in [false, true] {
            let (sink, observed) = collecting_sink();
            let state = strict_state(&[], carrier, Some(sink));
            let (parent, shim) = UnixStream::pair().expect("socket pair");
            send_hello(&shim, ROLE_RUSTDOC, b"rustdoc").expect("send");
            let accepted = accept_one(&parent, &state);
            if carrier {
                assert!(
                    accepted.is_ok(),
                    "the planned carrier's rustdoc is accepted"
                );
                assert!(matches!(
                    recv_verdict(&shim).expect("verdict"),
                    Verdict::Accept(_)
                ));
                release(&state);
            } else {
                let Err(refusal) = accepted else {
                    panic!("a rustdoc with no planned carrier is refused");
                };
                assert!(matches!(&refusal, Refusal::RustdocWithoutCarrier));
                refuse(&parent, &refusal, &state);
                assert!(matches!(
                    recv_verdict(&shim).expect("verdict"),
                    Verdict::Refuse
                ));
                assert!(observed.lock().expect("observed").iter().any(|o| matches!(
                    &o.event,
                    ObsEvent::AttributionRefused {
                        refusal: AttributionRefusal::RustdocWithoutCarrier,
                    }
                )));
            }
        }
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
        assert!(
            text.contains("suspect") && text.contains("a::aborts"),
            "{text}"
        );
        assert!(
            text.contains("    a::fails"),
            "the real failure is on the roster: {text}"
        );
        assert!(
            !text.contains("    a::aborts"),
            "the suspect is not on the roster: {text}"
        );
        assert!(
            closing
                .last()
                .is_some_and(|l| l.starts_with("test result: FAILED.")),
            "{text}"
        );

        // A suite that summarised is left alone.
        let closed = Mutex::new(TestTracker::default());
        let mut recon = JsonReconstructor::default();
        recon.observe(
            r#"{"type":"suite","event":"started","test_count":0}"#,
            &closed,
        );
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
            tap: None,
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
            tap: None,
        };
        assert!(!live_harness(&harness));
        assert!(!harness.eof.load(Ordering::Acquire));
    }
}
