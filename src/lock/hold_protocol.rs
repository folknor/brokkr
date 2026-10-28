use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::record::{read_lock_contents, rewrite_from_state};
use super::{
    AcqId, AuthHash, LockContext, LockGuard, LockInfo, LockInner, LockState, Nonce,
    compile_lock_path, held_registry, open_lock_file_raw, path_to_cstring, same_agent_hint,
    snapshot_of,
};
use crate::error::{AcquireRefusal, DevError, EntropyPurpose, LockError, MetadataStage};
use crate::process::HostScope;

/// The control socket a fresh hold starts: its acquisition id and the service.
type ServiceStart =
    fn(&LockContext<'_>) -> Result<Option<(AcqId, crate::lock_service::Service)>, DevError>;

/// What a fresh hold does beyond taking the flock: the host-wide effects.
///
/// Each effect is passed to the transition that owns it. The state types below
/// define the protocol order; this value only supplies host behavior at those
/// boundaries. Tests can replace one transition's effect while retaining the
/// others.
pub(super) struct HoldEffects {
    /// Start the control socket while the acquisition is draining.
    pub(super) service: ServiceStart,
    /// Refuse the hold when the disks the command writes to are nearly full.
    pub(super) disk_gate:
        fn(crate::cli::CommandId, &crate::project::BuildRoot) -> Result<(), DevError>,
    /// Wait out earlier compilation leases; the returned descriptor is the
    /// exclusive lease held while the capability is published.
    pub(super) drain: fn() -> Result<Option<OwnedFd>, DevError>,
    /// Activate the context's tree policy, when one is carried.
    pub(super) tree: fn(&LockContext<'_>) -> Result<Option<crate::tree_policy::HeldTree>, DevError>,
    /// Attach this hold's minted nonce to the live guard for child stamping.
    pub(super) capability: fn(Nonce) -> Option<Nonce>,
    /// Runs once the fresh hold is registered: the stray reap and the stale
    /// guard warning.
    pub(super) after_fresh_hold: fn(),
    /// Enter the per-acquisition accounting cgroup where delegation allows
    /// (`super::cgroup`), before the hold goes live and spawns anything.
    pub(super) cgroup: CgroupEntry,
}

/// The accounting-cgroup effect: the hold's ownership and a read-only view.
pub(super) type CgroupEntry =
    fn(&AcqId) -> Option<(super::cgroup::HoldCgroup, super::cgroup::CgroupView)>;

/// The effects a real acquisition has.
pub(super) const HOST_EFFECTS: HoldEffects = HoldEffects {
    service: host_service,
    disk_gate: crate::disk_gate::check,
    drain: host_drain,
    tree: activate_tree,
    capability: Some,
    after_fresh_hold: host_after_fresh_hold,
    cgroup: super::cgroup::enter,
};

fn activate_tree(ctx: &LockContext<'_>) -> Result<Option<crate::tree_policy::HeldTree>, DevError> {
    crate::tree_policy::activate_for_lock(ctx.tree)
}

fn no_tree(_: &LockContext<'_>) -> Result<Option<crate::tree_policy::HeldTree>, DevError> {
    Ok(None)
}

pub(super) const HOST_EFFECTS_WITHOUT_TREE: HoldEffects = HoldEffects {
    service: host_service,
    disk_gate: crate::disk_gate::check,
    drain: host_drain,
    tree: no_tree,
    capability: Some,
    after_fresh_hold: host_after_fresh_hold,
    cgroup: super::cgroup::enter,
};

fn host_service(
    ctx: &LockContext<'_>,
) -> Result<Option<(AcqId, crate::lock_service::Service)>, DevError> {
    crate::lock_service::sweep_leftovers();
    let id = crate::lock_service::mint_id().ok_or_else(|| {
        DevError::Lock(LockError::acquisition(AcquireRefusal::Entropy {
            purpose: EntropyPurpose::AcquisitionId,
        }))
    })?;
    let identity = identity_of(ctx);
    let service = crate::lock_service::Service::start(&id, identity)?;
    Ok(Some((id, service)))
}

/// Refuse to start or continue an acquisition once this process has committed
/// to stopping. Checked before re-entry, after the flock wait and after the
/// drain, so no path into a hold can bypass an accepted `brokkr kill`.
fn refuse_if_stopping() -> Result<(), DevError> {
    if !crate::shutdown::admits_new_work() {
        return Err(DevError::Interrupted);
    }
    Ok(())
}

fn host_drain() -> Result<Option<OwnedFd>, DevError> {
    crate::run_log::activate_run_log();
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

/// Path-explicit body of [`super::acquire`], parameterised by its [`HoldEffects`] -
/// the unit-test seam: tests pass inert effects and a scratch path, so they
/// exercise the flock, re-entry and metadata without draining, reaping, or
/// touching process-global state.
pub(super) fn acquire_at(
    path: &Path,
    ctx: &LockContext<'_>,
    effects: &HoldEffects,
) -> Result<LockGuard, DevError> {
    acquire_at_with(path, ctx, effects, FlockWait::Block)
}

/// Whether a fresh acquisition waits for a foreign holder or refuses at once.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FlockWait {
    Block,
    NonBlocking,
}

/// [`acquire_at`] with the flock wait chosen by the caller. A non-blocking
/// attempt runs the same successful-hold protocol; only a busy flock differs,
/// returning the typed busy refusal instead of waiting.
pub(super) fn acquire_at_with(
    path: &Path,
    ctx: &LockContext<'_>,
    effects: &HoldEffects,
    wait: FlockWait,
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
        return Err(DevError::Lock(LockError::acquisition(
            AcquireRefusal::InsideCompilation {
                marker: crate::hold::LEASE_MARKER_ENV,
            },
        )));
    }

    let c_path = path_to_cstring(path)?;
    let fd = open_lock_file_raw(&c_path).map_err(|source| {
        DevError::Lock(LockError::acquisition(AcquireRefusal::LockFileOpen {
            path: path.to_owned(),
            source,
        }))
    })?;
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let flocked = Flocked {
        fd: owned,
        path: path.to_owned(),
        service: None,
    };
    match wait {
        FlockWait::Block => {
            let inherited = crate::hold::inherited_capability_hash();
            wait_for_flock(flocked.fd.as_raw_fd(), inherited.as_ref())?;
        }
        FlockWait::NonBlocking => try_flock(flocked.fd.as_raw_fd())?,
    }
    // A stop accepted while this thread waited on the flock.
    refuse_if_stopping()?;
    // Each transition consumes its predecessor. Their order therefore stays
    // visible here and cannot be rearranged without changing the types.
    let flocked = flocked.start_service(ctx, effects.service)?;
    let gated = flocked.gate(ctx, effects.disk_gate)?;
    let drained = gated.drain(ctx, effects.drain)?;
    let authorized = drained.authorize(effects.tree, ctx)?;
    Ok(authorized.into_live(effects.capability, effects.after_fresh_hold, effects.cgroup))
}

/// A fresh exclusive flock, before the free-space gate has admitted work.
struct Flocked {
    // Fields are dropped in declaration order. The service must stop first,
    // while the exclusive flock still prevents another acquisition from
    // replacing it.
    service: Option<(AcqId, crate::lock_service::Service)>,
    fd: OwnedFd,
    path: PathBuf,
}

impl Flocked {
    /// The service is brought up while the acquisition is in progress so
    /// another brokkr can stop a hold that is waiting for compilers to drain.
    fn start_service(
        mut self,
        ctx: &LockContext<'_>,
        start: ServiceStart,
    ) -> Result<Self, DevError> {
        self.service = start(ctx)?;
        Ok(self)
    }

    /// Check the disk gate after taking the flock and before waiting on any
    /// compiler lease.
    fn gate(
        self,
        ctx: &LockContext<'_>,
        gate: fn(crate::cli::CommandId, &crate::project::BuildRoot) -> Result<(), DevError>,
    ) -> Result<Gated, DevError> {
        gate(ctx.command, ctx.build_root).map_err(|error| match error {
            DevError::Preflight(failures) => {
                DevError::Lock(LockError::acquisition(AcquireRefusal::DiskGate(failures)))
            }
            other => other,
        })?;
        Ok(Gated(self))
    }
}

/// A gated flock. No compiler drain has started yet.
struct Gated(Flocked);

impl Gated {
    /// Publish a draining record, then obtain the exclusive compile lease.
    fn drain(
        self,
        ctx: &LockContext<'_>,
        drain: fn() -> Result<Option<OwnedFd>, DevError>,
    ) -> Result<Drained, DevError> {
        let acq_id = self.0.service.as_ref().map(|(id, _)| id);
        let state = build_state(ctx, acq_id);
        rewrite_from_state(self.0.fd.as_raw_fd(), &state).map_err(|source| {
            DevError::Lock(LockError::acquisition(AcquireRefusal::MetadataPublish {
                stage: MetadataStage::Draining,
                source,
            }))
        })?;
        let compile_lease = drain()?;
        Ok(Drained {
            compile_lease,
            // `compile_lease` is declared first so an acquisition refused
            // during authorization releases it before its flocked state.
            flocked: self.0,
            state,
        })
    }
}

/// A gated hold whose earlier compilation leases have all drained.
struct Drained {
    /// Held until the new capability is published under the exclusive lease.
    compile_lease: Option<OwnedFd>,
    flocked: Flocked,
    state: LockState,
}

impl Drained {
    /// Run fallible tree activation before publishing the capability.
    /// Once this transition succeeds, `Authorized::into_live` performs no
    /// fallible work before the new `LockInner` owns the capability.
    fn authorize(
        mut self,
        tree_effect: fn(&LockContext<'_>) -> Result<Option<crate::tree_policy::HeldTree>, DevError>,
        ctx: &LockContext<'_>,
    ) -> Result<Authorized, DevError> {
        refuse_if_stopping()?;
        let tree = tree_effect(ctx)?;
        refuse_if_stopping()?;

        let nonce = Nonce::mint().ok_or_else(|| {
            DevError::Lock(LockError::acquisition(AcquireRefusal::Entropy {
                purpose: EntropyPurpose::Capability,
            }))
        })?;
        self.state.auth = Some(nonce.auth_hash());
        self.state.draining = false;
        rewrite_from_state(self.flocked.fd.as_raw_fd(), &self.state).map_err(|source| {
            DevError::Lock(LockError::acquisition(AcquireRefusal::MetadataPublish {
                stage: MetadataStage::Capability,
                source,
            }))
        })?;

        drop(self.compile_lease.take());
        Ok(Authorized {
            flocked: self.flocked,
            state: self.state,
            nonce,
            tree,
        })
    }
}

/// The only state allowed to publish the capability to child processes.
struct Authorized {
    flocked: Flocked,
    state: LockState,
    nonce: Nonce,
    tree: Option<crate::tree_policy::HeldTree>,
}

impl Authorized {
    /// Move into the live RAII owner. This transition is intentionally
    /// infallible: the published capability immediately becomes owned by the
    /// `LockInner` that controls its lifetime.
    fn into_live(
        self,
        attach_capability: fn(Nonce) -> Option<Nonce>,
        after_fresh_hold: fn(),
        enter_cgroup: CgroupEntry,
    ) -> LockGuard {
        let acq = self.flocked.service.as_ref().map(|(id, _)| id.clone());
        let service = self.flocked.service.map(|(_, service)| service);
        if let Some(service) = &service {
            service.update(snapshot_of(&self.state));
        }
        let observed = service.as_ref().map(crate::lock_service::Service::observed);
        // Before anything is spawned under the hold, so every child is born
        // inside the group. Optional: `None` wherever delegation is absent.
        let cgroup = acq.as_ref().and_then(enter_cgroup).map(|(hold, view)| {
            if let Some(observed) = &observed {
                observed.set_cgroup(view);
            }
            hold
        });
        let mut active_tree_dirs = HashMap::new();
        if let Some(held) = &self.tree {
            active_tree_dirs.insert(held.directory().to_owned(), 1);
        }
        let inner = Arc::new(LockInner {
            fd: self.flocked.fd,
            path: self.flocked.path,
            state: Mutex::new(self.state),
            tree: self.tree,
            capability: attach_capability(self.nonce),
            active_tree_dirs: Mutex::new(active_tree_dirs),
            service,
            observed,
            cgroup,
        });
        held_registry().push(Arc::downgrade(&inner));
        let guard = LockGuard { inner };
        after_fresh_hold();
        guard
    }
}

/// How often a waiting acquisition retries the flock and re-reads the holder.
pub(super) const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(100);

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
/// PID namespace hides: the shared child policy carries the hold's nonce, and
/// the lock record publishes its hash (`crate::hold`). `inherited` is this
/// process's inherited hash; a holder publishing the same one is the hold this
/// process descends from.
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
pub(super) fn wait_for_flock(fd: RawFd, inherited: Option<&AuthHash>) -> Result<(), DevError> {
    let mut announced = false;
    loop {
        // This is acquisition, not observation: it must establish exclusive
        // ownership. The shared probe used by status and the rustc guard would
        // allow two brokkr commands to proceed together.
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            if announced {
                // Wording fixed on purpose - do not shorten or "improve" it.
                // These two lines have been revised many times, and every
                // terser version was misread by agents: without "you now hold
                // the global brokkr lock for the duration of this command",
                // an agent concludes its runs are contested and that other
                // projects' brokkr commands are loading the machine during
                // its benchmarks. They are not - the lock excludes them.
                crate::output::lock_msg(
                    "acquired - you now hold the global brokkr lock for the duration of this \
                     command",
                );
            }
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => {}
            Some(libc::EINTR) => continue,
            _ => {
                return Err(DevError::Lock(LockError::acquisition(
                    AcquireRefusal::LockFlock { source: err },
                )));
            }
        }
        let holder = read_lock_contents(fd);
        if let (Some(mine), Some(info)) = (inherited, &holder)
            && info.auth.as_ref() == Some(mine)
        {
            return Err(DevError::Lock(LockError::acquisition(nested_refusal(info))));
        }
        if !announced {
            announced = true;
            // Deliberately terse, and fixed wording (see the acquired line
            // above): who holds the lock and how busy it is lives in
            // `brokkr lock`, which re-samples on every invocation.
            crate::output::lock_msg("waiting for the brokkr lock ...");
            if let Some(hint) = holder.as_ref().and_then(same_agent_hint) {
                crate::output::lock_msg(&hint);
            }
        }
        if crate::shutdown::should_unwind() {
            return Err(DevError::Interrupted);
        }
        std::thread::sleep(LOCK_POLL);
    }
}

/// One exclusive flock attempt. A held lock is the typed busy refusal, naming
/// the holder's command when its record can be read.
fn try_flock(fd: RawFd) -> Result<(), DevError> {
    loop {
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if ret == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EWOULDBLOCK) => {
                let holder = read_lock_contents(fd).map(|info| info.command);
                return Err(DevError::Lock(LockError::acquisition(
                    AcquireRefusal::Busy { holder },
                )));
            }
            _ => {
                return Err(DevError::Lock(LockError::acquisition(
                    AcquireRefusal::LockFlock { source: err },
                )));
            }
        }
    }
}

/// The refusal for a descendant of the live holder.
fn nested_refusal(holder: &LockInfo) -> AcquireRefusal {
    AcquireRefusal::NestedDescendant {
        command: holder.command.clone(),
    }
}

/// How long a fresh acquisition will wait for earlier compilation leases to
/// drain before giving up. Split into two windows: after the first, the stray
/// reaper runs once (a foreign compiler holding a lease is exactly what it
/// exists to kill), then the wait resumes.
///
/// The first window is short on purpose. Compilers admitted while no hold
/// existed (rust-analyzer's checks, a hand-typed `cargo build`) carry their
/// lease through exec too (`src/bin/rustc_guard.rs`, `Decision::Idle`), so any
/// IDE build in flight when a hold starts now blocks this drain. Every such
/// compiler is a stray by construction - no brokkr is its ancestor, and the
/// previous holder waited for its own cargo before releasing - and the
/// post-hold reap (`stray::reap_after_lock`) would SIGKILL it moments after
/// the drain anyway. Waiting longer only stalls the acquisition; the few
/// seconds let a nearly-finished check end on its own rather than be killed.
const DRAIN_BUDGET: Duration = Duration::from_secs(120);
const DRAIN_REAP_AFTER: Duration = Duration::from_secs(3);
const DRAIN_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
struct DrainPolicy {
    budget: Duration,
    reap_after: Duration,
    poll: Duration,
}

const DEFAULT_DRAIN_POLICY: DrainPolicy = DrainPolicy {
    budget: DRAIN_BUDGET,
    reap_after: DRAIN_REAP_AFTER,
    poll: DRAIN_POLL,
};

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
    drain_compile_leases_at(&path, DEFAULT_DRAIN_POLICY, crate::stray::reap_for_drain)
}

/// Parameterized drain used by the production wrapper and any focused drain
/// checks. Callers can use a scratch lock, short deadlines and an inert reaper
/// without waiting on or signalling host compilers.
fn drain_compile_leases_at(
    path: &Path,
    policy: DrainPolicy,
    reap: fn(),
) -> Result<OwnedFd, DevError> {
    let c_path = path_to_cstring(path)?;
    let fd = open_lock_file_raw(&c_path).map_err(|source| {
        DevError::Lock(LockError::acquisition(AcquireRefusal::CompileLeaseOpen {
            path: path.to_owned(),
            source,
        }))
    })?;
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    let start = Instant::now();
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
                return Err(DevError::Lock(LockError::acquisition(
                    AcquireRefusal::CompileLeaseFlock {
                        path: path.to_owned(),
                        source: err,
                    },
                )));
            }
        }
        // Silent wait: an IDE check in flight at acquisition is the ordinary
        // case, and the reap below reports anything it kills on its own.
        let waited = start.elapsed();
        if waited >= policy.budget {
            return Err(DevError::Lock(LockError::acquisition(
                AcquireRefusal::DrainTimeout {
                    waited: policy.budget,
                },
            )));
        }
        if !reaped && waited >= policy.reap_after {
            reaped = true;
            reap();
        }
        std::thread::sleep(policy.poll);
    }
}

/// Build the initial `LockState` for a freshly-acquired lock. Captures the
/// current brokkr invocation args (argv minus `argv[0]`) so `brokkr lock`
/// can show exactly what the user typed, plus the identity tokens readers
/// verify before trusting the PID.
fn build_state(ctx: &LockContext<'_>, acq_id: Option<&AcqId>) -> LockState {
    let identity = identity_of(ctx);
    let scope = HostScope::current();
    let process_identity = crate::process::ProcStat::read_self()
        .ok()
        .map(|stat| stat.identity());
    LockState {
        project: identity.project,
        command: identity.command,
        args: identity.args,
        project_root: identity.root,
        process_identity,
        scope,
        acq_id: acq_id.cloned(),
        codex_thread: identity.codex_thread,
        codex_session: identity.codex_session,
        auth: None,
        draining: true,
        children: Vec::new(),
        mocks: Vec::new(),
        progress: None,
    }
}

/// What this process states about itself as a lock holder, for the lock file
/// and the control socket alike.
fn identity_of(ctx: &LockContext<'_>) -> crate::lock_service::Identity {
    let ns = |path: &str| {
        crate::process::namespace_inode(path)
            .map(|inode| inode.get().to_string())
            .unwrap_or_default()
    };
    crate::lock_service::Identity {
        project: ctx.project.to_owned(),
        command: ctx.command.spec().name.to_owned(),
        args: current_invocation_args(),
        root: ctx.project_root.as_path().display().to_string(),
        pid_ns: ns("/proc/self/ns/pid"),
        // The thread reading `starttime` is this one, and its time namespace
        // is what offsets the value it reads.
        time_ns: ns("/proc/thread-self/ns/time"),
        codex_thread: std::env::var("CODEX_THREAD_ID").unwrap_or_default(),
        codex_session: std::env::var("CODEX_SESSION_ID").unwrap_or_default(),
    }
}

/// This invocation's arguments (argv minus `argv[0]`), shell-quoted for display.
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
