//! Cooperative shutdown — `brokkr kill` asks brokkr to wrap up a running
//! bench cleanly rather than letting the user SIGKILL it and leave scratch
//! data behind.
//!
//! The protocol:
//!
//! 1. `brokkr kill` reads the lockfile and sends `SIGTERM` to the brokkr PID.
//! 2. `install_sigterm_handler` (called once at `main` entry) registers a
//!    handler that sets [`SHUTDOWN_REQUESTED`]. The handler itself does
//!    nothing else — it must be async-signal-safe.
//! 3. The sidecar loop polls [`is_shutdown_requested`] on every sample tick
//!    (alongside `try_wait` and the `--stop` marker check). When set, it
//!    `SIGKILL`s the child and breaks out of the loop with
//!    `stopped_by_signal = true`.
//! 4. `run_external` propagates that up as [`crate::error::DevError::Interrupted`]
//!    after saving the partial sidecar data under the `dirty` alias. `main`
//!    catches that error, runs the scratch-cleanup path, and exits 130.
//!
//! `brokkr kill --hard` bypasses this entirely: it sends `SIGKILL` to
//! both the brokkr PID and the recorded child PID, leaving the scratch
//! tree in whatever state the tool left it (users can follow up with
//! `brokkr clean`).

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Whether a shutdown has been requested via SIGTERM.
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
}

/// Register a SIGTERM handler that sets [`SHUTDOWN_REQUESTED`]. Called
/// once from `main`. The handler is async-signal-safe: a single
/// atomic store, no allocation, no logging.
pub fn install_sigterm_handler() {
    extern "C" fn handler(_: libc::c_int) {
        SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
    }
    // SAFETY: `signal` with a plain function pointer is safe; the
    // handler body only touches an AtomicBool which is itself
    // async-signal-safe to write.
    let h: libc::sighandler_t = handler as *const () as usize;
    unsafe {
        libc::signal(libc::SIGTERM, h);
    }
}
