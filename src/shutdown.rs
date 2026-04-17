//! Cooperative shutdown — `brokkr kill` asks brokkr to wrap up a running
//! bench cleanly rather than letting the user SIGKILL it and leave scratch
//! data behind.
//!
//! The protocol:
//!
//! 1. `brokkr kill` reads the lockfile and sends `SIGTERM` to the brokkr PID.
//! 2. A [`SigtermGuard`] is installed for the lifetime of each sidecar run.
//!    Its handler sets [`SHUTDOWN_REQUESTED`] and nothing else — it must be
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

/// Whether a shutdown has been requested via SIGTERM since the current
/// `SigtermGuard` was installed.
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

extern "C" fn sigterm_handler(_: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
}

fn set_sigterm(handler: libc::sighandler_t) {
    // SAFETY: `signal` with a plain function pointer is safe; the
    // handler body only touches an AtomicBool which is itself
    // async-signal-safe to write.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
    }
}

/// RAII guard that installs the SIGTERM handler for the duration of a
/// sidecar run and restores the default action on drop. Scoped this
/// tightly so `brokkr kill` during non-sidecar work (cargo build,
/// brokkr check, …) terminates brokkr immediately instead of being
/// silently swallowed into a flag nobody polls.
pub struct SigtermGuard;

impl SigtermGuard {
    pub fn install() -> Self {
        SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
        let h: libc::sighandler_t = sigterm_handler as *const () as usize;
        set_sigterm(h);
        Self
    }
}

impl Drop for SigtermGuard {
    fn drop(&mut self) {
        set_sigterm(libc::SIG_DFL);
    }
}
