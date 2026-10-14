//! Descriptor-level probes used by child-process output readers.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};

/// Read a ready chunk from a descriptor, returning `None` at EOF, on error,
/// or when cancellation is requested.
///
/// Polling with a short timeout makes cancellation observable even while the
/// descriptor is idle. This is used for child pipes whose reader thread must
/// be joined before the owning run finishes.
pub(crate) fn read_unless_cancelled<R: Read + AsRawFd>(
    pipe: &mut R,
    buf: &mut [u8],
    cancel: &AtomicBool,
) -> Option<usize> {
    loop {
        if cancel.load(Ordering::Acquire) {
            return None;
        }
        let mut pfd = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for a descriptor owned by the caller.
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready == 0 {
            continue;
        }
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        return match pipe.read(buf) {
            Ok(0) => None,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => None,
            Ok(n) => Some(n),
        };
    }
}
