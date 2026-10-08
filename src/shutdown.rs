//! Cooperative shutdown - `brokkr kill` asks brokkr to wrap up a running
//! bench cleanly rather than letting the user SIGKILL it and leave scratch
//! data behind.
//!
//! The protocol:
//!
//! 1. `brokkr kill` reads the lockfile and asks the holder to stop over its
//!    control socket (`crate::lock_service`): the holder commits a sticky stop
//!    ([`commit_stop`]) and sends itself `SIGTERM`. Only when the socket does
//!    not answer, and the holder's identity verifies from the caller's own
//!    namespaces, does `kill` send that `SIGTERM` to the brokkr PID itself.
//! 2. A [`SigtermGuard`] is installed for the lifetime of each tracked-child
//!    window - a sidecar run, a passthrough child, an orchestrator's whole
//!    run, and the whole of `brokkr check` / `brokkr test` / `brokkr clippy`.
//!    Its handler kills registered test groups, sets [`SHUTDOWN_REQUESTED`],
//!    and nothing else - it must be async-signal-safe. Guards nest: only the
//!    outermost installs and restores the dispositions, so an inner guard can
//!    neither clear a pending request nor uncover the outer window. A
//!    *second* signal while a request is already pending takes the default
//!    action (after killing registered groups), so a wait that never polls
//!    the flag cannot swallow Ctrl-C for good. Outside any guard, `SIGTERM`
//!    falls through to the default terminate action; the one thing that
//!    would leave behind is a test runner's detached process group, and while
//!    one is live a [`GroupReaper`] handler SIGKILLs it and re-raises, so
//!    brokkr still dies by the signal.
//! 3. The sidecar loop polls [`is_shutdown_requested`] on every sample tick
//!    (alongside `try_wait` and the `--stop` marker check). When set, it
//!    `SIGKILL`s the child and breaks out of the loop with
//!    `stopped_by_signal = true`.
//! 4. `run_external` propagates that up as [`crate::error::DevError::Interrupted`]
//!    after saving the partial sidecar data under the `dirty` alias. `main`
//!    catches that error, runs the scratch-cleanup path, and exits 130.
//!
//! `brokkr kill --hard` bypasses this entirely: it SIGSTOPs the brokkr PID,
//! SIGKILLs the recorded child PID and every identity-checked descendant of
//! brokkr ([`kill_descendants`], so a test binary under cargo is not
//! orphaned), then the brokkr PID. Scratch is left in whatever state the tool
//! left it (follow up with `brokkr clean`). Against a brokkr SIGKILLed by
//! anything else (the OOM killer) there are two backstops: a test runner's
//! direct child carries a parent-death signal ([`die_with_parent`]), and
//! everything below it carries the run's token and is reaped at the next
//! fresh hold (`crate::test_orphans`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether a [`SigtermGuard`] currently owns the SIGTERM/SIGINT dispositions,
/// so a [`GroupReaper`] knows not to replace its handler.
static GUARD_ACTIVE: AtomicBool = AtomicBool::new(false);

/// How many [`SigtermGuard`]s are live. Only the transition 0 -> 1 installs
/// the handlers (and clears a stale request); only 1 -> 0 restores the
/// defaults. Touched outside signal context only.
static GUARD_DEPTH: Mutex<usize> = Mutex::new(0);

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

/// Set once a `brokkr kill` arriving over the lock socket has been accepted
/// (`crate::lock_service`), and never cleared. Unlike [`SHUTDOWN_REQUESTED`],
/// which each outermost [`SigtermGuard`] resets, an accepted stop must survive
/// guard transitions: the requester was told the command is stopping, so a
/// guard that drops without having polled the flag must not quietly forget it.
/// [`is_shutdown_requested`] reports it, and lock acquisition and
/// `lockfile::current_hold` refuse while it is set, so no new work is admitted.
static STOP_COMMITTED: AtomicBool = AtomicBool::new(false);

/// Commit this process to stopping. See [`STOP_COMMITTED`].
pub fn commit_stop() {
    STOP_COMMITTED.store(true, Ordering::SeqCst);
}

/// Whether [`commit_stop`] has run in this process and still bars new work.
/// [`admit_exit_cleanup`] lifts the bar for the one thing a stopped command
/// still does: its scratch cleanup on the way out.
pub fn stop_committed() -> bool {
    STOP_COMMITTED.load(Ordering::SeqCst) && !EXIT_CLEANUP.load(Ordering::SeqCst)
}

static EXIT_CLEANUP: AtomicBool = AtomicBool::new(false);

/// Called by `main`'s interrupt path immediately before the scratch cleanup,
/// which takes the lock like any `brokkr clean`. Nothing but that cleanup and
/// the process exit follow it.
pub fn admit_exit_cleanup() {
    EXIT_CLEANUP.store(true, Ordering::SeqCst);
}

/// Whether a shutdown has been requested via SIGTERM/SIGINT (or
/// [`request_shutdown`]) since the outermost `SigtermGuard` was installed, or
/// a stop was committed over the lock socket at any point.
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst) || stop_committed()
}

/// Raise the shutdown flag from inside brokkr, exactly as a `brokkr kill`
/// would, minus the signal. The `check` watchdog uses it after killing the
/// run's processes, so every polling runner returns `Interrupted`, no new
/// test group is spawned, and the command unwinds through its normal exit
/// path (history, toolchain restore, lock release) instead of `exit`ing past
/// them.
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

extern "C" fn shutdown_handler(sig: libc::c_int) {
    // Registered test process groups go down at once, not on the next poll:
    // a runner blocked in `wait()` never polls, and a test process has no
    // cooperative-shutdown contract to honour. See [`GroupReaper`].
    kill_registered_groups();
    if SHUTDOWN_REQUESTED.swap(true, Ordering::SeqCst) {
        // A request is already pending and brokkr is still here: whatever is
        // running is not polling the flag. The second Ctrl-C / `brokkr kill`
        // takes the default action, as it would with no guard at all.
        // SAFETY: signal(2) and raise(3) are async-signal-safe.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
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
/// Scoped to windows whose waits poll the flag, so `brokkr kill` / ctrl-C
/// during untracked work (a cargo build outside an orchestrator, ...)
/// terminates brokkr immediately instead of being silently swallowed into a
/// flag nobody polls. Where a guarded window does block somewhere that does
/// not poll, the second signal escalates to the default action (see
/// [`shutdown_handler`]).
///
/// **Re-entrant.** Guards nest the way the global lock does: the outermost
/// install clears any stale request and installs the handlers, inner installs
/// only count, and only the outermost drop restores `SIG_DFL` and clears the
/// flag. Before, an inner guard (the sidecar's, inside an orchestrator's)
/// cleared a request that had already landed, and its drop uncovered the
/// outer window - Ctrl-C then killed brokkr outright, with no mock teardown
/// and no toolchain restore.
pub struct SigtermGuard {
    _private: (),
}

fn guard_depth() -> std::sync::MutexGuard<'static, usize> {
    GUARD_DEPTH.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl SigtermGuard {
    pub fn install() -> Self {
        let mut depth = guard_depth();
        if *depth == 0 {
            SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
            GUARD_ACTIVE.store(true, Ordering::SeqCst);
            let h: libc::sighandler_t = shutdown_handler as *const () as usize;
            set_handler(libc::SIGTERM, h);
            set_handler(libc::SIGINT, h);
        }
        *depth += 1;
        Self { _private: () }
    }
}

impl Drop for SigtermGuard {
    fn drop(&mut self) {
        let mut depth = guard_depth();
        *depth = depth.saturating_sub(1);
        if *depth > 0 {
            // An outer guard still owns the window, and any pending request
            // is still its to act on.
            return;
        }
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
        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Test process groups die with brokkr.
//
// The test runners spawn their children with `process_group(0)` so the
// watchdog can kill a whole test tree with one `kill(-pgid)`. The price is that
// terminal Ctrl-C (delivered to brokkr's foreground group only) and `brokkr
// kill` (SIGTERM to brokkr's pid only) never reach those groups. Brokkr dying
// on the default action left the cargo and test processes running,
// unreapable by the stray reaper, which knows only cargo-family names and so
// never matches a directly executed test binary.
//
// So the runners REGISTER each test group, and the registration is what makes
// an interrupt reach it whatever else is going on. Under a `SigtermGuard` -
// which `check`, `brokkr test` and `brokkr clippy` now hold for their whole
// run, so a graceful `brokkr kill` ends them as `Interrupted` / exit 130 - the
// guard's handler kills the registered groups before setting its flag, so a
// runner blocked in `wait()` is released at once rather than on its next poll.
// Outside a guard, while any group is registered, SIGINT/SIGTERM run a handler
// that SIGKILLs every registered group and then re-raises the signal under the
// default action - brokkr dies exactly as it would have, taking its test groups
// with it. Everything either handler does (`kill`, `signal`, `raise`, atomic
// loads) is async-signal-safe.
//
// What this cannot cover is brokkr being SIGKILLed: no handler runs.
// `brokkr kill --hard` walks brokkr's process tree itself
// ([`kill_descendants`]) before killing brokkr, which reaches a test binary
// under cargo. For anything else (the OOM killer) [`die_with_parent`] takes
// the direct child down at once, and what sits below it - the test binary
// under cargo - is reaped by the next fresh hold through the token every
// test process carries (`crate::test_orphans`).
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
/// lane, cargo on the others); grandchildren are the [`GroupReaper`]'s job
/// while brokkr lives, and `crate::test_orphans`' once it has died unannounced.
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

// ---------------------------------------------------------------------------
// Killing a process tree.
//
// Two callers need "everything this brokkr started, wherever it sits": the
// `check` watchdog (for its own tree) and `brokkr kill --hard` (for the lock
// holder's). Neither a group signal nor the lockfile's child slot reaches it
// all - test runners put their children in their own groups, and a test
// binary under cargo is recorded nowhere - so both walk `/proc`.
// ---------------------------------------------------------------------------

/// Every live process whose parent chain reaches `root`, deepest first, each
/// paired with its `/proc` starttime (the identity token) from the same read
/// as its parent pid. Deepest first so a leaf dies before its parent can
/// notice and respawn it. `root` itself is not included.
pub fn descendants(root: u32) -> Vec<(u32, String)> {
    let mut children: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Some((ppid, starttime)) = proc_ppid_and_starttime(pid) {
            children.entry(ppid).or_default().push((pid, starttime));
        }
    }
    let mut out = Vec::new();
    let mut stack: Vec<(u32, String)> = vec![(root, String::new())];
    // Bounded: a consistent snapshot has no parent cycle, but an inconsistent
    // one (a pid reused mid-scan) could, and a bound costs nothing.
    while let Some((pid, starttime)) = stack.pop() {
        if out.len() > 65_536 {
            break;
        }
        if pid != root {
            out.push((pid, starttime));
        }
        if let Some(kids) = children.remove(&pid) {
            stack.extend(kids);
        }
    }
    out.reverse();
    out
}

/// SIGKILL every descendant of `root` (see [`descendants`]), deepest first.
/// Each signal is identity-checked: it is sent only if the pid still carries
/// the starttime recorded by the walk, so a pid that exited and was reused
/// between the read and the signal is left alone - the rule `stray::kill`
/// applies. Returns how many processes were signalled.
///
/// Not atomic containment: a process can fork between the walk and the
/// signal. Callers that must not leave such a straggler call this twice (the
/// second walk sees the children of anything the first missed), or freeze the
/// root first so nothing new is started from it.
pub fn kill_descendants(root: u32) -> usize {
    let mut killed = 0usize;
    for (pid, starttime) in descendants(root) {
        if starttime.is_empty()
            || crate::lockfile::proc_starttime(pid).as_deref() != Some(starttime.as_str())
        {
            continue;
        }
        // SAFETY: SIGKILL to a pid whose identity was re-verified immediately
        // above; ESRCH (already gone) is benign, and the residual window is
        // the one every pid-addressed signal has.
        if unsafe { libc::kill(pid.cast_signed(), libc::SIGKILL) } == 0 {
            killed += 1;
        }
    }
    killed
}

/// The parent pid and starttime from `/proc/<pid>/stat`, from one read so the
/// pair is coherent. The fields follow the parenthesised comm (which may
/// itself contain spaces and parentheses, hence the `rfind`): state, then
/// ppid; starttime is field 22 of the whole line, index 19 after the comm -
/// the same indexing as `lockfile::proc_starttime`.
fn proc_ppid_and_starttime(pid: u32) -> Option<(u32, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let comm_end = stat.rfind(')')?;
    let post: Vec<&str> = stat.get(comm_end + 2..)?.split_whitespace().collect();
    let ppid = post.get(1)?.parse().ok()?;
    let starttime = post.get(19).map(|s| (*s).to_owned()).unwrap_or_default();
    Some((ppid, starttime))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn own_process_is_a_descendant_of_its_grandparent() {
        let me = std::process::id();
        let (parent, _) = proc_ppid_and_starttime(me).unwrap();
        let (grandparent, _) = proc_ppid_and_starttime(parent).unwrap();
        if grandparent == 0 {
            return; // init as parent: no grandparent to walk from
        }
        let found = descendants(grandparent);
        let mine = found.iter().find(|(pid, _)| *pid == me).unwrap();
        // The identity token comes from the same read as the topology.
        assert_eq!(Some(mine.1.clone()), crate::lockfile::proc_starttime(me));
    }

    #[test]
    fn descendants_exclude_the_root() {
        let me = std::process::id();
        assert!(!descendants(me).iter().any(|(pid, _)| *pid == me));
    }

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
