//! Exit waits on a pidfd for a child the caller itself owns.
//!
//! A pidfd turns readable when its process exits, so a waiter wakes at the
//! exit rather than at the next poll. The child supervisor uses it between
//! ticks so a measured wall is taken at the exit, not on the tick grid; the
//! test runner uses it to stop billing a dead harness.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;

/// An unverified pidfd for a process the caller owns: its own child, not yet
/// reaped. The caller holding the unreaped child is what keeps the pid from
/// being recycled while it opens, so no identity check is needed.
pub(crate) fn open_own(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: a plain syscall with no pointer arguments.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.cast_signed(), 0_u32) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = RawFd::try_from(ret).map_err(|_| io::Error::other("pidfd out of range"))?;
    // SAFETY: a successful pidfd_open returns a fresh fd we uniquely own.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// How one bounded wait on a pidfd ended.
#[derive(Debug)]
pub(crate) enum PidfdWait {
    /// The pidfd is readable: the process has exited.
    Exited,
    /// The whole wait passed with the process still running.
    TimedOut,
    /// A signal cut the wait short; the caller re-waits what remains.
    Interrupted,
    /// The pidfd cannot be waited on; the caller stops using it.
    Failed(io::Error),
}

/// Wait up to `wait` for the process behind `fd` to exit. The timeout is
/// passed at full precision, not truncated to milliseconds, so a wait bounded
/// by a tick ends at the tick. Only `POLLIN` counts as an exit; any other
/// event set is a failure, so a broken fd cannot spin its caller.
pub(crate) fn wait(fd: &OwnedFd, wait: Duration) -> PidfdWait {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = libc::timespec {
        tv_sec: libc::time_t::try_from(wait.as_secs()).unwrap_or(libc::time_t::MAX),
        tv_nsec: libc::c_long::from(wait.subsec_nanos().cast_signed()),
    };
    // SAFETY: one valid pollfd and a valid timespec; no signal mask change.
    let ret = unsafe { libc::ppoll(&mut pfd, 1, &timeout, std::ptr::null()) };
    if ret < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            PidfdWait::Interrupted
        } else {
            PidfdWait::Failed(error)
        };
    }
    if ret == 0 {
        return PidfdWait::TimedOut;
    }
    if pfd.revents & libc::POLLIN != 0 {
        PidfdWait::Exited
    } else {
        PidfdWait::Failed(io::Error::other(format!(
            "pidfd poll returned events {:#x} without POLLIN",
            pfd.revents
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_wakes_at_the_exit_and_times_out_while_running() {
        let mut command = crate::process::spawn::ChildPolicy::plain_command("/bin/sleep");
        command.arg("0.2");
        let mut child = crate::process::spawn::spawn(command, None).expect("spawn sleep");
        let fd = open_own(child.id()).expect("pidfd");
        assert!(matches!(
            wait(&fd, Duration::from_millis(1)),
            PidfdWait::TimedOut
        ));
        let started = std::time::Instant::now();
        assert!(matches!(
            wait(&fd, Duration::from_secs(30)),
            PidfdWait::Exited
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
        child.wait().expect("reap");
    }
}
