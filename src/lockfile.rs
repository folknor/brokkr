use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::error::DevError;

/// Mutable lock-file state - maintained while brokkr holds the lock so
/// `brokkr lock` (from another invocation) can see the current child PID
/// and bench-run progress.
struct LockState {
    project: String,
    command: String,
    /// Full brokkr invocation minus `argv[0]` (e.g. `add-locations-to-ways
    /// --dataset europe --bench 3`). Captured at acquire time.
    args: String,
    project_root: String,
    /// This process's `/proc/self/stat` starttime (field 22, clock ticks
    /// since boot) as a canonical decimal string. Together with `boot_id`
    /// this is the identity token readers verify before trusting the PID:
    /// a PID number alone is meaningless across PID namespaces (a sandboxed
    /// holder writes pid=2, which is kthreadd on the host) and across PID
    /// recycling. Empty when unreadable - which makes every verification
    /// fail closed, by design.
    starttime: String,
    /// `/proc/sys/kernel/random/boot_id` at acquire time. Discriminates
    /// boots: the lock file lives under `$HOME` and so survives reboot, and a
    /// starttime from a previous boot could coincidentally match an
    /// unrelated process in this one.
    boot_id: String,
    /// This acquisition's public id, naming its control socket
    /// (`crate::lock_service`). Empty for a hold that runs no service (tests).
    acq_id: String,
    /// The holder's PID-namespace inode and the time-namespace inode of the
    /// thread that read `starttime`. A reader compares both with its own
    /// before trusting any recorded PID or starttime: the PID is numbered in
    /// the holder's PID namespace, and the starttime is offset by the
    /// *reading* thread's time namespace. Empty when unreadable, which makes
    /// every namespace comparison fail closed.
    pid_ns: String,
    time_ns: String,
    /// `CODEX_THREAD_ID` / `CODEX_SESSION_ID` from the holder's environment,
    /// when set. Advisory attribution only ("the holder reports the same codex
    /// thread as you"); never an identity anything is authorised on.
    codex_thread: String,
    codex_session: String,
    /// Hash of this acquisition's capability nonce (`crate::hold`), or empty
    /// while the hold is still draining and has not minted one. Empty is what
    /// makes the drain safe: a guard that reads an empty `auth` refuses, so no
    /// long-running compile can be admitted into a window brokkr has not
    /// finished clearing. It is the anti-starvation property, obtained from the
    /// publication order rather than from a separate flag that a crashed writer
    /// could leave behind.
    auth: String,
    /// True while this hold is waiting for earlier compilation leases to drain.
    /// Published for diagnostics only - `brokkr lock` and the wait message -
    /// so a waiter can tell "brokkr is clearing compilers" from "brokkr is
    /// working".
    draining: bool,
    /// PID of the most recent child process brokkr spawned under the lock,
    /// paired with its starttime token (captured when recorded; empty if
    /// unreadable, which fails verification closed). Updated by the harness
    /// each iteration of a bench run; cleared by the orchestrator after the
    /// captured runner returns so a stale PID can't be SIGKILLed by
    /// `--hard` after the kernel has recycled it.
    child: Option<(u32, String)>,
    /// Auxiliary long-running child PIDs - mock-servers (sæhrimnir) that
    /// live across many `child` rotations, each paired with its starttime
    /// token. Plural because `service --all` keeps one mock per distinct
    /// fixture alive for the whole cohort. `brokkr kill --hard` SIGKILLs
    /// each of these alongside `child` so none leak.
    mocks: Vec<(u32, String)>,
    /// Current bench-run progress as `(run, total)` (1-based).
    progress: Option<(u32, u32)>,
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
    /// Toolchain-disable guard activated under this lock (when `disable_toolchain`
    /// is armed). Restored on drop *before* the flock is released, so the pinned
    /// rust-toolchain is moved aside for exactly the locked window. See
    /// [`crate::toolchain`].
    toolchain: Option<crate::toolchain::DisabledToolchain>,
    /// Whether this hold put its nonce into the process-global capability slot
    /// ([`HoldEffects::publish_capability`]). Only a hold that published may
    /// clear it: a test hold on a scratch path must not wipe a capability it
    /// never owned.
    owns_capability: bool,
    /// This acquisition's control socket. Dropped first on release, while the
    /// flock is still held: a stop that has not committed by then is refused.
    service: Option<crate::lock_service::Service>,
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
        has_child: state.child.is_some(),
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
        // Forget this hold's compilation capability. A child spawned after
        // release must not carry a mark for a hold that is over: the next
        // holder publishes its own nonce, and a stale mark would be refused
        // anyway - forgetting it makes that explicit rather than incidental.
        if self.owns_capability {
            crate::hold::clear_capability();
        }
        // Restore the disabled toolchain (if any) while we still hold the flock,
        // then release. Doing it before LOCK_UN keeps the moved-aside window
        // inside the locked window, so a concurrent brokkr can never observe it.
        drop(self.toolchain.take());
        // The flock is released automatically when the fd is closed, but
        // unlock explicitly for clarity. OwnedFd handles close.
        unsafe {
            libc::flock(self.fd.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// RAII lock guard. The underlying flock is released when the last guard for
/// this hold drops ([`LockInner`]'s drop); in the common non-nested case that
/// is simply this guard's drop, as before.
pub struct LockGuard {
    inner: Arc<LockInner>,
}

/// The locks this process currently holds, weakly. [`acquire`] consults this
/// to re-enter an existing hold instead of opening a second fd on the same
/// file - `flock(2)` treats each fd as an independent contender even within
/// one process, so that second fd would block forever on a lock the process
/// itself holds (the self-deadlock trap that makes naive "hoist an acquire
/// around a loop of acquiring callees" fatal).
///
/// `Weak` so the registry never extends a hold: release is driven purely by
/// guard drops. Dead entries are swept opportunistically on each access.
static HELD: Mutex<Vec<Weak<LockInner>>> = Mutex::new(Vec::new());

/// Lock the registry, treating poison as recoverable. The critical sections
/// are tiny and allocation-only; if one somehow panicked, falling back to the
/// inner value is strictly safer than skipping the re-entry check (which
/// would send a nested acquire into the flock self-deadlock).
fn held_registry() -> std::sync::MutexGuard<'static, Vec<Weak<LockInner>>> {
    HELD.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl LockGuard {
    /// Record the PID of the child process currently running under the lock
    /// (with its starttime identity token), and rewrite the lock file so
    /// concurrent `brokkr lock` invocations can see it.
    pub fn set_child_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.child = Some((pid, proc_starttime(pid).unwrap_or_default()));
            self.inner.publish(&state);
        }
    }

    /// Add an auxiliary mock-server PID. `service --all` calls this once
    /// per distinct fixture spawned over the cohort's lifetime; `sync`
    /// (run or bench) and single-script `service` call it once. `brokkr kill --hard`
    /// SIGKILLs every PID in this set alongside the child so no mock
    /// leaks when the workload child is the one written to `child_pid`.
    pub fn add_mock_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            if !state.mocks.iter().any(|(p, _)| *p == pid) {
                state
                    .mocks
                    .push((pid, proc_starttime(pid).unwrap_or_default()));
            }
            self.inner.publish(&state);
        }
    }

    /// Remove a single mock-server PID. Used when one fixture session
    /// has drained but others remain (`service --all`'s cohort-scoped
    /// fixture reuse model).
    pub fn remove_mock_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.mocks.retain(|(p, _)| *p != pid);
            self.inner.publish(&state);
        }
    }

    /// Drop all mock-server PIDs (e.g. after the suite has drained every
    /// mock gracefully).
    pub fn clear_mock_pids(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.mocks.clear();
            self.inner.publish(&state);
        }
    }

    /// Drop the workload child PID. Called by orchestrators after the
    /// captured runner returns so a stale PID can't be SIGKILLed by
    /// `--hard` once the kernel has recycled it.
    pub fn clear_child_pid(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.child = None;
            self.inner.publish(&state);
        }
    }

    /// Record current bench-run progress (1-based run index out of total).
    /// Skips the update when `total <= 1` - a lone "run 1/1" line in
    /// `brokkr lock` is noise.
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
    pub project: &'a str,
    pub command: &'a str,
    pub project_root: &'a str,
}

/// Info read back from the lock file. `starttime` and `boot_id` are the
/// holder's identity tokens; every PID here is meaningful only after
/// [`verify_identity`] passes for it - the numbers were written in the
/// holder's PID namespace, which may not be the reader's.
pub struct LockInfo {
    pub pid: u32,
    pub starttime: String,
    pub boot_id: String,
    /// The holder's public acquisition id (its control socket), PID and time
    /// namespace inodes, and advisory codex thread id. Empty when the holder did not
    /// record them (an older brokkr) or they were unreadable.
    pub acq_id: String,
    pub pid_ns: String,
    pub time_ns: String,
    pub codex_thread: String,
    /// The hash of the holder's capability nonce (`crate::hold`), empty while
    /// it drains or when unrecorded. What a waiting descendant compares its
    /// inherited nonce against.
    pub auth: String,
    pub project: String,
    pub command: String,
    pub args: String,
    pub project_root: String,
    pub child: Option<(u32, String)>,
    pub mocks: Vec<(u32, String)>,
    pub progress: Option<(u32, u32)>,
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
/// `$HOME` is the one input, and it is per-user by construction, which is what
/// keeps the file writable and the lock scoped to the user whose builds it
/// serialises. If `$HOME` is unset there is no defensible default, so this
/// errors rather than inventing one.
fn lock_path() -> Result<PathBuf, DevError> {
    let home = std::env::var("HOME")
        .map_err(|_| DevError::Lock("$HOME is not set - cannot locate the brokkr lock".into()))?;
    let dir = PathBuf::from(home).join(".brokkr");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("brokkr.lock"))
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
/// (`BenchHarness::new`) still contains its own acquire for the
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
pub fn acquire(ctx: &LockContext<'_>) -> Result<LockGuard, DevError> {
    let path = lock_path()?;
    acquire_at(&path, ctx, &HOST_EFFECTS)
}

/// This process's live hold on the global lock, as one more guard on it, or
/// `None` when the process holds nothing.
///
/// The proof `build::cargo_build` demands before it runs cargo. Asking the
/// registry rather than taking a `&LockGuard` parameter keeps the check at the
/// one choke point every build passes through, including call paths several
/// layers below the command that took the lock. The returned guard shares the
/// hold (like a nested [`acquire`]), so it cannot outlive or split it.
pub fn current_hold() -> Option<LockGuard> {
    // A committed stop admits no further work under the hold it stopped.
    if crate::shutdown::stop_committed() {
        return None;
    }
    let path = lock_path().ok()?;
    let mut held = held_registry();
    held.retain(|w| w.strong_count() > 0);
    held.iter()
        .filter_map(Weak::upgrade)
        .find(|inner| inner.path == path)
        .map(|inner| LockGuard { inner })
}

/// What a fresh hold does beyond taking the flock: the host-wide and
/// process-global effects.
///
/// A seam, so unit tests can take real flocks on scratch paths without any of
/// them. Before it existed the test entry point ran the production drain on
/// the real `~/.brokkr/compile.lock` - which, 20 s into a wait behind a live
/// build, SIGKILLs whatever the stray reaper finds on the host, rust-analyzer's
/// cargo included - and mutated the process-global capability and
/// toolchain-disable state that other tests read.
struct HoldEffects {
    /// Wait out earlier compilation leases; the returned descriptor is the
    /// exclusive lease held while the capability is published. `None` means
    /// no lease was taken (tests).
    drain: fn() -> Result<Option<OwnedFd>, DevError>,
    /// Activate the armed toolchain-disable ([`crate::toolchain`]).
    toolchain: fn() -> Result<Option<crate::toolchain::DisabledToolchain>, DevError>,
    /// Install the minted nonce as this process's capability (`crate::hold`).
    publish_capability: bool,
    /// Runs once the fresh hold is registered: the stray reap and the stale
    /// guard warning.
    after_fresh_hold: fn(),
    /// Refuse the hold when the disks the command writes to are nearly full
    /// ([`crate::disk_gate`]).
    disk_gate: fn(&LockContext<'_>) -> Result<(), DevError>,
    /// Run this acquisition's control socket (`crate::lock_service`).
    service: bool,
}

/// The effects a real acquisition has.
const HOST_EFFECTS: HoldEffects = HoldEffects {
    drain: host_drain,
    toolchain: crate::toolchain::activate_for_lock,
    publish_capability: true,
    after_fresh_hold: host_after_fresh_hold,
    disk_gate: crate::disk_gate::check,
    service: true,
};

/// Refuse to start or continue an acquisition once this process has committed
/// to stopping. Checked before re-entry, after the flock wait and after the
/// drain, so no path into a hold can bypass an accepted `brokkr kill`.
fn refuse_if_stopping() -> Result<(), DevError> {
    if crate::shutdown::stop_committed() {
        return Err(DevError::Interrupted);
    }
    Ok(())
}

fn host_drain() -> Result<Option<OwnedFd>, DevError> {
    crate::check_cmd::activate_run_log();
    drain_compile_leases().map(Some)
}

/// Under the lock, so the scan cannot mistake another brokkr's cargo for a
/// stray: any cargo alive now with no brokkr ancestor is one nothing
/// brokkr-shaped started, and it holds (or will take) the build-directory lock
/// this command's cargo needs. A stale enrolled guard fails in shapes that read
/// as anything but staleness (a refused probe even poisons cargo's rustc-info
/// cache), so say it plainly, once, while a build may be about to run.
///
/// The orphan reap is the stray reap's complement for what a name cannot
/// match: test processes left behind by a brokkr that died by SIGKILL
/// (`crate::test_orphans`). Its liveness proof is a per-process flock, not
/// this hold, so it is only placed here, not dependent on it.
fn host_after_fresh_hold() {
    crate::stray::reap_after_lock();
    crate::test_orphans::reap_after_lock();
    crate::guard::warn_if_guard_stale();
}

/// Path-explicit body of [`acquire`], parameterised by its [`HoldEffects`] -
/// the unit-test seam: tests pass inert effects and a scratch path, so they
/// exercise the flock, re-entry and metadata without draining, reaping, or
/// touching process-global state.
fn acquire_at(
    path: &Path,
    ctx: &LockContext<'_>,
    effects: &HoldEffects,
) -> Result<LockGuard, DevError> {
    refuse_if_stopping()?;
    {
        let mut held = held_registry();
        held.retain(|w| w.strong_count() > 0);
        for weak in held.iter() {
            if let Some(inner) = weak.upgrade()
                && inner.path == *path
            {
                return Ok(LockGuard { inner });
            }
        }
    }

    // Refuse a *fresh* hold from inside an admitted compilation. Placed after
    // the re-entry check above, which is the legitimate nested case, and before
    // any flock, which is the deadlock: an admitted rustc holds a share of the
    // compile lease, and a brokkr started from inside it - a proc macro that
    // shells out to cargo - would take `brokkr.lock` and then wait forever for
    // an exclusive compile lease that its own waiting parent is holding a share
    // of. Refusing loses that build; blocking loses the machine.
    //
    // The marker is a convention, not a kernel fact: scrubbing the environment
    // drops it while leaving the inherited descriptor open, and a helper can
    // retain it after closing its own. It is the conservative direction in both
    // cases - a refusal, never a hang.
    if crate::hold::inside_admitted_compilation() {
        return Err(DevError::Lock(format!(
            "refusing to take the brokkr lock from inside a compilation brokkr admitted \
             ({} is set). Taking it here would deadlock: this compiler holds a share of the \
             compile lease that a new hold must drain. Run the brokkr command outside the \
             build - a build script or proc macro cannot drive brokkr.",
            crate::hold::LEASE_MARKER_ENV
        )));
    }

    let c_path = path_to_cstring(path)?;
    let fd = open_lock_file(&c_path)?;

    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    wait_for_flock(owned.as_raw_fd(), crate::hold::inherited_capability_hash().as_deref())?;
    // A stop accepted while this thread waited on the flock.
    refuse_if_stopping()?;
    // The control socket exists before the drain, so a hold that is clearing
    // compilers can already be asked about and stopped. Holding the flock
    // proves every other acquisition is over, so their leftover sockets go
    // first. Declared after `owned`, so on any error below it drops (refusing
    // further stops) before the flock is released.
    let service = if effects.service {
        crate::lock_service::sweep_leftovers();
        let id = crate::lock_service::mint_id().ok_or_else(|| {
            DevError::Lock("could not read /dev/urandom to mint a lock acquisition id".into())
        })?;
        let identity = identity_of(ctx);
        Some((id.clone(), crate::lock_service::Service::start(&id, identity)?))
    } else {
        None
    };
    let acq_id = service.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
    let service = service.map(|(_, s)| s);
    // The free-space gate. After the flock, not before: the wait may have been
    // behind a `brokkr clean` that freed the space, or a build that used it.
    // Before the drain, so a refusal costs nothing but the release, which
    // dropping `owned` does.
    (effects.disk_gate)(ctx)?;
    // We hold `brokkr.lock`, but the hold is not yet usable: compilers admitted
    // under an earlier hold may still be running. On failure `owned` drops here,
    // releasing `brokkr.lock`, and we never reach protected work - measuring
    // alongside a compiler we failed to clear is the one outcome worse than not
    // measuring at all.
    let Authorized { state, nonce, toolchain } =
        drain_and_authorize(owned.as_raw_fd(), ctx, effects, &acq_id)?;
    // A stop accepted during the drain: never authorise work after it.
    // `toolchain` drops here, restoring the moved-aside toolchain.
    refuse_if_stopping()?;
    if let Some(service) = &service {
        service.update(snapshot_of(&state));
    }

    // Construct the owner first, then hand the process registry the nonce.
    // Nothing fallible may sit between these two statements: an error there
    // would leave a capability installed that no `Drop` will ever clear.
    let inner = Arc::new(LockInner {
        fd: owned,
        path: path.to_owned(),
        state: Mutex::new(state),
        toolchain,
        owns_capability: effects.publish_capability,
        service,
    });
    if effects.publish_capability {
        crate::hold::publish_capability(&nonce);
    }
    // Register weakly so a later acquire in this process re-enters this hold
    // instead of self-deadlocking on a second fd.
    held_registry().push(Arc::downgrade(&inner));
    let guard = LockGuard { inner };
    (effects.after_fresh_hold)();
    Ok(guard)
}

/// How often a waiting acquisition retries the flock and re-reads the holder.
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Take the exclusive flock on `fd`, waiting for the current holder if there is
/// one - unless that holder is this process's own ancestor.
///
/// # Why a waiting descendant refuses
///
/// A brokkr started by a command that holds the lock - `brokkr run brokkr --
/// test NAME`, a script check or a test that shells out to brokkr - would block
/// on the flock while its ancestor waits for it to exit: neither moves, and the
/// machine-wide lock stays held until someone kills them (it sat 22 minutes
/// once). The ancestor is recognisable without any process ancestry, which a
/// PID namespace hides: every child of a hold inherits that hold's nonce, and
/// the lock record publishes the nonce's hash (`crate::hold`). `inherited` is
/// this process's inherited hash; a holder publishing the same one is the hold
/// this process descends from.
///
/// That match proves authorization, not deadlock: an ancestor could release
/// and only then wait. Refusing that case loses an execution that would have
/// worked; waiting in the common case loses the machine. The refusal says what
/// it knows and no more.
///
/// # Why poll rather than block
///
/// The record can be unreadable for a moment while the holder rewrites it
/// (child pid, progress), and a single look before a blocking `flock` would
/// then commit to the very wait this exists to prevent. Each round retries the
/// flock, then re-reads the holder; acquisition wins over whatever stale record
/// is on disk (a dead ancestor's flock is gone with it - the descriptor is
/// close-on-exec). A shutdown requested while waiting ends the wait.
fn wait_for_flock(fd: RawFd, inherited: Option<&str>) -> Result<(), DevError> {
    let mut announced = false;
    let mut rounds = 0u32;
    let began = Instant::now();
    loop {
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            if announced {
                crate::output::lock_msg(&format!(
                    "acquired after {}",
                    format_duration(began.elapsed().as_secs())
                ));
            }
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => {}
            Some(libc::EINTR) => continue,
            _ => return Err(DevError::Lock(format!("flock failed: {err}"))),
        }
        let holder = read_lock_contents(fd);
        if let (Some(mine), Some(info)) = (inherited, &holder)
            && !info.auth.is_empty()
            && info.auth == mine
        {
            return Err(DevError::Lock(nested_refusal(info)));
        }
        rounds += 1;
        // A record read mid-rewrite is `None`; give it a few rounds to settle
        // so the announcement can name the holder, then announce regardless.
        if !announced && (holder.is_some() || rounds >= 5) {
            announced = true;
            // Terse: how busy the holder is lives in `brokkr lock`, which
            // re-samples on every invocation.
            let who = holder.as_ref().and_then(holder_blurb);
            crate::output::lock_msg(&waiting_line(who.as_deref()));
            if let Some(hint) = holder.as_ref().and_then(same_agent_hint) {
                crate::output::lock_msg(&hint);
            }
        }
        if crate::shutdown::is_shutdown_requested() || crate::shutdown::stop_committed() {
            return Err(DevError::Interrupted);
        }
        std::thread::sleep(LOCK_POLL);
    }
}

/// `held by brokkr check (pbfhogg), 4m` - the holder's command, project and
/// how long its process has run. `None` unless the holder's identity verifies
/// from this namespace ([`shares_namespaces`] and [`verify_identity`]): an
/// unverifiable record may belong to a recycled PID or another boot, and the
/// wait message then says only that the lock is busy.
fn holder_blurb(info: &LockInfo) -> Option<String> {
    if info.command.is_empty() || !shares_namespaces(info) {
        return None;
    }
    let up = verified_uptime(info.pid, &info.starttime, &info.boot_id)?;
    let project = match info.project.as_str() {
        // "brokkr" is the no-project placeholder (`acquire_cmd_lock_opt`).
        "" | "unknown" | "brokkr" => String::new(),
        p => format!(" ({p})"),
    };
    Some(format!("held by brokkr {}{project}, {up}", info.command))
}

/// The wait announcement, with the holder blurb when one could be verified.
fn waiting_line(who: Option<&str>) -> String {
    match who {
        Some(who) => format!("waiting for the brokkr lock ({who}) ..."),
        None => "waiting for the brokkr lock ...".to_owned(),
    }
}

/// The refusal for a descendant of the live holder.
fn nested_refusal(holder: &LockInfo) -> String {
    format!(
        "cannot take the brokkr lock independently while the hold you were started under is \
         active - it is held by your own ancestor (`brokkr {}`), which waiting here would never \
         let go of. Run the inner command outside the outer one: run the built binary directly \
         (for brokkr itself, build it and invoke `target/debug/brokkr ...`, or `brokkr install` \
         and invoke the installed one), or run it before or after the outer command, not \
         beneath it.",
        holder.command
    )
}

/// Check the global lock status. Returns `None` if no lock is held.
///
/// The flock is the sole authority on held/not-held: the non-blocking probe
/// either succeeds (no holder - a dead process cannot retain a flock, so a
/// leftover file with no flock is simply not held) or fails (a live process
/// holds it). There is deliberately no PID-liveness fallback and no
/// stale-file deletion: the recorded PID is namespace-local, so "that PID
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

    // Try to acquire - if we succeed, no one holds it.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    if ret == 0 {
        // We got the lock → no one was holding it. Release and close.
        // SAFETY: valid fd from open_lock_file, unique ownership.
        let _close = unsafe { OwnedFd::from_raw_fd(fd) };
        return Ok(None);
    }

    // Only contention means held. Any other flock failure is infrastructure,
    // and reporting it as a holder would invent one.
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
        // SAFETY: valid fd from open_lock_file, unique ownership.
        let _close = unsafe { OwnedFd::from_raw_fd(fd) };
        return Err(DevError::Lock(format!("could not probe the lock: {err}")));
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
            auth: String::new(),
            project: "unknown".into(),
            command: "unknown".into(),
            args: String::new(),
            project_root: "unknown".into(),
            child: None,
            mocks: Vec::new(),
            progress: None,
        }));
    };

    Ok(Some(info))
}

/// Verify that `/proc/{pid}` in *this* namespace describes the process the
/// recorded tokens were captured from. True only when the recorded boot id
/// matches this kernel's and the PID's current starttime equals the recorded
/// one exactly (canonical decimal ticks - fixed at process creation, so it
/// never jitters; exact equality is what defeats PID recycling).
///
/// A failure means "identity could not be verified from this namespace" -
/// PID namespace, time namespace (the kernel offsets displayed starttimes by
/// the reader's timens), another boot, a recycled PID, hidepid, or torn
/// metadata. It deliberately does not distinguish which.
pub fn verify_identity(pid: u32, starttime: &str, boot_id: &str) -> bool {
    if pid == 0 || starttime.is_empty() || boot_id.is_empty() {
        return false;
    }
    match local_boot_id() {
        Some(local) if local == boot_id => {}
        _ => return false,
    }
    proc_starttime(pid).as_deref() == Some(starttime)
}

/// Open a pidfd on a recorded process, authenticated: verify -> `pidfd_open`
/// -> re-verify. The numeric `/proc/{pid}` still described the recorded
/// process generation when the pidfd was opened, and from then on the pidfd
/// cannot be redirected by PID recycling. `None` covers every failure -
/// unverifiable identity, a process already gone, or a PID that changed
/// generation mid-check.
pub fn open_verified_pidfd(
    pid: u32,
    starttime: &str,
    boot_id: &str,
) -> Result<std::os::fd::OwnedFd, PidfdRefusal> {
    if !verify_identity(pid, starttime, boot_id) {
        return Err(PidfdRefusal::Unverified);
    }
    let Some(pidfd) = pidfd_open(pid) else {
        return Err(PidfdRefusal::OpenFailed);
    };
    if proc_starttime(pid).as_deref() != Some(starttime) {
        return Err(PidfdRefusal::IdentityChanged);
    }
    Ok(pidfd)
}

/// Why [`open_verified_pidfd`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidfdRefusal {
    /// The recorded tokens do not describe any process visible here.
    Unverified,
    /// `pidfd_open` failed: the process is gone or namespace-isolated.
    OpenFailed,
    /// The PID changed generation between the two checks.
    IdentityChanged,
}

/// `pidfd_open(2)` via raw syscall (no libc wrapper yet).
fn pidfd_open(pid: u32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: a plain syscall with no pointer arguments.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.cast_signed(), 0u32) };
    let fd = i32::try_from(ret).ok()?;
    if fd < 0 {
        return None;
    }
    // SAFETY: a successful pidfd_open returns a fresh fd we uniquely own.
    Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// `pidfd_send_signal(2)` via raw syscall. `Ok(false)` when the process has
/// already exited (ESRCH); any other failure is returned for the caller to
/// judge.
pub fn pidfd_send_signal(
    pidfd: &std::os::fd::OwnedFd,
    signal: libc::c_int,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: the fd is a live pidfd we own; a null siginfo is permitted.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if ret == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(err)
}

/// This kernel's boot id, trimmed.
pub fn local_boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_owned())
}

/// A PID's starttime (field 22 of `/proc/{pid}/stat`, clock ticks since
/// boot) as a canonical decimal string. Validated as `u64` and re-rendered,
/// never routed through floating point - this is an identity token, not a
/// duration.
pub fn proc_starttime(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let comm_end = stat.rfind(')')?;
    let fields: Vec<&str> = stat[comm_end + 2..].split_whitespace().collect();
    // Field 19 after comm (index 19 in the post-comm fields) is starttime.
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    Some(ticks.to_string())
}

/// Get how long a verified process has been running, as a human-readable
/// string. `None` when identity verification fails - an unverified PID's
/// uptime is some other process's uptime (in the kthreadd case, the whole
/// machine's).
pub fn verified_uptime(pid: u32, starttime: &str, boot_id: &str) -> Option<String> {
    if !verify_identity(pid, starttime, boot_id) {
        return None;
    }
    process_uptime_str(pid)
}

/// Get how long a process has been running, as a human-readable string.
///
/// Reads `/proc/{pid}/stat` starttime and compares against system uptime.
/// Display only - callers gate on [`verify_identity`] first (or use
/// [`verified_uptime`] / [`verified_summary`]).
fn process_uptime_str(pid: u32) -> Option<String> {
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    if clk_tck <= 0.0 {
        return None;
    }

    // System uptime in seconds.
    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;

    // Process start time in clock ticks since boot.
    let starttime: f64 = proc_starttime(pid)?.parse().ok()?;

    let start_secs = starttime / clk_tck;
    let elapsed_secs = uptime_secs - start_secs;

    if elapsed_secs < 0.0 {
        return None;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let elapsed = elapsed_secs as u64;
    Some(format_duration(elapsed))
}

/// Format a second count as `3h05m` / `3m12s` / `42s`.
pub fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Build a one-line summary of a running, identity-verified process from
/// `/proc`.
///
/// Returns something like `"running 12m, RSS 4.2 GB, 847 MB read, 4 threads"`.
/// Returns `None` if verification fails, the process is gone, or `/proc` is
/// unreadable. Identity is checked *before and after* collection: the
/// process could exit and its PID be recycled while `/proc/{pid}/status`
/// and `/proc/{pid}/io` are being read, and stats half-read from two
/// different processes must not be displayed.
pub fn verified_summary(pid: u32, starttime: &str, boot_id: &str) -> Option<String> {
    if !verify_identity(pid, starttime, boot_id) {
        return None;
    }
    let summary = process_summary_unverified(pid)?;
    // Re-verify: same starttime means the stats above came from the same
    // process generation.
    if proc_starttime(pid).as_deref() != Some(starttime) {
        return None;
    }
    Some(summary)
}

/// Stat collection body of [`verified_summary`]. Never call for display
/// without the verification sandwich around it.
fn process_summary_unverified(pid: u32) -> Option<String> {
    let uptime = process_uptime_str(pid)?;

    // Read /proc/{pid}/status for RSS.
    let status_path = format!("/proc/{pid}/status");
    let status_text = std::fs::read_to_string(&status_path).ok()?;
    let mut rss_kb: i64 = 0;
    let mut threads: i64 = 0;
    for line in status_text.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            let val_str = rest.trim().trim_end_matches(" kB");
            match key {
                "VmRSS" => rss_kb = val_str.parse().unwrap_or(0),
                "Threads" => threads = val_str.parse().unwrap_or(0),
                _ => {}
            }
        }
    }

    // Read /proc/{pid}/io for bytes read.
    let io_path = format!("/proc/{pid}/io");
    let mut read_bytes: i64 = 0;
    let mut write_bytes: i64 = 0;
    if let Ok(io_text) = std::fs::read_to_string(&io_path) {
        for line in io_text.lines() {
            if let Some((key, rest)) = line.split_once(':') {
                let val: i64 = rest.trim().parse().unwrap_or(0);
                match key {
                    "read_bytes" => read_bytes = val,
                    "write_bytes" => write_bytes = val,
                    _ => {}
                }
            }
        }
    }

    let mut parts = Vec::with_capacity(5);
    parts.push(format!("running {uptime}"));

    if rss_kb > 0 {
        parts.push(format_bytes_kb(rss_kb, "RSS"));
    }
    if read_bytes > 0 {
        parts.push(format_bytes(read_bytes, "read"));
    }
    if write_bytes > 0 {
        parts.push(format_bytes(write_bytes, "written"));
    }
    if threads > 1 {
        parts.push(format!("{threads} threads"));
    }

    Some(parts.join(", "))
}

/// Format kB as human-readable (e.g. "RSS 4.2 GB").
fn format_bytes_kb(kb: i64, label: &str) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = kb as f64 / 1024.0;
    if mb >= 1024.0 {
        format!("{label} {:.1} GB", mb / 1024.0)
    } else {
        format!("{label} {mb:.0} MB")
    }
}

/// Format bytes as human-readable (e.g. "847 MB read").
fn format_bytes(bytes: i64, label: &str) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB {label}", mb / 1024.0)
    } else {
        format!("{mb:.0} MB {label}")
    }
}

/// Open (or create) the lock file, returning the raw fd.
fn open_lock_file(c_path: &std::ffi::CString) -> Result<RawFd, DevError> {
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC,
            0o644,
        )
    };

    if fd < 0 {
        return Err(DevError::Lock(format!(
            "failed to open lock file: {}",
            std::io::Error::last_os_error()
        )));
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
    Ok(lock_path()?.with_file_name("compile.lock"))
}

/// How long a fresh acquisition will wait for earlier compilation leases to
/// drain before giving up. Split into two windows: after the first, the stray
/// reaper runs once (a foreign compiler holding a lease is exactly what it
/// exists to kill), then the wait resumes.
const DRAIN_BUDGET: Duration = Duration::from_secs(120);
const DRAIN_REAP_AFTER: Duration = Duration::from_secs(20);

/// Wait for every participating compilation lease to be released, and return
/// the exclusive descriptor that proves it.
///
/// The returned fd is held only long enough to publish the new capability, then
/// dropped so this hold's own descendants can take shares of it.
///
/// # What success proves, and what it does not
///
/// Linux does not permit a shared and an exclusive flock on one file to
/// coexist, so obtaining the exclusive lock proves no independent share
/// remained at that instant. It proves this only for compilation that
/// *participates* - that took a lease and retained it through execution.
/// A compiler whose guard could not open this file at all fails open by design
/// (see `src/bin/rustc_guard.rs`) and is outside the proof.
///
/// # Why it can fail
///
/// A lease lives as long as *any* process retains the open file description,
/// which a detached helper can extend without bound while carrying a name no
/// cargo-family scan will ever match. So this cannot be written as a wait that
/// must eventually succeed: the budget expiring is a real outcome, and the
/// caller must release `brokkr.lock` and refuse to proceed rather than
/// measuring alongside a compiler it failed to clear.
fn drain_compile_leases() -> Result<OwnedFd, DevError> {
    let path = compile_lock_path()?;
    let c_path = path_to_cstring(&path)?;
    let fd = open_lock_file(&c_path)?;
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    let start = Instant::now();
    let mut announced = false;
    let mut reaped = false;
    loop {
        let rc = unsafe { libc::flock(owned.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(owned);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => {}
            Some(libc::EINTR) => continue,
            // Any other flock failure is infrastructure, not contention. The
            // holder side fails closed here on purpose: brokkr must never run
            // protected work believing it has exclusion it never obtained.
            _ => {
                return Err(DevError::Lock(format!(
                    "could not take the compile lease lock at {}: {err}",
                    path.display()
                )));
            }
        }
        if !announced {
            announced = true;
            crate::output::lock_msg(
                "waiting for compilers started under the previous hold to finish - brokkr clears them before measuring",
            );
        }
        let waited = start.elapsed();
        if waited >= DRAIN_BUDGET {
            return Err(DevError::Lock(format!(
                "compilation leases did not drain within {}s. A compiler admitted under an \
                 earlier hold is still holding its lease, or a process that inherited its \
                 descriptor is. This is not a transient busy signal - an unchanged blocker \
                 will fail every retry. `brokkr strays` shows what is running; \
                 `brokkr lock` shows the holder.",
                DRAIN_BUDGET.as_secs()
            )));
        }
        if !reaped && waited >= DRAIN_REAP_AFTER {
            reaped = true;
            crate::output::lock_msg(
                "compilers have not drained; reaping strays once, then waiting again",
            );
            crate::stray::reap_for_drain();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Turn a freshly-taken `brokkr.lock` into a usable hold: publish who we are,
/// drain earlier compilation leases, then mint and publish this acquisition's
/// capability. Returns the published state.
///
/// The order is the protocol, and every step depends on the one before it:
///
/// 1. Publish identity with `auth=` empty and `draining=1`. A guard reading an
///    empty `auth` refuses, so no new compiler can be admitted while we clear -
///    which is the anti-starvation property, obtained from the publication order
///    rather than from a separate intent flag a crashed writer could leave
///    behind. It also lets `brokkr lock` and `kill` see who is draining. This
///    publication must succeed: a hold whose metadata never existed is
///    indistinguishable from torn metadata for every reader, and `kill` could
///    never target it.
/// 2. Drain, and hold the exclusive compile lease.
/// 3. Mint the capability and publish its hash *while still holding that
///    exclusive lease*. A guard paused before taking its share resumes after we
///    release, reads the new hash under its share, and so cannot turn an earlier
///    hold's nonce into an admitted compiler.
/// 4. Release the exclusive lease, so this hold's own descendants can take
///    shares of it.
fn drain_and_authorize(
    fd: RawFd,
    ctx: &LockContext<'_>,
    effects: &HoldEffects,
    acq_id: &str,
) -> Result<Authorized, DevError> {
    let mut state = build_state(ctx, acq_id);
    rewrite_from_state(fd, &state)
        .map_err(|e| DevError::Lock(format!("failed to publish lock metadata: {e}")))?;

    let compile_lease = (effects.drain)()?;

    let nonce = crate::hold::mint_nonce().ok_or_else(|| {
        DevError::Lock(
            "could not read /dev/urandom to mint a compilation capability; a hold that cannot \
             mint one would have every one of its own compiles refused"
                .into(),
        )
    })?;
    state.auth = crate::hold::auth_hash(&nonce);
    state.draining = false;
    rewrite_from_state(fd, &state)
        .map_err(|e| DevError::Lock(format!("failed to publish the compilation capability: {e}")))?;

    // Activate the armed toolchain-disable here: after the drain, so the
    // moved-aside window never spans a wait on other processes, and *before*
    // the capability enters the process registry, so a failure cannot leave a
    // nonce installed with no `LockInner` to own it. That was a real defect -
    // publishing the capability and then doing fallible work meant an
    // activation failure released `brokkr.lock` without ever constructing the
    // guard whose `Drop` clears the registry, and the process went on able to
    // stamp a nonce belonging to no hold.
    let toolchain = (effects.toolchain)()?;

    drop(compile_lease);
    Ok(Authorized { state, nonce, toolchain })
}

/// A drained, authorized hold, ready for its [`LockInner`] to take ownership.
///
/// The nonce is carried out rather than published here on purpose: nothing
/// fallible may run between "the process registry names this nonce" and "a
/// `LockInner` exists whose `Drop` clears it".
struct Authorized {
    state: LockState,
    nonce: String,
    toolchain: Option<crate::toolchain::DisabledToolchain>,
}

/// Build the initial `LockState` for a freshly-acquired lock. Captures the
/// current brokkr invocation args (argv minus `argv[0]`) so `brokkr lock`
/// can show exactly what the user typed, plus the identity tokens readers
/// verify before trusting the PID.
fn build_state(ctx: &LockContext<'_>, acq_id: &str) -> LockState {
    let identity = identity_of(ctx);
    LockState {
        project: identity.project,
        command: identity.command,
        args: identity.args,
        project_root: identity.root,
        // `/proc/self`, not `/proc/<getpid()>`: under a procfs mounted for an
        // ancestor namespace the numeric path names some other process.
        starttime: self_starttime().unwrap_or_default(),
        boot_id: local_boot_id().unwrap_or_default(),
        acq_id: acq_id.to_owned(),
        pid_ns: identity.pid_ns,
        time_ns: identity.time_ns,
        codex_thread: identity.codex_thread,
        codex_session: identity.codex_session,
        auth: String::new(),
        draining: true,
        child: None,
        mocks: Vec::new(),
        progress: None,
    }
}

/// What this process states about itself as a lock holder, for the lock file
/// and the control socket alike.
fn identity_of(ctx: &LockContext<'_>) -> crate::lock_service::Identity {
    let ns = |n: Option<u64>| n.map(|v| v.to_string()).unwrap_or_default();
    crate::lock_service::Identity {
        project: ctx.project.to_owned(),
        command: ctx.command.to_owned(),
        args: current_invocation_args(),
        root: ctx.project_root.to_owned(),
        pid_ns: ns(ns_inode("/proc/self/ns/pid")),
        // The thread reading `starttime` is this one, and its time namespace
        // is what offsets the value it reads.
        time_ns: ns(ns_inode("/proc/thread-self/ns/time")),
        codex_thread: std::env::var("CODEX_THREAD_ID").unwrap_or_default(),
        codex_session: std::env::var("CODEX_SESSION_ID").unwrap_or_default(),
    }
}

/// The inode of a namespace link (`/proc/self/ns/pid` and kin).
pub fn ns_inode(path: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.ino())
}

/// This process's starttime token, read through `/proc/self`.
fn self_starttime() -> Option<String> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let comm_end = stat.rfind(')')?;
    let fields: Vec<&str> = stat.get(comm_end + 2..)?.split_whitespace().collect();
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    Some(ticks.to_string())
}

/// Whether recorded PIDs and starttimes mean the same thing here as they did
/// to the holder: its PID namespace is ours, the thread that read its
/// starttime shared our time namespace, and this `/proc` numbers processes for
/// our own PID namespace (`/proc/1` is our namespace's init). Every numeric
/// verification and signal is gated on this; anything unreadable is `false`.
///
/// A holder that recorded no namespaces (an older brokkr) is judged by the
/// procfs check alone, which is what verification relied on before.
pub fn shares_namespaces(info: &LockInfo) -> bool {
    let own_pid = ns_inode("/proc/self/ns/pid");
    if own_pid.is_none() || ns_inode("/proc/1/ns/pid") != own_pid {
        return false;
    }
    if info.pid_ns.is_empty() && info.time_ns.is_empty() {
        return true;
    }
    let own_time = ns_inode("/proc/thread-self/ns/time");
    own_pid.map(|v| v.to_string()).as_deref() == Some(info.pid_ns.as_str())
        && own_time.map(|v| v.to_string()).as_deref() == Some(info.time_ns.as_str())
}

/// The holder's PID namespace inode when it differs from this process's, i.e.
/// the holder runs sandboxed relative to us. `None` when it is ours, or when
/// either side is unknown.
pub fn foreign_holder_ns(info: &LockInfo) -> Option<u64> {
    let holder: u64 = info.pid_ns.parse().ok()?;
    let own = ns_inode("/proc/self/ns/pid")?;
    (holder != own).then_some(holder)
}

/// One advisory line when the holder recorded the same codex thread id as
/// this process carries: almost always an earlier exec session of this very
/// agent, which an agent otherwise has no way to recognise from inside its
/// own sandbox. Advisory, because the id is an environment variable.
pub fn same_agent_hint(info: &LockInfo) -> Option<String> {
    let mine = std::env::var("CODEX_THREAD_ID").ok()?;
    if mine.is_empty() || mine != info.codex_thread {
        return None;
    }
    Some(format!(
        "the holder reports the same codex thread as this shell ({mine}): it is very likely one of \
         your own earlier commands, still running in another exec session. Wait for it, stop that \
         session, or run `brokkr kill`"
    ))
}

/// Publish updated state to the lock file, degrading safely on failure: a
/// state update that cannot be written must not leave the *previous* state
/// standing, because that state may advertise a child or mock PID we no
/// longer track - which `kill --hard` could legitimately verify and signal.
/// So on failure the metadata is invalidated (truncated to zero, readers
/// fail closed) and the run continues; a later successful update republishes
/// in full, since every rewrite carries the complete state.
fn publish(fd: RawFd, state: &LockState) {
    if let Err(e) = rewrite_from_state(fd, state) {
        invalidate_mutable_metadata(fd, state);
        crate::output::warn_stderr(&format!("lock: failed to write lock metadata: {e}"));
    }
}

/// Clear the mutable record while preserving this hold's authorization.
///
/// Truncating the whole file here - which is what this used to do - would strip
/// `auth` from a hold that is still live and still driving compilers, so every
/// subsequent child of the *current* holder would be refused by the guard: a
/// bookkeeping failure would silently become an inability to compile. So the
/// identity and authorization block is rewritten and only the mutable tail
/// (child, mocks, progress) is cleared, which is the part that could otherwise
/// advertise a PID we no longer track to `kill --hard`.
///
/// If even that write fails there is nothing honest left but full
/// invalidation: readers fail closed, and this holder's own later compiles are
/// refused. That is the correct direction - a reader trusting a stale child PID
/// is worse than a build that stops.
fn invalidate_mutable_metadata(fd: RawFd, state: &LockState) {
    let cleared = LockState {
        project: state.project.clone(),
        command: state.command.clone(),
        args: state.args.clone(),
        project_root: state.project_root.clone(),
        starttime: state.starttime.clone(),
        boot_id: state.boot_id.clone(),
        acq_id: state.acq_id.clone(),
        pid_ns: state.pid_ns.clone(),
        time_ns: state.time_ns.clone(),
        codex_thread: state.codex_thread.clone(),
        codex_session: state.codex_session.clone(),
        auth: state.auth.clone(),
        draining: state.draining,
        child: None,
        mocks: Vec::new(),
        progress: None,
    };
    if let Err(e) = rewrite_from_state(fd, &cleared) {
        invalidate_metadata(fd);
        crate::output::warn_stderr(&format!("lock: failed to preserve lock authorization: {e}"));
    }
}

/// Truncate the metadata to zero length so readers fail closed. Used on
/// release (while still holding the flock) and after a failed publish.
fn invalidate_metadata(fd: RawFd) {
    unsafe {
        if libc::ftruncate(fd, 0) == -1 {
            crate::output::warn_stderr(&format!(
                "lock: failed to invalidate lock metadata: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
}

/// Rewrite the lock file contents from the given state.
///
/// Fields are newline-separated `key=value` pairs, **every key always
/// emitted** (empty value = unset). Always-emit is load-bearing: readers
/// parse first-occurrence-wins, so a fresh empty `child_pid=` line shadows
/// any stale `child_pid=1234` tail left between the write and the truncate -
/// without it, clearing a field would leave no fresh occurrence to win.
/// Identity fields come first and are byte-identical on every rewrite by one
/// holder, so a torn read can never mix two holders' identities; the
/// unbounded `args=` line is last so truncation only ever sacrifices it.
/// Textual values are escaped ([`escape_value`]) because paths and argv can
/// contain newlines, which would otherwise inject lines into the format.
fn rewrite_from_state(fd: RawFd, state: &LockState) -> std::io::Result<()> {
    // `auth` and `draining` ride with the identity block, ahead of every
    // mutable field: the authorization record must never be the line a
    // truncation sacrifices, and it must be byte-identical across a holder's
    // rewrites so a torn read cannot blend two holders' capabilities.
    let mut contents = format!(
        "pid={}\nstarttime={}\nboot_id={}\nacq={}\npid_ns={}\ntime_ns={}\ncodex_thread={}\n\
         codex_session={}\nauth={}\ndraining={}\n",
        std::process::id(),
        state.starttime,
        state.boot_id,
        state.acq_id,
        state.pid_ns,
        state.time_ns,
        escape_value(&state.codex_thread),
        escape_value(&state.codex_session),
        state.auth,
        u8::from(state.draining),
    );
    match &state.child {
        Some((pid, st)) => {
            contents.push_str(&format!("child_pid={pid}\nchild_starttime={st}\n"));
        }
        None => contents.push_str("child_pid=\nchild_starttime=\n"),
    }
    let mocks = state
        .mocks
        .iter()
        .map(|(p, st)| format!("{p}:{st}"))
        .collect::<Vec<_>>()
        .join(",");
    contents.push_str(&format!("mock_pids={mocks}\n"));
    match state.progress {
        Some((run, total)) => contents.push_str(&format!("progress={run}/{total}\n")),
        None => contents.push_str("progress=\n"),
    }
    contents.push_str(&format!(
        "project={}\ncommand={}\nroot={}\nargs={}\n",
        escape_value(&state.project),
        escape_value(&state.command),
        escape_value(&state.project_root),
        escape_value(&state.args),
    ));

    // Write first, then truncate. The inverse order (truncate → write) gave
    // a concurrent `brokkr lock` reader a window to read 0 bytes and print
    // an unknown holder. Writing first means any reader sees either the old
    // full contents or a valid new prefix (plus stale trailing bytes, which
    // first-occurrence parsing renders harmless). Note this is coherence
    // best-effort, not a snapshot guarantee - readers double-read and fail
    // closed on change (see `read_lock_contents`).
    let bytes = contents.as_bytes();
    unsafe {
        if libc::lseek(fd, 0, libc::SEEK_SET) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let mut written = 0usize;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(std::io::Error::other("zero-byte write to lock file"));
        }
        #[allow(clippy::cast_sign_loss)]
        {
            written += n as usize;
        }
    }
    // Trim any stale tail from a previous longer write - only after the
    // whole buffer landed, and to the full intended length, so a failure
    // above never truncates to a partial record.
    unsafe {
        #[allow(clippy::cast_possible_wrap)]
        if libc::ftruncate(fd, bytes.len() as libc::off_t) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Capture `std::env::args()` minus `argv[0]`, shell-quoting any element that
/// contains whitespace or a double-quote so the joined string is unambiguous.
fn current_invocation_args() -> String {
    let args: Vec<String> = std::env::args().skip(1).collect();
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_owned()
    }
}

/// Percent-escape the three bytes that would break the line-oriented
/// `key=value` format: `%` (the escape itself), `\n` (line injection) and
/// `\r`. Everything else passes through, keeping the file human-readable.
fn escape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of [`escape_value`]. Unknown or truncated escapes pass through
/// literally - this decodes a display string, it must never fail.
fn unescape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            match &s[i + 1..i + 3] {
                "25" => {
                    out.push('%');
                    i += 3;
                    continue;
                }
                "0A" => {
                    out.push('\n');
                    i += 3;
                    continue;
                }
                "0D" => {
                    out.push('\r');
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        // Advance by one char (values are valid UTF-8; `%` is single-byte).
        let ch_len = s[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Read lock file contents and parse the key=value fields.
///
/// Reads the file **twice** and requires byte-identical contents, retrying a
/// few times, before parsing - stated narrowly, this rejects metadata that
/// changes between reads; it cannot prove identical reads are untorn (a
/// writer descheduled mid-write is stable). The remaining torn shapes are
/// defused by the format instead: identity fields are a byte-identical
/// prefix per holder, every key is always emitted, and parsing is
/// first-occurrence-wins so a stale tail can never override a fresh prefix.
fn read_lock_contents(fd: RawFd) -> Option<LockInfo> {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let first = read_file_bytes(fd)?;
        let second = read_file_bytes(fd)?;
        if first == second && !first.is_empty() {
            let text = std::str::from_utf8(&first).ok()?;
            return parse_lock_contents(text);
        }
    }
    None
}

/// One full read of the lock file from offset 0.
fn read_file_bytes(fd: RawFd) -> Option<Vec<u8>> {
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
    let mut contents: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if n == 0 {
            break;
        }
        let len = usize::try_from(n).ok()?;
        contents.extend_from_slice(&chunk[..len]);
        if len < chunk.len() {
            break;
        }
    }
    Some(contents)
}

/// Parse the `key=value` lines. First occurrence of each key wins (see
/// [`read_lock_contents`]); an empty or unparseable value means unset.
fn parse_lock_contents(text: &str) -> Option<LockInfo> {
    let mut fields: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fields.entry(key).or_insert(value.trim());
        }
    }

    let raw = |key: &str| fields.get(key).map_or("", |v| *v);
    let escaped = |key: &str| unescape_value(raw(key));

    let pid: u32 = raw("pid").parse().unwrap_or(0);
    let project = escaped("project");
    if pid == 0 && project.is_empty() {
        return None;
    }

    let child = raw("child_pid")
        .parse()
        .ok()
        .map(|p: u32| (p, raw("child_starttime").to_owned()));
    let mocks = raw("mock_pids")
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let (p, st) = entry.split_once(':')?;
            Some((p.trim().parse().ok()?, st.trim().to_owned()))
        })
        .collect();
    let progress = raw("progress").split_once('/').and_then(|(r, t)| {
        Some((r.parse::<u32>().ok()?, t.parse::<u32>().ok()?))
    });

    Some(LockInfo {
        pid,
        starttime: raw("starttime").to_owned(),
        boot_id: raw("boot_id").to_owned(),
        acq_id: raw("acq").to_owned(),
        pid_ns: raw("pid_ns").to_owned(),
        time_ns: raw("time_ns").to_owned(),
        codex_thread: escaped("codex_thread"),
        auth: raw("auth").to_owned(),
        project,
        command: escaped("command"),
        args: escaped("args"),
        project_root: escaped("root"),
        child,
        mocks,
        progress,
    })
}

/// Convert a `Path` to a `CString`.
fn path_to_cstring(path: &std::path::Path) -> Result<std::ffi::CString, DevError> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| DevError::Lock(format!("lock path contains nul byte: {}", path.display())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Per-test scratch lock file. Each test gets its own path so parallel
    /// test threads never contend (or falsely nest) with each other, and
    /// none of them ever touch the real global lock.
    fn tmp_lock(name: &str) -> PathBuf {
        crate::test_scratch::scratch_path("lockfile", name)
    }

    fn no_drain() -> Result<Option<OwnedFd>, DevError> {
        Ok(None)
    }

    fn no_toolchain() -> Result<Option<crate::toolchain::DisabledToolchain>, DevError> {
        Ok(None)
    }

    fn nothing_after() {}

    fn no_disk_gate(_: &LockContext<'_>) -> Result<(), DevError> {
        Ok(())
    }

    /// The effects every test hold uses: no drain of the real compile lease
    /// (and so no stray reap, which SIGKILLs host processes), no toolchain
    /// activation, no capability published into the process-global slot, no
    /// post-hold reap, and no free-space gate (a test must not fail because
    /// the host's disk is full). What remains is exactly what these tests are
    /// about: the flock, re-entry, and the lock-file metadata.
    const INERT: HoldEffects = HoldEffects {
        drain: no_drain,
        toolchain: no_toolchain,
        publish_capability: false,
        after_fresh_hold: nothing_after,
        disk_gate: no_disk_gate,
        service: false,
    };

    /// A holder on `path` publishing `auth`, as a second descriptor of this
    /// process stands in for another process: flock locks are per open file
    /// description, so the two contend exactly as two processes would.
    fn foreign_holder(path: &Path, auth: &str) -> OwnedFd {
        std::fs::write(path, format!("pid=1\nproject=outer\ncommand=run\nauth={auth}\n")).unwrap();
        let fd = open_lock_file(&path_to_cstring(path).unwrap()).unwrap();
        assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    fn waiter(path: &Path) -> OwnedFd {
        let fd = open_lock_file(&path_to_cstring(path).unwrap()).unwrap();
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    // The deadlock this exists for: a brokkr whose inherited nonce is the
    // live holder's would wait on its own ancestor forever. It refuses at
    // once instead, naming the holder.
    #[test]
    fn a_descendant_of_the_holder_refuses_instead_of_waiting() {
        let path = tmp_lock("nested_refuses");
        let _holder = foreign_holder(&path, "abc123");
        let me = waiter(&path);
        let started = std::time::Instant::now();
        let err = wait_for_flock(me.as_raw_fd(), Some("abc123")).unwrap_err().to_string();
        assert!(err.contains("your own ancestor") && err.contains("brokkr run"), "{err}");
        assert!(started.elapsed() < LOCK_POLL, "refused without a poll's sleep");
    }

    // A different hold - an unrelated holder, or a later hold after the one
    // this process descends from was released - is waited for as before, and
    // the wait ends when it releases.
    #[test]
    fn an_unrelated_holder_is_waited_for() {
        let path = tmp_lock("unrelated_waits");
        let holder = foreign_holder(&path, "theirs");
        let me = waiter(&path);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            drop(holder);
        });
        wait_for_flock(me.as_raw_fd(), Some("mine")).unwrap();
        release.join().unwrap();
    }

    // A holder still draining publishes an empty `auth`, which matches no
    // inherited nonce; and a process that inherited none never refuses.
    #[test]
    fn an_empty_or_absent_nonce_never_reads_as_nesting() {
        let path = tmp_lock("empty_auth_waits");
        let holder = foreign_holder(&path, "");
        let me = waiter(&path);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(holder);
        });
        wait_for_flock(me.as_raw_fd(), Some("")).unwrap();
        release.join().unwrap();
        drop(me);
        let _holder = foreign_holder(&path, "abc");
        let me = waiter(&path);
        let path_t = path.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(_holder);
            path_t
        });
        wait_for_flock(me.as_raw_fd(), None).unwrap();
        release.join().unwrap();
    }

    /// A refused gate releases the lock: the next acquirer takes it at once.
    #[test]
    fn a_refused_disk_gate_releases_the_lock() {
        fn refuse(_: &LockContext<'_>) -> Result<(), DevError> {
            Err(DevError::Preflight(vec!["disk full".to_owned()]))
        }
        let path = tmp_lock("disk-gate.lock");
        let ctx = LockContext {
            project: "p",
            command: "check",
            project_root: "/r",
        };
        let refusing = HoldEffects { disk_gate: refuse, ..INERT };
        assert!(acquire_at(&path, &ctx, &refusing).is_err());
        assert!(!flock_is_held(&path), "a refused hold must not keep the flock");
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
        let counting = HoldEffects { after_fresh_hold: count, ..INERT };
        let path = tmp_lock("fresh-hook.lock");
        let outer = acquire_at(&path, &ctx(), &counting).unwrap();
        let nested = acquire_at(&path, &ctx(), &counting).unwrap();
        assert_eq!(RUNS.load(Ordering::SeqCst), 1, "nested acquire must not re-run the hook");
        drop(nested);
        drop(outer);
        let _again = acquire_at(&path, &ctx(), &counting).unwrap();
        assert_eq!(RUNS.load(Ordering::SeqCst), 2, "a new fresh hold runs it again");
    }

    /// A test hold neither installs nor clears the process-global capability:
    /// it did not publish one, so it has none to forget. Before the seam the
    /// test holds did both, racing `hold`'s own capability test.
    #[test]
    fn inert_hold_leaves_the_capability_slot_alone() {
        let _serial = crate::test_scratch::process_global_lock();
        let before = crate::hold::capability();
        crate::hold::publish_capability("sentinel");
        let path = tmp_lock("capability.lock");
        let guard = acquire_at(&path, &ctx(), &INERT).unwrap();
        assert_eq!(crate::hold::capability().as_deref(), Some("sentinel"));
        drop(guard);
        assert_eq!(
            crate::hold::capability().as_deref(),
            Some("sentinel"),
            "a hold that never published must not clear the slot on release"
        );
        match before {
            Some(n) => crate::hold::publish_capability(&n),
            None => crate::hold::clear_capability(),
        }
    }

    fn ctx() -> LockContext<'static> {
        LockContext {
            project: "test",
            command: "lock-test",
            project_root: "/nonexistent",
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
        nested.add_mock_pid(5151);
        nested.set_progress(2, 5);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("child_pid=4242"), "got: {contents}");
        assert!(contents.contains("mock_pids=5151:"), "got: {contents}");
        assert!(contents.contains("progress=2/5"), "got: {contents}");

        // And the clears forward too - via the outer handle, proving both
        // handles mutate the same state. Every key stays present (always-emit
        // format), but with an empty value.
        outer.clear_child_pid();
        nested.clear_mock_pids();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("child_pid=\n"), "got: {contents}");
        assert!(contents.contains("mock_pids=\n"), "got: {contents}");
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
        assert!(
            verify_identity(info.pid, &info.starttime, &info.boot_id),
            "self-identity must verify: starttime={} boot_id={}",
            info.starttime,
            info.boot_id
        );
    }

    /// The wait line names the holder only when its identity verifies; a record
    /// with wrong tokens falls back to the plain wording.
    #[test]
    fn wait_line_names_a_verified_holder_and_fails_closed() {
        let path = tmp_lock("blurb.lock");
        let _guard = acquire_at(&path, &ctx(), &INERT).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let mut info = parse_lock_contents(&contents).unwrap();
        if shares_namespaces(&info) {
            let blurb = holder_blurb(&info).unwrap();
            assert!(blurb.starts_with("held by brokkr "), "{blurb}");
        }
        info.starttime = "1".into();
        assert_eq!(holder_blurb(&info), None);
        assert_eq!(waiting_line(None), "waiting for the brokkr lock ...");
        assert_eq!(
            waiting_line(Some("held by brokkr check (p), 4m00s")),
            "waiting for the brokkr lock (held by brokkr check (p), 4m00s) ..."
        );
    }

    /// A wrong starttime or boot id fails verification - the fail-closed
    /// path every namespace/recycling case funnels into.
    #[test]
    fn wrong_tokens_fail_verification() {
        let pid = std::process::id();
        let boot = local_boot_id().unwrap();
        let start = proc_starttime(pid).unwrap();
        assert!(!verify_identity(pid, "1", &boot));
        assert!(!verify_identity(
            pid,
            &start,
            "00000000-0000-0000-0000-000000000000"
        ));
        assert!(!verify_identity(pid, "", &boot));
        assert!(!verify_identity(pid, &start, ""));
    }

    /// First occurrence wins: a stale tail (old longer record surviving
    /// between write and truncate) must not override the fresh prefix.
    #[test]
    fn first_occurrence_parsing_ignores_stale_tail() {
        let text = "pid=100\nstarttime=5\nboot_id=b\nchild_pid=\nchild_starttime=\nmock_pids=\nprogress=\nproject=fresh\ncommand=run\nroot=/r\nargs=\nchild_pid=999\nproject=stale\n";
        let info = parse_lock_contents(text).unwrap();
        assert_eq!(info.pid, 100);
        assert_eq!(info.project, "fresh");
        assert!(info.child.is_none(), "stale child_pid tail must not win");
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
