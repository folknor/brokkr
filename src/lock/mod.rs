//! The global brokkr lock: state and API, flock acquisition, same-process
//! re-entry and release.
//!
//! The fresh-hold order is in `hold_protocol`, the waiter queue in `queue`,
//! lock-file I/O in `record`, and holder uptime and hints in `presentation`.
//! The control socket, `service.rs` here, is mounted as `crate::lock_service`.
//! The holder report is built from
//! `observe`, `activity`, `collector`, `cgroup` and `report`. The wire
//! literals, probe and record format shared with the standalone rustc guard
//! are the crate-root `lock_*.rs` files included below.
//!
//! Under `cfg(test)` the lock and the compile lease live under one scratch
//! home per test process ([`lock_home`]), so no test takes, reads or drains
//! the real ones.

use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use crate::error::{DevError, LockError};
use crate::process::{HostScope, Pid, ProcIdentity, StartTicks};

pub(crate) mod activity;
pub(crate) mod cgroup;
pub(crate) mod collector;
mod hold_protocol;
pub(crate) mod observe;
pub(super) mod presentation;
pub(crate) mod queue;
mod record;
pub(crate) mod report;
use self::hold_protocol::{HOST_EFFECTS, HOST_EFFECTS_WITHOUT_TREE, acquire_at};
#[cfg(test)]
use self::hold_protocol::{HoldEffects, INERT, LOCK_POLL, wait_for_flock};
pub use self::presentation::{same_agent_hint, verified_uptime_for};
pub use self::record::LockInfo;
use self::record::{invalidate_metadata, publish, read_lock_contents};

mod lock_record {
    include!("../lock_record.rs");
    // The full codec, kept out of `lock_record.rs` because the standalone
    // guard includes that file and only ever reads `auth`.
    include!("../lock_record_codec.rs");
}

mod lock_probe {
    include!("../lock_probe.rs");
    // The minting side and the holder-only identities, kept out of
    // `lock_probe.rs` because the standalone guard includes that file and
    // only ever parses and hashes - it would carry them as dead code.
    include!("../lock_tokens.rs");
}

pub(crate) use lock_probe::protocol::{
    CAPABILITY_ENV, GUARD_HANDSHAKE_PREFIX, GUARD_INFO_ARG, GUARD_PROTOCOL, LEASE_MARKER_ENV,
};
pub(crate) use lock_probe::{AcqId, AuthHash, Nonce, RunToken};
pub(crate) use lock_record::{Record as LockRecord, escape_value, unescape_value};

/// Mutable lock-file state - maintained while brokkr holds the lock so
/// `brokkr lock` (from another invocation) can see the current child PID
/// and bench-run progress.
struct LockState {
    /// Whether the holder found a `brokkr.toml`; `project` is its key when it
    /// did and empty when not.
    configured: bool,
    project: String,
    command: String,
    /// Full brokkr invocation minus `argv[0]` (e.g. `add-locations-to-ways
    /// --dataset europe --bench 3`). Captured at acquire time.
    args: String,
    project_root: String,
    /// Typed process identity and the host/namespace scope in which it was
    /// captured. Missing values serialize as empty protocol fields and fail
    /// closed when a reader tries to verify them.
    process_identity: Option<ProcIdentity>,
    scope: Option<HostScope>,
    /// This acquisition's public id, naming its control socket
    /// (`crate::lock_service`). `None` for a hold that runs no service (tests).
    acq_id: Option<AcqId>,
    /// `CODEX_THREAD_ID` / `CODEX_SESSION_ID` from the holder's environment,
    /// when set. Advisory attribution only ("the holder reports the same codex
    /// thread as you"); never an identity anything is authorised on.
    codex_thread: String,
    codex_session: String,
    /// Hash of this acquisition's capability nonce (`crate::hold`), or `None`
    /// while the hold is still draining and has not minted one. `None` is what
    /// makes the drain safe: a guard that reads an empty `auth` refuses, so no
    /// long-running compile can be admitted into a window brokkr has not
    /// finished clearing. It is the anti-starvation property, obtained from the
    /// publication order rather than from a separate flag that a crashed writer
    /// could leave behind.
    auth: Option<AuthHash>,
    /// True while this hold is waiting for earlier compilation leases to drain.
    /// Published in the control-service snapshot so `brokkr lock` can distinguish
    /// acquisition from work.
    draining: bool,
    /// The workload children brokkr published under the lock, one slot per
    /// spawning thread so concurrent lanes do not overwrite each other, each
    /// paired with its starttime token (captured when recorded; empty if
    /// unreadable, which fails verification closed). Updated by the harness
    /// each iteration of a bench run; cleared by the orchestrator after the
    /// captured runner returns. A slot left behind is harmless to a recycled
    /// PID: `--hard` signals only a generation whose starttime still matches.
    children: Vec<(std::thread::ThreadId, TrackedProcess)>,
    /// Auxiliary long-running child PIDs - mock-servers (sæhrimnir) that
    /// live across many `child` rotations, each paired with its starttime
    /// token. Plural because `service --all` keeps one mock per distinct
    /// fixture alive for the whole cohort. `brokkr kill --hard` SIGKILLs
    /// each of these alongside `child` so none leak.
    mocks: Vec<TrackedProcess>,
    /// Current bench-run progress as `(run, total)` (1-based).
    progress: Option<(u32, u32)>,
}

/// A tracked process slot keeps a usable PID for diagnostics and an optional
/// typed generation token for signalling. Unreadable start ticks are `None`,
/// never an empty string that each caller must remember to reject.
#[derive(Debug, Clone)]
struct TrackedProcess {
    pid: Pid,
    start: Option<StartTicks>,
}

impl TrackedProcess {
    fn capture(pid: u32) -> Option<Self> {
        let pid = Pid::new(pid)?;
        let start = crate::process::ProcStat::read(pid)
            .ok()
            .map(|stat| stat.start());
        Some(Self { pid, start })
    }

    /// Whether the recorded generation is provably gone (`process::generation`).
    /// Only positive evidence prunes a slot: an unreadable start token, a
    /// procfs entry hidden from us, or a zombie leader (whose other threads
    /// may still run) all keep it, so a live target is never dropped from
    /// what `brokkr kill --hard` sees.
    fn gone(&self) -> bool {
        self.start.is_some_and(|start| {
            crate::process::generation(crate::process::ProcIdentity::new(self.pid, start))
                == crate::process::Generation::Gone
        })
    }

    fn wire(&self) -> (u32, String) {
        (
            self.pid.get(),
            self.start
                .map(|start| start.get().to_string())
                .unwrap_or_default(),
        )
    }
}

/// The flock-owning core shared by every [`LockGuard`] handed out for one
/// hold. Exactly one exists per held lock file; nested [`acquire`] calls in
/// the same process get another `Arc` to it (see [`acquire`] for why), so
/// the flock is released only when the *last* guard drops.
struct LockInner {
    fd: OwnedFd,
    /// The lock file this flock is on. Re-entry matches on it, so a test
    /// lock on a scratch path can never alias the real global lock.
    path: PathBuf,
    state: Mutex<LockState>,
    /// Tree edits activated under this lock (a disabled toolchain pin, a
    /// filtered cargo config). Undone on drop *before* the flock is released,
    /// so they are in force for exactly the locked window. See
    /// [`crate::tree_policy`].
    tree: Option<crate::tree_policy::HeldTree>,
    /// The compilation capability belongs to this hold. Nested guards share
    /// the `LockInner`, and dropping the last guard drops this nonce with it.
    capability: Option<Nonce>,
    /// Directories whose tree edits need watchdog restoration. The primary
    /// lock-scoped guard is inserted at construction; worktree guards register
    /// their paths for exactly their own lifetime.
    active_tree_dirs: Mutex<HashMap<PathBuf, usize>>,
    /// This acquisition's control socket. Dropped first on release, while the
    /// flock is still held: a stop that has not committed by then is refused.
    service: Option<crate::lock_service::Service>,
    /// What the hold says about its work (`observe`), shared with the
    /// service; `None` for a hold that runs no service (tests).
    observed: Option<Arc<observe::Observed>>,
    /// The per-acquisition accounting cgroup, where delegation allows one.
    /// Restored on release, before the flock goes.
    cgroup: Option<cgroup::HoldCgroup>,
    /// Set only on dispatch's fresh acquisition ([`acquire_for_command`]) and
    /// cleared by the one [`LockGuard::take_maintenance`] that claims it,
    /// which dispatch makes before the command runs; every other hold starts
    /// without one, so no re-entered guard can ever claim it.
    maintenance_pending: std::sync::atomic::AtomicBool,
}

impl LockInner {
    /// Rewrite the lock file and the service's status snapshot from `state`.
    fn publish(&self, state: &LockState) {
        publish(self.fd.as_raw_fd(), state);
        if let Some(service) = &self.service {
            service.update(snapshot_of(state));
        }
    }
}

fn snapshot_of(state: &LockState) -> crate::lock_service::Snapshot {
    crate::lock_service::Snapshot {
        draining: state.draining,
        progress: state.progress,
        has_child: !state.children.is_empty(),
        mocks: state.mocks.len(),
    }
}

impl Drop for LockInner {
    fn drop(&mut self) {
        // End the control service first: after this no stop can commit
        // against this acquisition, and the socket path is gone.
        drop(self.service.take());
        // Invalidate the metadata while we still hold the flock. A reader
        // during the release/re-acquire handoff then sees an empty file and
        // fails closed, instead of a complete, still-verifiable record of a
        // holder that no longer holds anything - which `kill` could
        // otherwise legitimately verify and signal. This covers normal
        // release; abnormal termination (SIGKILL) leaves metadata behind,
        // but the kernel has released the flock so `status()`'s probe
        // reports no holder and the next acquirer rewrites it.
        invalidate_metadata(self.fd.as_raw_fd());
        // Move the holder back out of its accounting cgroup while the flock
        // is still held, so the next acquisition's sweep never races it.
        drop(self.cgroup.take());
        // Undo the tree edits (if any) while we still hold the flock, then
        // release. Doing it before the close keeps the edited window inside the
        // locked window, so a concurrent brokkr can never observe it.
        drop(self.tree.take());
        // Released by closing the descriptor (the `OwnedFd` field), never by
        // an explicit `LOCK_UN`: the flock belongs to the open file
        // description, so a privileged trim that inherited a duplicate of it
        // (`crate::fstrim`) keeps the lock held until it exits, whatever path
        // ends this process. An unlock here would release it under the trim.
    }
}

/// RAII lock guard. The underlying flock is released when the last guard for
/// this hold drops ([`LockInner`]'s drop); in the common non-nested case that
/// is simply this guard's drop, as before.
pub struct LockGuard {
    inner: Arc<LockInner>,
}

/// The locks this process currently holds, weakly. This registry is
/// intentionally process-wide: any thread can re-enter a process-owned flock,
/// and the watchdog thread also reads the active hold's tree paths. A
/// thread-local index could miss the existing hold and self-deadlock.
/// [`acquire`] consults this to re-enter an existing hold instead of opening
/// a second fd on the same file. `flock(2)` treats each fd as an independent
/// contender even within one process, so that second fd would block forever
/// on a lock the process itself holds (the self-deadlock trap that makes naive
/// "hoist an acquire around a loop of acquiring callees" fatal).
///
/// `Weak` so the registry never extends a hold: release is driven purely by
/// guard drops. Dead entries are swept opportunistically on each access.
static HELD: Mutex<Vec<Weak<LockInner>>> = Mutex::new(Vec::new());

/// Lock the registry, treating poison as recoverable. The critical sections
/// are tiny and allocation-only; if one somehow panicked, falling back to the
/// inner value is strictly safer than skipping the re-entry check (which
/// would send a nested acquire into the flock self-deadlock).
fn held_registry() -> std::sync::MutexGuard<'static, Vec<Weak<LockInner>>> {
    HELD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The one window in which store maintenance may remove what earlier, dead
/// brokkr commands left: after a fresh exclusive acquisition, before the
/// acquiring command does any work. It borrows the guard and is neither
/// `Clone` nor `Copy`; `crate::maintenance::run` consumes it, so no later
/// code holds the authority and nothing the command itself creates can be
/// swept.
pub(crate) struct MaintenancePhase<'g> {
    _guard: std::marker::PhantomData<&'g LockGuard>,
}

impl LockGuard {
    /// A close-on-exec duplicate of the descriptor holding the flock, numbered
    /// 10 or above so a spawn's own descriptors never collide with it. It
    /// shares the open file description, so whoever keeps it open keeps the
    /// lock held: only `crate::fstrim` hands one to a child, which it reaps
    /// before the hold can drop.
    pub(crate) fn duplicate_flock(&self) -> std::io::Result<OwnedFd> {
        // SAFETY: a valid descriptor this hold owns for its whole life.
        let fd = unsafe { libc::fcntl(self.inner.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fcntl just returned this descriptor, owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The maintenance phase of a hold [`acquire_for_command`] made, once:
    /// `None` for any other hold, and on every call after the first.
    pub(crate) fn take_maintenance(&self) -> Option<MaintenancePhase<'_>> {
        self.inner
            .maintenance_pending
            .swap(false, std::sync::atomic::Ordering::AcqRel)
            .then_some(MaintenancePhase {
                _guard: std::marker::PhantomData,
            })
    }

    /// Register an additional edited tree directory for the lifetime of this
    /// worktree guard, so the watchdog can restore it if the main thread
    /// cannot unwind. Registered before the edits are applied, so no applied
    /// edit is ever invisible to the watchdog.
    pub(crate) fn track_tree_dir(&self, dir: &Path) -> TreeRegistration {
        let dir = dir.to_owned();
        let mut active = self
            .inner
            .active_tree_dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active.entry(dir.clone()).or_default() += 1;
        TreeRegistration {
            inner: Arc::downgrade(&self.inner),
            dir,
        }
    }

    /// Record the PID of the child process currently running under the lock
    /// (with its starttime identity token), and rewrite the lock file so
    /// concurrent `brokkr lock` invocations can see it.
    ///
    /// One slot per calling thread: a second call from the same thread
    /// replaces its earlier child, and concurrent lanes on other threads keep
    /// theirs. Slots whose process has gone are pruned on every update.
    pub fn set_child_pid(&self, pid: u32) {
        let Some(child) = TrackedProcess::capture(pid) else {
            return;
        };
        let me = std::thread::current().id();
        if let Ok(mut state) = self.inner.state.lock() {
            state
                .children
                .retain(|(thread, tracked)| *thread != me && !tracked.gone());
            state.mocks.retain(|tracked| !tracked.gone());
            state.children.push((me, child));
            self.inner.publish(&state);
        }
    }

    /// Drop this thread's workload child PID. Called by orchestrators after
    /// the captured runner returns, so `brokkr lock` stops showing it and
    /// `--hard` does not have to classify it. Other threads' slots stay,
    /// except any whose process is provably gone - so a slot set on one
    /// thread and cleared from another does not linger either.
    pub fn clear_child_pid(&self) {
        let me = std::thread::current().id();
        if let Ok(mut state) = self.inner.state.lock() {
            state
                .children
                .retain(|(thread, tracked)| *thread != me && !tracked.gone());
            state.mocks.retain(|tracked| !tracked.gone());
            self.inner.publish(&state);
        }
    }

    /// Record current bench-run progress (1-based run index out of total).
    /// Skips the update when `total <= 1` - a lone "run 1/1" line in
    /// `brokkr lock` is noise.
    #[cfg(test)]
    pub fn set_progress(&self, run: u32, total: u32) {
        if total <= 1 {
            return;
        }
        if let Ok(mut state) = self.inner.state.lock() {
            state.progress = Some((run, total));
            self.inner.publish(&state);
        }
    }
}

/// Context written to the lock file so `brokkr lock` can explain who holds it.
pub struct LockContext<'a> {
    /// The `brokkr.toml` project key, `None` without a `brokkr.toml`. The
    /// lock record keeps the two apart (`configured`), since `brokkr kill`
    /// scopes a configured hold by its key and an unconfigured one by its
    /// state root, and any string, the empty one included, is a valid key.
    pub project: Option<&'a str>,
    /// The parsed command; its spec decides the disk-gate exemption and its
    /// name is what the lock file records.
    pub command: crate::cli::CommandId,
    /// The config/state root, recorded as the holder's display identity.
    pub project_root: &'a crate::project::ProjectRoot,
    /// The code tree whose `target` the disk gate probes.
    pub build_root: &'a crate::project::BuildRoot,
    /// The build root and policy a fresh hold applies ([`crate::tree_policy`]),
    /// from the detected project config. `None` keeps acquisition independent
    /// of any process-global setting.
    pub tree: Option<crate::tree_policy::TreeTarget<'a>>,
}

/// Registration of a directory whose tree edits are currently in force. It is
/// separate from the editing guard so a watchdog thread can find the
/// directory while that guard remains on a blocked main-thread stack.
pub(crate) struct TreeRegistration {
    inner: Weak<LockInner>,
    dir: PathBuf,
}

impl Drop for TreeRegistration {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut active = inner
            .active_tree_dirs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove = match active.get_mut(&self.dir) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if remove {
            active.remove(&self.dir);
        }
    }
}

/// Resolve the global lock file path: always `$HOME/.brokkr/brokkr.lock`.
///
/// One path, no alternatives. The lock is brokkr's *global* mutual exclusion -
/// two invocations that resolve different paths are not excluding each other,
/// they are two unsynchronised builds sharing one target dir. That is exactly
/// what the previous `$XDG_RUNTIME_DIR`-first rule allowed: the variable is set
/// per *session* by logind and is freely repointed by sandboxes, containers and
/// `systemd-run`, so a brokkr under one and a brokkr in a login shell held two
/// different files and both believed they had the lock.
///
/// Not `~/.cache/brokkr/`: a cache directory is by specification disposable, and
/// unlinking the lock file while a hold is live breaks the mutex outright - the
/// holder keeps its flock on a now-nameless inode while the next invocation
/// creates a fresh file and locks that instead. `~/.brokkr/` is not swept.
///
/// A non-empty `$HOME` is the one input, and it is per-user by construction,
/// which is what keeps the file writable and the lock scoped to the user whose
/// builds it serialises. If `$HOME` is unset or empty there is no defensible
/// default, so this errors rather than inventing a cwd-relative one.
fn lock_path() -> Result<PathBuf, DevError> {
    let path = crate::user_dirs::lock_file_at(lock_home().as_deref()).ok_or_else(|| {
        DevError::Lock(LockError::from(
            "$HOME is not set or is empty - cannot locate the brokkr lock",
        ))
    })?;
    let dir = path.parent().ok_or_else(|| {
        DevError::Lock(LockError::from("cannot resolve the brokkr lock directory"))
    })?;
    std::fs::create_dir_all(dir)?;
    Ok(path)
}

/// The lock queue's directory ([`queue`]), under the same home as the lock,
/// so unit tests stay on their scratch queue.
pub(crate) fn queue_dir() -> Option<PathBuf> {
    crate::user_dirs::queue_dir_at(lock_home().as_deref())
}

/// The queue that orders acquisitions of the lock file `lock`: the `queue`
/// directory beside it. For the global lock this is [`queue_dir`], since both
/// live in the state directory. The queue is per directory, so this assumes
/// one lock file per directory: two in one would share an order and forfeit
/// each other's places on unrelated flocks. Scratch locks in tests each get a
/// directory of their own for that reason.
pub(super) fn queue_dir_for(lock: &Path) -> Option<PathBuf> {
    lock.parent().map(|dir| dir.join("queue"))
}

/// The bench cell cursor (`crate::bench::host_trace`), in the lock's
/// namespace: only a holder of this lock writes it.
pub(crate) fn cell_cursor_path() -> Option<PathBuf> {
    crate::user_dirs::cell_cursor_at(lock_home().as_deref())
}

/// The home both lock files live under: `$HOME`. Under `cfg(test)` it is one
/// scratch directory per test process instead, so no test takes, reads or
/// drains the real lock or compile lease through this module, as
/// `reservations::store` keeps tests off the real leases. A test that needs
/// `build::cargo_build` holds that scratch lock through `test_hold`.
fn lock_home() -> Option<PathBuf> {
    #[cfg(test)]
    {
        static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        Some(
            HOME.get_or_init(|| crate::test_scratch::scratch("lockfile", "process-home"))
                .clone(),
        )
    }
    #[cfg(not(test))]
    {
        crate::user_dirs::home()
    }
}

/// A hold on this test process's scratch lock ([`lock_home`]) with no host
/// effects: no control service, no drain, no capability of its own, no reap,
/// no tree policy and no disk gate. It is what [`hold_for_new_work`] finds, so code
/// under test may build; a cargo it starts keeps the capability the test
/// process inherited, which is what admits its compilers.
#[cfg(test)]
pub(crate) fn test_hold(
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
) -> LockGuard {
    let path = lock_path().unwrap_or_else(|e| panic!("the test lock path: {e}"));
    acquire_at(
        &path,
        &LockContext {
            project: Some("test"),
            command: crate::cli::CommandId::Bench,
            project_root,
            build_root,
            tree: None,
        },
        &hold_protocol::INERT,
    )
    .unwrap_or_else(|e| panic!("the test hold: {e}"))
}

/// Acquire an exclusive lock on the global lock file, blocking until free.
///
/// If the lock is held by *another process*, prints a waiting message
/// describing the holder and blocks until it is released. On success, writes
/// PID + identity tokens + context to the lock file.
///
/// **Re-entrant within the process.** If this process already holds the lock,
/// the call returns immediately with another guard on the same hold, and the
/// flock is released only when the last guard drops. This exists for the
/// gate cohort (`sync --gate all`): it acquires once around the whole sweep
/// so measurement stays serialized end-to-end, while each swept member
/// (its measurement session) still contains its own acquire for the
/// single-invocation path. Without re-entrancy the inner acquire would open
/// a second fd on the same file and `flock(2)` would block it on the lock
/// this very process holds - a self-deadlock. (`sync --all` also holds the
/// lock sweep-wide, but no longer nests: its per-script runs share the
/// sweep's prebuilt harness and its single acquire.)
///
/// The trade-off, deliberately accepted: two *threads* of one process
/// first-acquiring concurrently used to serialize on the flock and now may
/// share the hold if one registers before the other checks. Brokkr's command
/// flow is sequential - every acquire on one thread is part of the same
/// logical work unit - so sharing is the correct semantics here. Cross-process
/// serialization (the property benchmarks depend on) is untouched.
///
/// A nested acquire keeps the outer hold's lock-file contents (project /
/// command / args): the outer command is the honest holder for `brokkr lock`
/// to report, and its `ctx` was captured from the same argv anyway.
///
/// **Every fresh hold reaps strays and checks the enrolled guard.** Both run
/// here, in the fresh-hold path, rather than in any one caller: they used to
/// live in `context::acquire_cmd_lock_opt`, and a dozen commands that called
/// this function directly (bench contexts, the verify harness, piners,
/// ratatoskr) silently skipped both. A nested acquire runs neither - the
/// outer hold already did.
///
/// A typed acquisition refusal lets callers distinguish a nested-holder
/// refusal, a drain timeout and lock infrastructure failures without parsing
/// the message.
pub fn acquire(ctx: &LockContext<'_>) -> Result<LockGuard, DevError> {
    let path = lock_path()?;
    acquire_at(&path, ctx, &HOST_EFFECTS)
}

/// Time this process spent queued for the global lock, summed over every
/// acquisition that had to wait. Measured from joining the queue to winning
/// the flock; an acquisition admitted on its first try adds nothing. History
/// records it beside the command's total so a listing can tell the command's
/// own time from the queue's.
static LOCK_WAIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn total_lock_wait() -> std::time::Duration {
    std::time::Duration::from_millis(LOCK_WAIT_MS.load(std::sync::atomic::Ordering::Relaxed))
}

fn add_lock_wait(waited: std::time::Duration) {
    let ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
    LOCK_WAIT_MS.fetch_add(ms, std::sync::atomic::Ordering::Relaxed);
}

/// Acquire the global lock without applying the configured build-root tree
/// policy. Worktree execution uses this because its own tree is activated
/// explicitly after the checkout exists.
pub(crate) fn acquire_without_tree(ctx: &LockContext<'_>) -> Result<LockGuard, DevError> {
    let path = lock_path()?;
    acquire_at(&path, ctx, &HOST_EFFECTS_WITHOUT_TREE)
}

/// Dispatch's acquisition for a locked command, without the tree policy: a
/// fresh hold carries the maintenance phase
/// ([`LockGuard::take_maintenance`]) dispatch consumes before the command
/// runs. `nonblocking` refuses a lock another process holds at once, with the
/// typed busy refusal, instead of waiting; re-entry into this process's own
/// hold still succeeds. The phase belongs to the hold, not to a guard, and
/// dispatch claims it before any command code can re-enter.
pub(crate) fn acquire_for_command(
    ctx: &LockContext<'_>,
    nonblocking: bool,
) -> Result<LockGuard, DevError> {
    let path = lock_path()?;
    hold_protocol::acquire_at_with(
        &path,
        ctx,
        &hold_protocol::HOST_EFFECTS_FOR_COMMAND,
        if nonblocking {
            hold_protocol::FlockWait::NonBlocking
        } else {
            hold_protocol::FlockWait::Block
        },
    )
}

/// This process's live hold, as one more guard on it, whatever the stop
/// state: for what records a stopping command rather than starting work for
/// it (its run log). Work asks [`hold_for_new_work`].
pub(crate) fn live_hold() -> Option<LockGuard> {
    let path = lock_path().ok()?;
    let mut held = held_registry();
    held.retain(|w| w.strong_count() > 0);
    held.iter()
        .filter_map(Weak::upgrade)
        .find(|inner| inner.path == path)
        .map(|inner| LockGuard { inner })
}

/// Why [`hold_for_new_work`] offers no hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoHold {
    /// A `brokkr kill` committed this process to stopping. The hold may still
    /// be live, but it admits no further work. A caller that needed the hold
    /// calls [`announce_stopped`] and returns [`DevError::Interrupted`], as
    /// the acquisition's own refusal does.
    Stopped,
    /// The process holds no global lock.
    Absent,
}

/// This process's live hold on the global lock, as one more guard on it, for
/// new work: refused when the process holds nothing or a committed stop
/// admits no new work under it, with the reason, so a caller can tell a
/// stopped command from a missing lock.
///
/// The proof `build::cargo_build` demands before it runs cargo. Asking the
/// registry rather than taking a `&LockGuard` parameter keeps the check at the
/// one choke point every build passes through, including call paths several
/// layers below the command that took the lock. The returned guard shares the
/// hold (like a nested [`acquire`]), so it cannot outlive or split it.
pub fn hold_for_new_work() -> Result<LockGuard, NoHold> {
    // A committed stop admits no further work under the hold it stopped.
    if !crate::shutdown::admits_new_work() {
        return Err(NoHold::Stopped);
    }
    live_hold().ok_or(NoHold::Absent)
}

/// [`hold_for_new_work`] for a caller that refuses without it: a committed
/// stop announces itself and is [`DevError::Interrupted`], a missing hold is
/// `absent`'s error.
pub fn require_hold(absent: impl FnOnce() -> DevError) -> Result<LockGuard, DevError> {
    match hold_for_new_work() {
        Ok(hold) => Ok(hold),
        Err(NoHold::Stopped) => {
            announce_stopped();
            Err(DevError::Interrupted)
        }
        Err(NoHold::Absent) => Err(absent()),
    }
}

/// The line a command prints when a committed stop refuses it new work.
/// Quoted in the refusal table of `docs/commands/check.hygiene.md`.
pub(crate) const STOPPED_MESSAGE: &str =
    "stopped by `brokkr kill`; this command starts no new work";

static STOP_ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Say once why a stopped command refuses work: the exit status alone (130)
/// reads as an ordinary Ctrl-C, and every later refusal in the same unwind
/// would otherwise repeat it.
pub(crate) fn announce_stopped() {
    if !STOP_ANNOUNCED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        crate::output::lock_msg(STOPPED_MESSAGE);
    }
}

/// The capability for the live global hold, without the admission check used
/// by `hold_for_new_work`. A committed stop forbids new work, but retaining the
/// capability until the hold drops preserves the child-stamping contract for
/// any spawn already in progress.
pub(crate) fn current_capability() -> Option<Nonce> {
    let mut held = held_registry();
    held.retain(|w| w.strong_count() > 0);
    held.iter()
        .filter_map(Weak::upgrade)
        .find_map(|inner| inner.capability.clone())
}

/// The live hold's observation table, without the admission check: an
/// activity or a narration during shutdown still describes the hold.
pub(crate) fn current_observed() -> Option<Arc<observe::Observed>> {
    current_observed_and_capability().map(|(observed, _)| observed)
}

/// The live hold's observation table and capability, from the same
/// `LockInner`, so a child stamped with both names one acquisition.
pub(crate) fn current_observed_and_capability() -> Option<(Arc<observe::Observed>, Option<Nonce>)> {
    let mut held = held_registry();
    held.retain(|w| w.strong_count() > 0);
    held.iter().filter_map(Weak::upgrade).find_map(|inner| {
        inner
            .observed
            .clone()
            .map(|observed| (observed, inner.capability.clone()))
    })
}

/// What a process spawned to outlive the live global hold needs from it: a
/// duplicate of the descriptor of the holder's original cgroup directory when
/// the hold runs inside its own accounting group (`cgroup`), so the spawn can
/// move the child back out before it execs. `Ok(None)` when no hold or no
/// group exists; an error when the group exists but its parent cannot be
/// duplicated, since the child would then stay inside the hold's group.
pub(crate) fn current_hold_cgroup_parent() -> std::io::Result<Option<OwnedFd>> {
    let path = match lock_path() {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let mut held = held_registry();
    held.retain(|w| w.strong_count() > 0);
    let Some(inner) = held
        .iter()
        .filter_map(Weak::upgrade)
        .find(|inner| inner.path == path)
    else {
        return Ok(None);
    };
    let Some(cgroup) = inner.cgroup.as_ref() else {
        return Ok(None);
    };
    // SAFETY: a valid descriptor this hold owns for its whole life.
    let fd = unsafe { libc::fcntl(cgroup.parent_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fcntl just returned this descriptor, owned by nobody else.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// Edited tree paths attached to the active global hold, or `None` if no such
/// hold exists. `Some([])` is meaningful: the live hold intentionally edits
/// nothing, so a watchdog must not fall back to the command's armed path.
pub(crate) fn current_hold_tree_dirs() -> Option<Vec<PathBuf>> {
    let path = lock_path().ok()?;
    let mut held = held_registry();
    held.retain(|w| w.strong_count() > 0);
    let inner = held
        .iter()
        .filter_map(Weak::upgrade)
        .find(|inner| inner.path == path)?;
    let active = inner
        .active_tree_dirs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut dirs = active.keys().cloned().collect::<Vec<_>>();
    dirs.sort();
    Some(dirs)
}

/// Check the global lock status. Returns `None` if no lock is held.
///
/// The flock is the sole authority on held/not-held: the non-blocking shared
/// probe succeeds unless a live exclusive brokkr hold conflicts with it. Other
/// observers can share the probe, and a dead process cannot retain a flock, so
/// a leftover file without an exclusive holder is simply not held. There is
/// deliberately no PID-liveness fallback and no stale-file deletion: the
/// recorded PID is namespace-local, so "that PID
/// looks dead from here" can only mean the PID is untrustworthy from this
/// namespace, never that the lock is stale - and deleting the path while the
/// old inode is flocked would let the next acquirer lock a *fresh* inode,
/// silently splitting the serialization the lock exists to provide.
pub fn status() -> Result<Option<LockInfo>, DevError> {
    let path = lock_path()?;

    if !path.exists() {
        return Ok(None);
    }

    let c_path = path_to_cstring(&path)?;
    let fd = open_lock_file(&c_path)?;

    // Shared probes conflict only with a real exclusive holder, so a status
    // query cannot briefly become the exclusive holder and make the rustc
    // guard refuse an otherwise idle compile.
    match lock_probe::try_shared(fd) {
        Ok(true) => {
            // SAFETY: valid fd from open_lock_file, unique ownership.
            let _close = unsafe { OwnedFd::from_raw_fd(fd) };
            return Ok(None);
        }
        Ok(false) => {}
        Err(err) => {
            // SAFETY: valid fd from open_lock_file, unique ownership.
            let _close = unsafe { OwnedFd::from_raw_fd(fd) };
            return Err(DevError::Lock(LockError::from(format!(
                "could not probe the lock: {err}"
            ))));
        }
    }

    // Someone holds it. Read the contents.
    let info = read_lock_contents(fd);
    // SAFETY: valid fd from open_lock_file, unique ownership.
    let _close = unsafe { OwnedFd::from_raw_fd(fd) };

    let Some(info) = info else {
        // Could not parse - report as unknown holder.
        return Ok(Some(LockInfo {
            pid: 0,
            starttime: String::new(),
            boot_id: String::new(),
            acq_id: String::new(),
            pid_ns: String::new(),
            time_ns: String::new(),
            codex_thread: String::new(),
            auth: None,
            configured: false,
            project: "unknown".into(),
            command: "unknown".into(),
            args: String::new(),
            project_root: "unknown".into(),
            children: Vec::new(),
            mocks: Vec::new(),
            progress: None,
        }));
    };

    Ok(Some(info))
}

/// Open (or create) the lock file, returning the raw fd.
fn open_lock_file(c_path: &std::ffi::CString) -> Result<RawFd, DevError> {
    open_lock_file_raw(c_path).map_err(|source| {
        DevError::Lock(LockError::from(format!(
            "failed to open lock file: {source}"
        )))
    })
}

fn open_lock_file_raw(c_path: &std::ffi::CString) -> std::io::Result<RawFd> {
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC,
            0o644,
        )
    };

    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(fd)
}

/// The reader/writer lock that admitted compilation holds a share of.
///
/// A second file, distinct from `brokkr.lock`, because the two locks answer
/// different questions and a process needs one while another holds the other.
/// **Never unlinked and never replaced**: the proof that a drain succeeded is
/// that an exclusive flock was obtained on *this inode*, so a fresh inode at
/// the same path would silently split the exclusion in two.
pub fn compile_lock_path() -> Result<PathBuf, DevError> {
    // `lock_path` first: it creates the shared directory and owns the error.
    lock_path()?;
    crate::user_dirs::compile_lock_at(lock_home().as_deref()).ok_or_else(|| {
        DevError::Lock(LockError::from(
            "$HOME is not set or is empty - cannot locate the compile lock",
        ))
    })
}

/// Capture `std::env::args()` minus `argv[0]`, shell-quoting any element that
/// contains whitespace or a double-quote so the joined string is unambiguous.
fn path_to_cstring(path: &std::path::Path) -> Result<std::ffi::CString, DevError> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        DevError::Lock(LockError::from(format!(
            "lock path contains nul byte: {}",
            crate::output::path(path)
        )))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::record::parse_lock_contents;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Per-test scratch lock file. Each test gets its own directory, so
    /// parallel test threads never contend (or falsely nest) with each other,
    /// nor wait in one admission order through the queue beside the lock
    /// ([`queue_dir_for`]), and none of them ever touch the real global lock.
    fn tmp_lock(name: &str) -> PathBuf {
        crate::test_scratch::scratch("lockfile", &format!("{name}.d")).join("lock")
    }

    /// A holder on `path` publishing `auth`, as a second descriptor of this
    /// process stands in for another process: flock locks are per open file
    /// description, so the two contend exactly as two processes would.
    fn foreign_holder(path: &Path, auth: Option<&AuthHash>) -> OwnedFd {
        std::fs::write(
            path,
            format!(
                "pid=1\nproject=outer\ncommand=run\nauth={}\n",
                auth.map_or("", AuthHash::as_str)
            ),
        )
        .unwrap();
        let fd = open_lock_file(&path_to_cstring(path).unwrap()).unwrap();
        assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    fn waiter(path: &Path) -> OwnedFd {
        let fd = open_lock_file(&path_to_cstring(path).unwrap()).unwrap();
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    fn waiter_record() -> queue::WaiterRecord {
        queue::WaiterRecord {
            pid: std::process::id(),
            starttime: String::new(),
            boot_id: String::new(),
            pid_ns: String::new(),
            time_ns: String::new(),
            configured: true,
            project: "test".into(),
            root: "/test".into(),
            command: "check".into(),
            args: "check".into(),
            joined: 0,
        }
    }

    /// [`wait_for_flock`] on `fd` with a scratch queue of its own.
    fn wait(
        fd: &OwnedFd,
        inherited: Option<&AuthHash>,
        queue: &Path,
    ) -> Result<(queue::Ticket, bool), DevError> {
        // Only the descendant check reads the lock path, through a probe of
        // its own; the fd names the same file in every caller here.
        let path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
            .map_err(|e| DevError::Lock(LockError::from(e.to_string())))?;
        wait_for_flock(fd.as_raw_fd(), &path, inherited, queue, &waiter_record)
    }

    /// The global lock's queue is the one beside it, so `brokkr kill` and
    /// `brokkr lock` read the queue acquisition orders by.
    #[test]
    fn the_global_locks_queue_is_the_one_beside_it() {
        assert_eq!(queue_dir_for(&lock_path().unwrap()), queue_dir());
    }


    // A waiter joins the queue before it announces itself, and a cancel
    // committed while it waits ends the wait as interrupted, without the lock.
    #[test]
    fn a_waiter_is_queued_and_a_cancel_ends_its_wait() {
        let path = tmp_lock("cancelled_wait");
        let dir = crate::test_scratch::scratch("lockfile", "cancelled-wait-queue");
        let _holder = foreign_holder(&path, None);
        let me = waiter(&path);
        let queue_dir = dir.clone();
        let waiting = std::thread::spawn(move || wait(&me, None, &queue_dir).map(|_| ()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let entry = loop {
            if let Some(entry) = queue::list(&dir).unwrap().into_iter().next() {
                break entry;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the waiter never queued"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(entry.record.unwrap().project, "test");
        assert!(matches!(
            queue::cancel(&dir, &entry.token).unwrap(),
            queue::Cancel::Committed(_)
        ));
        assert!(matches!(
            waiting.join().unwrap(),
            Err(DevError::Interrupted)
        ));
        assert!(
            queue::list(&dir).unwrap().is_empty(),
            "the waiter left the queue"
        );
    }

    // The deadlock this exists for: a brokkr whose inherited nonce is the
    // live holder's would wait on its own ancestor forever. It refuses at
    // once instead, naming the holder.
    #[test]
    fn a_descendant_of_the_holder_refuses_instead_of_waiting() {
        let path = tmp_lock("nested_refuses");
        let auth = AuthHash::parse("0123456789abcdef0123456789abcdef").unwrap();
        let _holder = foreign_holder(&path, Some(&auth));
        let me = waiter(&path);
        let started = std::time::Instant::now();
        let dir = crate::test_scratch::scratch("lockfile", "nested-queue");
        let err = wait(&me, Some(&auth), &dir)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("your own ancestor") && err.contains("brokkr run"),
            "{err}"
        );
        assert!(
            started.elapsed() < LOCK_POLL,
            "refused without a poll's sleep"
        );
    }

    // A different hold - an unrelated holder, or a later hold after the one
    // this process descends from was released - is waited for as before, and
    // the wait ends when it releases.
    #[test]
    fn an_unrelated_holder_is_waited_for() {
        let path = tmp_lock("unrelated_waits");
        let their_auth = AuthHash::parse("0123456789abcdef0123456789abcdef").unwrap();
        let my_auth = AuthHash::parse("fedcba9876543210fedcba9876543210").unwrap();
        let holder = foreign_holder(&path, Some(&their_auth));
        let me = waiter(&path);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            drop(holder);
        });
        let dir = crate::test_scratch::scratch("lockfile", "unrelated-queue");
        let (_ticket, waited) = wait(&me, Some(&my_auth), &dir).unwrap();
        assert!(waited, "a waiter that waited says so");
        release.join().unwrap();
    }

    // A holder still draining publishes an empty `auth`, which matches no
    // inherited nonce; and a process that inherited none never refuses.
    #[test]
    fn an_empty_or_absent_nonce_never_reads_as_nesting() {
        let path = tmp_lock("empty_auth_waits");
        let inherited = AuthHash::parse("0123456789abcdef0123456789abcdef").unwrap();
        let holder = foreign_holder(&path, None);
        let me = waiter(&path);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(holder);
        });
        let dir = crate::test_scratch::scratch("lockfile", "empty-auth-queue");
        wait(&me, Some(&inherited), &dir).unwrap();
        release.join().unwrap();
        drop(me);
        let _holder = foreign_holder(&path, None);
        let me = waiter(&path);
        let path_t = path.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(_holder);
            path_t
        });
        wait(&me, None, &dir).unwrap();
        release.join().unwrap();
    }

    /// A refused gate releases the lock: the next acquirer takes it at once.
    #[test]
    fn a_refused_disk_gate_releases_the_lock() {
        fn refuse(_: crate::cli::CommandId, _: &crate::project::BuildRoot) -> Result<(), DevError> {
            Err(DevError::Preflight(vec!["disk full".to_owned()]))
        }
        let path = tmp_lock("disk-gate.lock");
        let ctx = ctx();
        let refusing = HoldEffects {
            disk_gate: refuse,
            ..INERT
        };
        assert!(acquire_at(&path, &ctx, &refusing).is_err());
        assert!(
            !flock_is_held(&path),
            "a refused hold must not keep the flock"
        );
        let guard = acquire_at(&path, &ctx, &INERT).unwrap();
        drop(guard);
    }

    /// The fresh-hold hook is where the stray reap and the guard warning live,
    /// so every `acquire` caller gets them. It must run once per *fresh* hold
    /// and never on re-entry - a nested acquire inside a cohort would
    /// otherwise reap in the middle of the sweep.
    #[test]
    fn fresh_hold_hook_runs_once_and_not_on_reentry() {
        static RUNS: AtomicUsize = AtomicUsize::new(0);
        fn count() {
            RUNS.fetch_add(1, Ordering::SeqCst);
        }
        let counting = HoldEffects {
            after_fresh_hold: count,
            ..INERT
        };
        let path = tmp_lock("fresh-hook.lock");
        let outer = acquire_at(&path, &ctx(), &counting).unwrap();
        let nested = acquire_at(&path, &ctx(), &counting).unwrap();
        assert_eq!(
            RUNS.load(Ordering::SeqCst),
            1,
            "nested acquire must not re-run the hook"
        );
        drop(nested);
        drop(outer);
        let _again = acquire_at(&path, &ctx(), &counting).unwrap();
        assert_eq!(
            RUNS.load(Ordering::SeqCst),
            2,
            "a new fresh hold runs it again"
        );
    }

    /// The maintenance phase exists only on a hold acquired with it, is taken
    /// once, and a re-entered guard never gets one; a hold acquired without
    /// it has none at all.
    #[test]
    fn the_maintenance_phase_is_taken_once_and_only_where_acquired() {
        let with = HoldEffects {
            maintenance: true,
            ..INERT
        };
        let path = tmp_lock("maintenance.lock");
        let outer = acquire_at(&path, &ctx(), &with).unwrap();
        assert!(outer.take_maintenance().is_some());
        let nested = acquire_at(&path, &ctx(), &with).unwrap();
        assert!(nested.take_maintenance().is_none());
        assert!(outer.take_maintenance().is_none());
        drop((nested, outer));

        let plain = acquire_at(&path, &ctx(), &INERT).unwrap();
        assert!(plain.take_maintenance().is_none());
    }

    /// An inert test hold has no capability of its own and cannot replace the
    /// capability returned by another live hold.
    #[test]
    fn inert_hold_does_not_gain_or_replace_a_capability() {
        let _serial = crate::test_scratch::process_global_lock();
        let before = crate::hold::capability();
        let path = tmp_lock("capability.lock");
        let guard = acquire_at(&path, &ctx(), &INERT).unwrap();
        assert_eq!(
            crate::hold::capability().as_ref().map(Nonce::expose),
            before.as_ref().map(Nonce::expose)
        );
        drop(guard);
        assert_eq!(
            crate::hold::capability().as_ref().map(Nonce::expose),
            before.as_ref().map(Nonce::expose),
            "a hold without a capability must not replace another hold's nonce"
        );
    }

    /// Fixture roots are minted through detection (the only constructor),
    /// once per test process, and leaked so the context can be `'static`.
    fn ctx() -> LockContext<'static> {
        static ROOTS: std::sync::OnceLock<(
            crate::project::ProjectRoot,
            crate::project::BuildRoot,
        )> = std::sync::OnceLock::new();
        let roots =
            ROOTS.get_or_init(|| crate::project::detect_roots().expect("test cwd resolves"));
        LockContext {
            project: Some("test"),
            command: crate::cli::CommandId::Lock,
            project_root: &roots.0,
            build_root: &roots.1,
            tree: None,
        }
    }

    /// Probe whether the flock on `path` is held, from a *fresh fd* - which
    /// is exactly the position a second brokkr process (or the naive hoisted
    /// acquire) would be in, since flock treats each fd independently even
    /// within one process.
    fn flock_is_held(path: &Path) -> bool {
        let c = path_to_cstring(path).unwrap();
        let fd = open_lock_file(&c).unwrap();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        let held = ret != 0;
        if !held {
            unsafe {
                libc::flock(fd, libc::LOCK_UN);
            }
        }
        // Close the probe fd either way.
        let _close = unsafe { OwnedFd::from_raw_fd(fd) };
        held
    }

    /// The defect scenario: a cohort acquires, then a swept member acquires
    /// again. Pre-fix this self-deadlocked; post-fix the nested call returns
    /// a second handle on the same hold.
    ///
    /// Run on a worker thread under a watchdog, because the failure mode
    /// being guarded against is a *block*, not a wrong value. Asserting it
    /// inline would mean a reintroduced deadlock hangs the whole suite -
    /// which reads as broken CI rather than as this regression. The
    /// watchdog turns it back into a named failure.
    #[test]
    fn nested_acquire_reenters_the_same_hold() {
        let path = tmp_lock("nested.lock");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            let outer = acquire_at(&worker_path, &ctx(), &INERT).unwrap();
            let nested = acquire_at(&worker_path, &ctx(), &INERT).unwrap();
            tx.send((
                Arc::ptr_eq(&outer.inner, &nested.inner),
                Arc::strong_count(&outer.inner),
            ))
            .ok();
        });

        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok((same_hold, strong)) => {
                assert!(same_hold, "nested acquire must share the outer's hold");
                assert_eq!(strong, 2);
                worker.join().unwrap();
            }
            Err(_) => panic!(
                "nested acquire_at() blocked - lockfile re-entrancy is gone, so a \
                 cohort hoisting an acquire around acquiring callees will \
                 self-deadlock on a second flock fd"
            ),
        }
    }

    /// Dropping the inner guard must NOT release the flock - the cohort's
    /// whole point is that the lock spans the sweep, not each member. Only
    /// the last guard's drop releases.
    #[test]
    fn inner_drop_keeps_lock_until_outer_drop() {
        let path = tmp_lock("drop-order.lock");
        let outer = acquire_at(&path, &ctx(), &INERT).unwrap();
        let nested = acquire_at(&path, &ctx(), &INERT).unwrap();

        drop(nested);
        assert!(
            flock_is_held(&path),
            "inner drop must not release the sweep's lock"
        );

        drop(outer);
        assert!(
            !flock_is_held(&path),
            "outermost drop must release the flock"
        );
    }

    /// Guard-drop order is refcounted, not stack-ordered: releasing the
    /// *outer* handle first while the nested one lives must also keep the
    /// flock (a swept member is still running under it).
    #[test]
    fn outer_drop_before_inner_keeps_lock() {
        let path = tmp_lock("outer-first.lock");
        let outer = acquire_at(&path, &ctx(), &INERT).unwrap();
        let nested = acquire_at(&path, &ctx(), &INERT).unwrap();

        drop(outer);
        assert!(flock_is_held(&path));

        drop(nested);
        assert!(!flock_is_held(&path));
    }

    /// The mutating methods on a nested guard reach the one real hold: the
    /// lock file a concurrent `brokkr lock` reads reflects them.
    #[test]
    fn nested_guard_forwards_state_to_the_lock_file() {
        let path = tmp_lock("forwarding.lock");
        let outer = acquire_at(&path, &ctx(), &INERT).unwrap();
        let nested = acquire_at(&path, &ctx(), &INERT).unwrap();

        nested.set_child_pid(4242);
        nested.set_progress(2, 5);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("child_pid=4242"), "got: {contents}");
        assert!(contents.contains("progress=2/5"), "got: {contents}");

        // And the clear forwards too - via the outer handle, proving both
        // handles mutate the same state. Every key stays present (always-emit
        // format), but with an empty value.
        outer.clear_child_pid();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("child_pid=\n"), "got: {contents}");
    }

    /// Concurrent lanes publish one child each; clearing on one thread leaves
    /// the other's slot, and the first slot stays readable as `child_pid` by
    /// readers that know only the single key.
    #[test]
    fn child_slots_are_per_thread() {
        let path = tmp_lock("child-slots.lock");
        let guard = acquire_at(&path, &ctx(), &INERT).unwrap();
        let me = std::process::id();
        let parent = crate::process::ProcStat::read_self()
            .unwrap()
            .parent()
            .unwrap()
            .get();
        guard.set_child_pid(me);
        let other = LockGuard {
            inner: Arc::clone(&guard.inner),
        };
        std::thread::spawn(move || other.set_child_pid(parent))
            .join()
            .unwrap();
        let info = parse_lock_contents(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let pids: Vec<u32> = info.children.iter().map(|(pid, _)| *pid).collect();
        assert_eq!(pids, vec![me, parent]);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains(&format!("child_pid={me}\n")),
            "{contents}"
        );
        guard.clear_child_pid();
        let info = parse_lock_contents(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let pids: Vec<u32> = info.children.iter().map(|(pid, _)| *pid).collect();
        assert_eq!(pids, vec![parent]);
    }

    /// Re-entry matches on the lock *path* - a hold on one file must not
    /// satisfy an acquire on another (the test seam depends on this, and it
    /// keeps the registry honest if a second lock file ever exists).
    #[test]
    fn different_paths_do_not_alias() {
        let path_a = tmp_lock("distinct-a.lock");
        let path_b = tmp_lock("distinct-b.lock");
        let a = acquire_at(&path_a, &ctx(), &INERT).unwrap();
        let b = acquire_at(&path_b, &ctx(), &INERT).unwrap();
        assert!(!Arc::ptr_eq(&a.inner, &b.inner));
        assert_eq!(Arc::strong_count(&a.inner), 1);
        assert_eq!(Arc::strong_count(&b.inner), 1);
    }

    /// A fresh acquire after full release takes a fresh hold (the dead
    /// registry entry is swept, not resurrected).
    #[test]
    fn reacquire_after_release_is_a_fresh_hold() {
        let path = tmp_lock("reacquire.lock");
        let first = acquire_at(&path, &ctx(), &INERT).unwrap();
        drop(first);
        assert!(!flock_is_held(&path));

        let second = acquire_at(&path, &ctx(), &INERT).unwrap();
        assert_eq!(Arc::strong_count(&second.inner), 1);
        assert!(flock_is_held(&path));
    }

    /// Release invalidates the metadata (truncate-to-zero under the flock),
    /// so a reader during the handoff fails closed instead of verifying a
    /// holder that no longer holds anything.
    #[test]
    fn release_truncates_metadata() {
        let path = tmp_lock("release-truncate.lock");
        let guard = acquire_at(&path, &ctx(), &INERT).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().is_empty());
        drop(guard);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    /// The holder's own identity tokens verify against its live /proc entry.
    #[test]
    fn own_identity_verifies() {
        let path = tmp_lock("identity.lock");
        let _guard = acquire_at(&path, &ctx(), &INERT).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let info = parse_lock_contents(&contents).unwrap();
        assert_eq!(info.pid, std::process::id());
        let identity = info.process_identity().expect("recorded identity decodes");
        let boot = crate::process::BootId::new(&info.boot_id).expect("recorded boot id");
        assert!(
            crate::process::verify_identity(identity, &boot),
            "self-identity must verify: starttime={} boot_id={}",
            info.starttime,
            info.boot_id
        );
    }

    /// A wrong starttime or boot id fails verification - the fail-closed
    /// path every namespace/recycling case funnels into.
    #[test]
    fn wrong_tokens_fail_verification() {
        use crate::process::{BootId, ProcIdentity, StartTicks};
        let own = crate::process::ProcStat::read_self().unwrap().identity();
        let boot = crate::process::local_boot_id().unwrap();
        let wrong_start = ProcIdentity::new(own.pid(), StartTicks::new(1));
        assert!(!crate::process::verify_identity(wrong_start, &boot));
        let other_boot = BootId::new("00000000-0000-0000-0000-000000000000").unwrap();
        assert!(!crate::process::verify_identity(own, &other_boot));
        assert!(StartTicks::parse("").is_none());
        assert!(BootId::new("").is_none());
    }

    /// First occurrence wins: a stale tail (old longer record surviving
    /// between write and truncate) must not override the fresh prefix.
    #[test]
    fn first_occurrence_parsing_ignores_stale_tail() {
        let text = "pid=100\nstarttime=5\nboot_id=b\nchild_pid=\nchild_starttime=\nmock_pids=\nprogress=\nproject=fresh\ncommand=run\nroot=/r\nargs=\nchild_pid=999\nproject=stale\n";
        let info = parse_lock_contents(text).unwrap();
        assert_eq!(info.pid, 100);
        assert_eq!(info.project, "fresh");
        assert!(
            info.children.is_empty(),
            "stale child_pid tail must not win"
        );
    }

    /// Escaping round-trips values containing newlines and percent signs -
    /// a path or argv with a newline must not inject format lines.
    #[test]
    fn escape_roundtrip() {
        for v in ["plain", "with\nnewline", "50%\r\n done", "%0A literal"] {
            assert_eq!(unescape_value(&escape_value(v)), v);
        }
        assert!(!escape_value("a\nb").contains('\n'));
    }
}
