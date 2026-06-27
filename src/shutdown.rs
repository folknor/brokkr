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
//!    mid-`brokkr check` is what the user wants anyway (no child to reap,
//!    no scratch to clean).
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
//! `brokkr clean`).

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

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
        let h: libc::sighandler_t = shutdown_handler as *const () as usize;
        set_handler(libc::SIGTERM, h);
        set_handler(libc::SIGINT, h);
        Self
    }
}

impl Drop for SigtermGuard {
    fn drop(&mut self) {
        set_handler(libc::SIGTERM, libc::SIG_DFL);
        set_handler(libc::SIGINT, libc::SIG_DFL);
        // Reset the flag so a captured subprocess invoked AFTER this
        // guard's scope (e.g. in main's cleanup path) doesn't see a
        // sticky `true` left over from a SIGTERM/SIGINT that already
        // fired and was handled. Without this, the captured runner's
        // flag-poll loop would spuriously SIGTERM unrelated children.
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
    }
}
