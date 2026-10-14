//! Compatibility surface for lock acquisition and lock-record operations.
//!
//! The implementation lives under `lock/`; process identity primitives live
//! under `process/`. Existing callers keep this path while they migrate.

#[path = "lock/mod.rs"]
mod lock;

pub use self::lock::*;
pub(crate) use self::lock::{AcqId, AuthHash, Nonce, RunToken};
pub(crate) use self::lock::{escape_value, unescape_value};

pub use crate::process::PidfdRefusal;

/// Open a pidfd after the shared process layer has verified the recorded
/// process both before and after opening it.
pub fn open_verified_pidfd(
    pid: u32,
    starttime: &str,
    boot_id: &str,
) -> Result<std::os::fd::OwnedFd, PidfdRefusal> {
    crate::process::open_legacy_pidfd(pid, starttime, boot_id)
}

/// Send a signal to an already verified pidfd.
pub fn pidfd_send_signal(
    pidfd: &std::os::fd::OwnedFd,
    signal: libc::c_int,
) -> std::io::Result<bool> {
    crate::process::pidfd_send_signal(pidfd, signal)
}

/// This kernel's boot id, in the lock protocol's string representation.
pub fn local_boot_id() -> Option<String> {
    crate::process::local_boot_id().map(|id| id.as_str().to_owned())
}

/// A process's start ticks, in the lock protocol's canonical decimal form.
pub fn proc_starttime(pid: u32) -> Option<String> {
    crate::process::proc_starttime(pid).map(|ticks| ticks.get().to_string())
}
