use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::record::{read_lock_contents, rewrite_from_state};
use super::{
    AcqId, AuthHash, LockContext, LockGuard, LockInfo, LockInner, LockState, Nonce,
    compile_lock_path, held_registry, open_lock_file, path_to_cstring, same_agent_hint,
    snapshot_of,
};
use crate::error::DevError;
use crate::process::HostScope;

/// What a fresh hold does beyond taking the flock: the host-wide effects.
///
/// A seam, so unit tests can take real flocks on scratch paths without any of
/// them. Before it existed the test entry point ran the production drain on
/// the real `~/.brokkr/compile.lock` - which, 20 s into a wait behind a live
/// build, SIGKILLs whatever the stray reaper finds on the host, rust-analyzer's
/// cargo included - and mutated toolchain-disable state that other tests read.
pub(super) struct HoldEffects {
    /// Wait out earlier compilation leases; the returned descriptor is the
    /// exclusive lease held while the capability is published. `None` means
    /// no lease was taken (tests).
    pub(super) drain: fn() -> Result<Option<OwnedFd>, DevError>,
    /// Activate the armed toolchain-disable ([`crate::toolchain`]).
    pub(super) toolchain: fn() -> Result<Option<crate::toolchain::DisabledToolchain>, DevError>,
    /// Attach the minted nonce to this hold for child stamping.
    pub(super) attach_capability: bool,
    /// Runs once the fresh hold is registered: the stray reap and the stale
    /// guard warning.
    pub(super) after_fresh_hold: fn(),
    /// Refuse the hold when the disks the command writes to are nearly full
    /// ([`crate::disk_gate`]).
    pub(super) disk_gate: fn(&str) -> Result<(), DevError>,
    /// Run this acquisition's control socket (`crate::lock_service`).
    pub(super) service: bool,
}

/// The effects a real acquisition has.
pub(super) const HOST_EFFECTS: HoldEffects = HoldEffects {
    drain: host_drain,
    toolchain: crate::toolchain::activate_for_lock,
    attach_capability: true,
    after_fresh_hold: host_after_fresh_hold,
    disk_gate: crate::disk_gate::check,
    service: true,
};

fn no_toolchain() -> Result<Option<crate::toolchain::DisabledToolchain>, DevError> {
    Ok(None)
}

pub(super) const HOST_EFFECTS_WITHOUT_TOOLCHAIN: HoldEffects = HoldEffects {
    drain: host_drain,
    toolchain: no_toolchain,
    attach_capability: true,
    after_fresh_hold: host_after_fresh_hold,
    disk_gate: crate::disk_gate::check,
    service: true,
};

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

/// Path-explicit body of [`super::acquire`], parameterised by its [`HoldEffects`] -
/// the unit-test seam: tests pass inert effects and a scratch path, so they
/// exercise the flock, re-entry and metadata without draining, reaping, or
/// touching process-global state.
pub(super) fn acquire_at(
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
    let inherited = crate::hold::inherited_capability_hash();
    wait_for_flock(owned.as_raw_fd(), inherited.as_ref())?;
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
        Some((
            id.clone(),
            crate::lock_service::Service::start(&id, identity)?,
        ))
    } else {
        None
    };
    let acq_id = service.as_ref().map(|(id, _)| id.clone());
    let service = service.map(|(_, s)| s);
    // The free-space gate. After the flock, not before: the wait may have been
    // behind a `brokkr clean` that freed the space, or a build that used it.
    // Before the drain, so a refusal costs nothing but the release, which
    // dropping `owned` does.
    (effects.disk_gate)(ctx.command)?;
    // We hold `brokkr.lock`, but the hold is not yet usable: compilers admitted
    // under an earlier hold may still be running. On failure `owned` drops here,
    // releasing `brokkr.lock`, and we never reach protected work - measuring
    // alongside a compiler we failed to clear is the one outcome worse than not
    // measuring at all.
    let Authorized {
        state,
        nonce,
        toolchain,
    } = drain_and_authorize(owned.as_raw_fd(), ctx, effects, acq_id.as_ref())?;
    // A stop accepted during the drain: never authorise work after it.
    // `toolchain` drops here, restoring the moved-aside toolchain.
    refuse_if_stopping()?;
    if let Some(service) = &service {
        service.update(snapshot_of(&state));
    }

    // Construct the owner with its capability before registering it. No
    // process-global nonce is published, so a test hold or a later hold cannot
    // clear or replace the capability owned by this one.
    let mut active_toolchain_dirs = HashMap::new();
    if let Some(disabled) = &toolchain {
        active_toolchain_dirs.insert(disabled.directory().to_owned(), 1);
    }
    let inner = Arc::new(LockInner {
        fd: owned,
        path: path.to_owned(),
        state: Mutex::new(state),
        toolchain,
        capability: effects.attach_capability.then_some(nonce),
        active_toolchain_dirs: Mutex::new(active_toolchain_dirs),
        service,
    });
    // Register weakly so a later acquire in this process re-enters this hold
    // instead of self-deadlocking on a second fd.
    held_registry().push(Arc::downgrade(&inner));
    let guard = LockGuard { inner };
    (effects.after_fresh_hold)();
    Ok(guard)
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
            _ => return Err(DevError::Lock(format!("flock failed: {err}"))),
        }
        let holder = read_lock_contents(fd);
        if let (Some(mine), Some(info)) = (inherited, &holder)
            && info.auth.as_ref() == Some(mine)
        {
            return Err(DevError::Lock(nested_refusal(info)));
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
                "waiting for compilers started before this hold (an earlier hold's, or an IDE's) to finish - brokkr clears them before measuring",
            );
        }
        let waited = start.elapsed();
        if waited >= policy.budget {
            return Err(DevError::Lock(format!(
                "compilation leases did not drain within {}s. A compiler started before this \
                 hold is still holding its lease, or a process that inherited its \
                 descriptor is. This is not a transient busy signal - an unchanged blocker \
                 will fail every retry. `brokkr strays` shows what is running; \
                 `brokkr lock` shows the holder.",
                policy.budget.as_secs()
            )));
        }
        if !reaped && waited >= policy.reap_after {
            reaped = true;
            crate::output::lock_msg(
                "compilers have not drained; reaping strays once, then waiting again",
            );
            reap();
        }
        std::thread::sleep(policy.poll);
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
    acq_id: Option<&AcqId>,
) -> Result<Authorized, DevError> {
    let mut state = build_state(ctx, acq_id);
    rewrite_from_state(fd, &state)
        .map_err(|e| DevError::Lock(format!("failed to publish lock metadata: {e}")))?;

    let compile_lease = (effects.drain)()?;

    let nonce = Nonce::mint().ok_or_else(|| {
        DevError::Lock(
            "could not read /dev/urandom to mint a compilation capability; a hold that cannot \
             mint one would have every one of its own compiles refused"
                .into(),
        )
    })?;
    state.auth = Some(nonce.auth_hash());
    state.draining = false;
    rewrite_from_state(fd, &state).map_err(|e| {
        DevError::Lock(format!("failed to publish the compilation capability: {e}"))
    })?;

    // Activate the armed toolchain-disable here: after the drain, so the
    // moved-aside window never spans a wait on other processes, and before the
    // new `LockInner` takes ownership of the nonce. If activation fails, the
    // published lock-file hash has no matching child capability and the hold
    // is refused before protected work starts.
    let toolchain = (effects.toolchain)()?;

    drop(compile_lease);
    Ok(Authorized {
        state,
        nonce,
        toolchain,
    })
}

/// A drained, authorized hold, ready for its [`LockInner`] to take ownership.
///
/// The nonce is carried out rather than attached here on purpose: it only
/// becomes available to child stamping when the owning `LockInner` is built.
struct Authorized {
    state: LockState,
    nonce: Nonce,
    toolchain: Option<crate::toolchain::DisabledToolchain>,
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
        child: None,
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
        command: ctx.command.to_owned(),
        args: current_invocation_args(),
        root: ctx.project_root.to_owned(),
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
