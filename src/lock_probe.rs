// Shared, non-blocking observation of the global exclusive lock. `include!`d
// into both `lockfile` and the `brokkr-rustc-guard` bin, so it carries `//`
// comments, not `//!` (an inner doc comment is rejected inside `include!`).

use std::os::fd::RawFd;

/// Try to take a shared flock on `fd`.
///
/// `Ok(true)` means the shared lock was acquired and remains held until `fd`
/// is closed. `Ok(false)` means an exclusive holder conflicts with the probe.
/// Other errors do not establish whether the lock is held.
pub(super) fn try_shared(fd: RawFd) -> std::io::Result<bool> {
    // SAFETY: the caller keeps this open fd alive for the probe and any reads
    // protected by a successfully acquired shared lock.
    if unsafe { libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }

    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}
