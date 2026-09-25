//! Cooperative shutdown - `brokkr kill` asks brokkr to wrap up a running
//! bench cleanly rather than letting the user SIGKILL it and leave scratch
//! data behind.
//!
//! The protocol:
//!
//! 1. `brokkr kill` reads the lockfile and sends `SIGTERM` to the brokkr PID.
//! 2. A [`SigtermGuard`] is installed for the lifetime of each sidecar run.
//!    Its handler sets [`SHUTDOWN_REQUESTED`] and nothing else - it must be
//!    async-signal-safe. Outside the sidecar window, `SIGTERM` falls through
//!    to the default terminate action: killing brokkr mid-`cargo build` or
//!    mid-`brokkr check` is what the user wants anyway (no scratch to
//!    clean). The one thing the default action would leave behind is a test
//!    runner's detached process group; while one is live a [`GroupReaper`]
//!    handler SIGKILLs it and re-raises, so brokkr still dies by the signal.
//! 3. The sidecar loop polls [`is_shutdown_requested`] on every sample tick
//!    (alongside `try_wait` and the `--stop` marker check). When set, it
//!    `SIGKILL`s the child and breaks out of the loop with
//!    `stopped_by_signal = true`.
//! 4. `run_external` propagates that up as [`crate::error::DevError::Interrupted`]
//!    after saving the partial sidecar data under the `dirty` alias. `main`
//!    catches that error, runs the scratch-cleanup path, and exits 130.
//!
//! `brokkr kill --hard` bypasses this entirely: it SIGKILLs the recorded
//! child PID first (so it is not orphaned), then the brokkr PID. Scratch
//! is left in whatever state the tool left it (follow up with
//! `brokkr clean`). A test runner's direct child carries a parent-death
//! signal ([`die_with_parent`]), so it goes down with a SIGKILLed brokkr too.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether a [`SigtermGuard`] currently owns the SIGTERM/SIGINT dispositions,
/// so a [`GroupReaper`] knows not to replace its handler.
static GUARD_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Put brokkr in its own process group at startup when it is safe to do so,
/// so brokkr's internal `kill(-pgid, …)` sweeps can never escape *upward*
/// into the process group of whatever launched it.
///
/// brokkr is full of group-kills: the deadline / cooperative-SIGTERM paths in
/// `output.rs`, the test-runner watchdog, `ratatoskr/process.rs`, and the
/// `--hard` branch in `main_parts/commands.rs`. Every one assumes "my process
/// group holds only me and my descendants". An interactive shell guarantees
/// that (each job gets its own group); so does a launcher that calls `setsid`
/// (Claude Code runs each command in a fresh session). But a launcher that
/// does *neither* (e.g. a `subprocess.run(...)` without `start_new_session`)
/// leaves brokkr sharing the launcher's group, so one `kill(-pgid)` takes the
/// launcher and its siblings down too. We refuse to depend on the launcher
/// being polite and establish the invariant ourselves.
///
/// Two cases where we must NOT move, both detected before acting:
///   * Already our own group leader (`getpgrp() == getpid()`) - the normal
///     interactive-foreground job. `kill(-pgid)` already only reaches our
///     subtree, and `setpgid(0, 0)` would be a no-op regardless.
///   * Our group is the controlling terminal's foreground group (the
///     `cmd | brokkr …` pipeline case). Detaching would cut us off from
///     terminal-delivered SIGINT - the exact mechanism [`SigtermGuard`]
///     relies on to forward Ctrl-C to tracked children - and risk SIGTTOU
///     on output.
///
/// Everything else - notably the supervised/captured case where stdio is
/// pipes and no terminal has us in the foreground - is where detaching both
/// helps and is safe. Best-effort: any failure leaves us exactly where we
/// were, no worse than before this call existed.
pub fn isolate_process_group() {
    // SAFETY: getpid/getpgrp/setpgid take no pointers and cannot corrupt
    // memory; we only read their integer results.
    let pid = unsafe { libc::getpid() };
    let pgrp = unsafe { libc::getpgrp() };
    if pgrp == pid {
        return; // already a group leader
    }
    if terminal_foreground_pgrp() == Some(pgrp) {
        return; // moving would break terminal Ctrl-C delivery
    }
    // Become a new group leader (PGID == PID). EPERM/ESRCH just mean we stay
    // put, which is the pre-existing behaviour.
    unsafe {
        libc::setpgid(0, 0);
    }
}

/// The foreground process group of brokkr's controlling terminal, or `None`
/// when there is no controlling terminal (the supervised / fully-redirected
/// case). Queries `/dev/tty` - the controlling terminal regardless of where
/// stdio points - opened `O_NOCTTY` so the probe never *acquires* one.
fn terminal_foreground_pgrp() -> Option<libc::pid_t> {
    let path = b"/dev/tty\0";
    // SAFETY: `path` is a NUL-terminated literal; open/tcgetpgrp/close take
    // no caller-owned pointers beyond it and we check every return value.
    let fd = unsafe {
        libc::open(
            path.as_ptr().cast::<libc::c_char>(),
            libc::O_RDONLY | libc::O_NOCTTY,
        )
    };
    if fd < 0 {
        return None; // no controlling terminal
    }
    let fg = unsafe { libc::tcgetpgrp(fd) };
    unsafe {
        libc::close(fd);
    }
    if fg < 0 { None } else { Some(fg) }
}

/// Whether a shutdown has been requested via SIGTERM since the current
/// `SigtermGuard` was installed.
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

extern "C" fn shutdown_handler(_: libc::c_int) {
    // Registered test process groups go down at once, not on the next poll:
    // a runner blocked in `wait()` never polls, and a test process has no
    // cooperative-shutdown contract to honour. See [`GroupReaper`].
    kill_registered_groups();
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

fn set_handler(signum: libc::c_int, handler: libc::sighandler_t) {
    // SAFETY: `signal` with a plain function pointer is safe; the
    // handler body only touches an AtomicBool which is itself
    // async-signal-safe to write.
    unsafe {
        libc::signal(signum, handler);
    }
}

/// RAII guard that installs SIGTERM + SIGINT handlers for the duration
/// of a tracked-child window and restores the default action on drop.
///
/// SIGTERM covers `brokkr kill`. SIGINT covers terminal Ctrl-C: now that
/// captured children spawn with `process_group(0)`, the terminal sends
/// SIGINT only to brokkr's foreground PG (which excludes the child), so
/// without this handler ctrl-C would orphan the child. The handler sets
/// `SHUTDOWN_REQUESTED`; the captured runner's poll loop sees the flag
/// and forwards SIGTERM to the child PG before returning `Interrupted`.
///
/// Scoped tightly so `brokkr kill` / ctrl-C during non-tracked work
/// (cargo build outside an orchestrator, `brokkr check`, ...) terminates
/// brokkr immediately instead of being silently swallowed into a flag
/// nobody polls.
pub struct SigtermGuard;

impl SigtermGuard {
    pub fn install() -> Self {
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
        GUARD_ACTIVE.store(true, Ordering::SeqCst);
        let h: libc::sighandler_t = shutdown_handler as *const () as usize;
        set_handler(libc::SIGTERM, h);
        set_handler(libc::SIGINT, h);
        Self
    }
}

impl Drop for SigtermGuard {
    fn drop(&mut self) {
        GUARD_ACTIVE.store(false, Ordering::SeqCst);
        set_handler(libc::SIGTERM, libc::SIG_DFL);
        set_handler(libc::SIGINT, libc::SIG_DFL);
        // A test process group registered inside this guard's window must stay
        // covered once the guard's cooperative handler is gone.
        reinstall_reaper_if_needed();
        // Reset the flag so a captured subprocess invoked AFTER this
        // guard's scope (e.g. in main's cleanup path) doesn't see a
        // sticky `true` left over from a SIGTERM/SIGINT that already
        // fired and was handled. Without this, the captured runner's
        // flag-poll loop would spuriously SIGTERM unrelated children.
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Test process groups die with brokkr.
//
// The test runners spawn their children with `process_group(0)` so the
// watchdog can kill a whole test tree with one `kill(-pgid)`. The price is that
// terminal Ctrl-C (delivered to brokkr's foreground group only) and `brokkr
// kill` (SIGTERM to brokkr's pid only) never reach those groups. With no
// `SigtermGuard` around `check`/`test` - deliberately, so Ctrl-C still stops
// brokkr at once during a build - brokkr died on the default action and the
// cargo and test processes kept running, unreapable by the stray reaper, which
// knows only cargo-family names and so never matches a directly executed test
// binary.
//
// A `SigtermGuard` for the whole command is the wrong fix: every blocking wait
// in the pipeline would have to poll its flag, or Ctrl-C would be swallowed.
// Instead the runners REGISTER each test group. While any group is registered
// (and no `SigtermGuard` owns the dispositions), SIGINT/SIGTERM run a handler
// that SIGKILLs every registered group and then re-raises the signal under the
// default action - so brokkr still dies exactly as it did, it just takes its
// test groups with it. Under a `SigtermGuard`, the guard's own handler kills
// the registered groups before setting its flag. Everything the handler does
// (`kill`, `signal`, `raise`, atomic loads) is async-signal-safe.
//
// What this cannot cover is brokkr being SIGKILLed (`brokkr kill --hard`, the
// OOM killer): no handler runs. [`die_with_parent`] is the backstop for the
// direct child there.
// ---------------------------------------------------------------------------

/// Slots for concurrently registered groups. A parallel sweep registers one per
/// in-flight binary, bounded by its budget; a registration beyond the table is
/// simply not covered (the pre-existing behaviour), never an error.
const REAP_SLOTS: usize = 256;

/// Registered process-group ids; `0` marks a free slot. Read by the signal
/// handler, so atomics only.
static REAP_PGIDS: [AtomicI32; REAP_SLOTS] = [const { AtomicI32::new(0) }; REAP_SLOTS];

/// The signals the reaper handles: the same pair [`SigtermGuard`] covers.
const REAPED_SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

/// Registration bookkeeping, touched only outside signal context.
struct ReapState {
    /// Number of occupied slots.
    live: usize,
    /// The dispositions the reaper replaced, restored when the last group
    /// unregisters. `None` while the reaper handler is not installed.
    saved: Option<[libc::sighandler_t; 2]>,
}

static REAP_STATE: Mutex<ReapState> = Mutex::new(ReapState { live: 0, saved: None });

fn reap_state() -> std::sync::MutexGuard<'static, ReapState> {
    REAP_STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// SIGKILL every registered group. Async-signal-safe.
fn kill_registered_groups() {
    for slot in &REAP_PGIDS {
        let pgid = slot.load(Ordering::SeqCst);
        if pgid > 1 {
            // SAFETY: kill(2) is async-signal-safe and takes no pointers.
            // `pgid > 1` keeps this from ever becoming kill(0) (our own group)
            // or kill(-1) (every process we may signal).
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

extern "C" fn reap_and_die(sig: libc::c_int) {
    kill_registered_groups();
    // Back to the default action and re-deliver: brokkr terminates by the
    // signal it was sent, exactly as it did before the reaper existed. The
    // signal is blocked while this handler runs, so it lands on return.
    // SAFETY: signal(2) and raise(3) are async-signal-safe.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Install the reaper handler, remembering what it replaced. A signal brokkr
/// was started with ignored (a background job's SIGINT) stays ignored.
fn install_reaper(state: &mut ReapState) {
    let h: libc::sighandler_t = reap_and_die as *const () as usize;
    let mut saved = [libc::SIG_DFL; 2];
    for (i, sig) in REAPED_SIGNALS.iter().enumerate() {
        // SAFETY: as in `set_handler`; the handler touches only atomics and
        // async-signal-safe calls.
        let prev = unsafe { libc::signal(*sig, h) };
        let original = state.saved.map_or(prev, |s| s[i]);
        if original == libc::SIG_IGN {
            set_handler(*sig, libc::SIG_IGN);
        }
        saved[i] = original;
    }
    state.saved = Some(saved);
}

fn uninstall_reaper(state: &mut ReapState) {
    if let Some(saved) = state.saved.take() {
        for (i, sig) in REAPED_SIGNALS.iter().enumerate() {
            set_handler(*sig, saved[i]);
        }
    }
}

/// Called when a [`SigtermGuard`] drops: its default dispositions would
/// otherwise uncover groups still registered.
fn reinstall_reaper_if_needed() {
    let mut state = reap_state();
    if state.live > 0 {
        install_reaper(&mut state);
    } else {
        // The guard just set the defaults; nothing of the reaper's to restore.
        state.saved = None;
    }
}

/// A registered test process group, killed if brokkr is interrupted while it
/// lives. Hold it for exactly as long as the group's leader is un-reaped: drop
/// it right after `wait`, so the id cannot be recycled into an unrelated group
/// while still registered.
pub struct GroupReaper {
    slot: Option<usize>,
}

impl GroupReaper {
    /// Register `pgid` (the leader's pid of a child spawned with
    /// `process_group(0)`). Best effort: a full table or an out-of-range id
    /// registers nothing.
    pub fn register(pgid: u32) -> Self {
        let Ok(pgid) = i32::try_from(pgid) else {
            return Self { slot: None };
        };
        if pgid <= 1 {
            return Self { slot: None };
        }
        let mut state = reap_state();
        let slot = REAP_PGIDS.iter().position(|s| {
            s.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        });
        if slot.is_some() {
            state.live += 1;
            if state.saved.is_none() && !GUARD_ACTIVE.load(Ordering::SeqCst) {
                install_reaper(&mut state);
            }
        }
        Self { slot }
    }
}

impl Drop for GroupReaper {
    fn drop(&mut self) {
        let Some(i) = self.slot else { return };
        let mut state = reap_state();
        REAP_PGIDS[i].store(0, Ordering::SeqCst);
        state.live = state.live.saturating_sub(1);
        if state.live == 0 {
            if GUARD_ACTIVE.load(Ordering::SeqCst) {
                // The guard owns the dispositions now and restores the
                // defaults itself when it drops.
                state.saved = None;
            } else {
                uninstall_reaper(&mut state);
            }
        }
    }
}

/// Have the kernel SIGKILL the child if the thread that spawned it dies -
/// the backstop for a brokkr that is itself SIGKILLed, where no signal handler
/// runs. It covers the direct child only (the test binary on the parallel
/// lane, cargo on the others); grandchildren are the [`GroupReaper`]'s job.
///
/// The parent is the spawning THREAD, so the caller must wait on the child
/// from the thread that spawned it - true of every test runner, which spawns
/// and waits in one function.
pub fn die_with_parent(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: getpid takes no pointers.
    let parent = unsafe { libc::getpid() };
    let sig = libc::c_ulong::try_from(libc::SIGKILL).unwrap_or(9);
    // SAFETY: between fork and exec only async-signal-safe calls are allowed;
    // prctl and getppid are both raw syscalls.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, sig, 0, 0, 0) == 0
                && libc::getppid() != parent
            {
                // The parent died before the death signal was armed, so it
                // will never fire: refuse to run an orphan.
                return Err(std::io::Error::other("parent exited before exec"));
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A registration occupies a slot for exactly its guard's lifetime, and
    /// ids that would turn the handler's `kill(-pgid)` into a broadcast are
    /// refused outright.
    #[test]
    fn a_registration_lives_exactly_as_long_as_its_guard() {
        // An id no live test will ever register, so a concurrent test's own
        // registrations cannot confuse the count.
        let pgid: i32 = 0x3fff_fff1;
        let count = || REAP_PGIDS.iter().filter(|s| s.load(Ordering::SeqCst) == pgid).count();
        {
            let _r = GroupReaper::register(u32::try_from(pgid).unwrap());
            assert_eq!(count(), 1);
        }
        assert_eq!(count(), 0);
        assert!(GroupReaper::register(0).slot.is_none());
        assert!(GroupReaper::register(1).slot.is_none());
        assert!(GroupReaper::register(u32::MAX).slot.is_none());
    }
}
