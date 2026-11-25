//! The supervised group leader: the one handle every group signal and every
//! reap of a test run's leader goes through.
//!
//! A numeric `kill(-pgid)` sent after the runner reaped the leader can land on
//! an unrelated group that reused the number. `pidfd_send_signal` with
//! `PIDFD_SIGNAL_PROCESS_GROUP` (Linux 6.9) signals the group of the pid object
//! the fd was opened on, so reaping cannot redirect it, and it needs no
//! userspace lock. On an older kernel the fallback is a numeric send under a
//! lifecycle mutex that every reap also takes: a numeric signal is sent only
//! while the leader is unreaped, and is skipped after.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::evidence::{ProcDir, Root};

/// `PIDFD_SIGNAL_PROCESS_GROUP` from `linux/pidfd.h`.
const PIDFD_SIGNAL_PROCESS_GROUP: libc::c_uint = 1 << 2;

/// Set once a group pidfd signal with a valid signal came back `EINVAL`: this
/// kernel predates the flag, and every later send takes the numeric path.
static GROUP_PIDFD_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// Supervised group leaders this process has spawned and not yet reaped. A
/// proxy for how many test groups brokkr had running: it counts an exited
/// leader still awaiting its reap, and misses a group whose members outlive
/// the leader's reap.
static UNREAPED_LEADERS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn unreaped_leaders() -> usize {
    UNREAPED_LEADERS.load(Ordering::Relaxed)
}

/// How one group signal attempt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SignalOutcome {
    /// The kernel accepted the signal for the group.
    Sent,
    /// The group no longer exists.
    Gone,
    /// The numeric fallback found the leader already reaped, so it had no
    /// safe target and sent nothing.
    SkippedReaped,
    Failed(String),
}

impl SignalOutcome {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Sent => "sent".into(),
            Self::Gone => "the group was already gone".into(),
            Self::SkippedReaped => {
                "not sent: the leader was already reaped and this kernel cannot signal its group safely".into()
            }
            Self::Failed(e) => format!("failed: {e}"),
        }
    }
}

pub(crate) struct LeaderHandle {
    pgid: u32,
    /// The leader's `/proc` directory, opened at spawn while it is an
    /// unreaped child: the root every evidence walk reads through, so a
    /// reused pid is never read as the leader. Opening it takes no lock of
    /// the target's. `None` when the open failed.
    dir: Option<Arc<ProcDir>>,
    pidfd: Option<OwnedFd>,
    /// Whether the leader has been reaped. Taken by the numeric signal path and
    /// by every reap; never by the group-pidfd path.
    reaped: Mutex<bool>,
}

impl LeaderHandle {
    /// Take ownership of a freshly spawned, unreaped group leader. The pidfd
    /// is opened here, before any reap, so it names this process.
    pub(crate) fn new(child: &Child) -> Self {
        let pgid = child.id();
        UNREAPED_LEADERS.fetch_add(1, Ordering::Relaxed);
        Self {
            pgid,
            dir: ProcDir::open(pgid).ok().map(Arc::new),
            pidfd: crate::process::pidfd::open_own(pgid).ok(),
            reaped: Mutex::new(false),
        }
    }

    pub(crate) fn dir(&self) -> Option<&Arc<ProcDir>> {
        self.dir.as_ref()
    }

    /// The leader as a report candidate. No start time: its rows come
    /// through [`Self::dir`], so they are this process's by construction.
    pub(crate) fn root(&self) -> Root {
        Root {
            pid: self.pgid,
            start: None,
        }
    }

    pub(crate) fn pgid(&self) -> u32 {
        self.pgid
    }

    pub(crate) fn signal_group(&self, signal: libc::c_int) -> SignalOutcome {
        if let Some(fd) = &self.pidfd
            && !GROUP_PIDFD_UNSUPPORTED.load(Ordering::Relaxed)
        {
            // SAFETY: a valid pidfd, a null siginfo (the kernel fills a
            // SI_USER one) and a flag word; no other pointers.
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    PIDFD_SIGNAL_PROCESS_GROUP,
                )
            };
            if ret == 0 {
                return SignalOutcome::Sent;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => return SignalOutcome::Gone,
                // Every caller passes a valid signal, so EINVAL is the flag
                // this kernel does not know.
                Some(libc::EINVAL) => GROUP_PIDFD_UNSUPPORTED.store(true, Ordering::Relaxed),
                _ => return SignalOutcome::Failed(error.to_string()),
            }
        }
        let reaped = self
            .reaped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *reaped {
            return SignalOutcome::SkippedReaped;
        }
        match crate::process::signal_process_group(self.pgid, signal) {
            Ok(true) => SignalOutcome::Sent,
            Ok(false) => SignalOutcome::Gone,
            Err(e) => SignalOutcome::Failed(e.to_string()),
        }
    }

    /// A nonblocking reap, under the lifecycle lock so the numeric fallback
    /// never signals a reaped leader's number.
    pub(crate) fn try_reap(&self, child: &mut Child) -> io::Result<Option<ExitStatus>> {
        let mut reaped = self
            .reaped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *reaped {
            return Err(io::Error::other("leader already reaped"));
        }
        let status = child.try_wait()?;
        if status.is_some() {
            *reaped = true;
            UNREAPED_LEADERS.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(status)
    }

    /// Reap, waiting for the exit outside the lock.
    pub(crate) fn reap(&self, child: &mut Child) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_reap(child)? {
                return Ok(status);
            }
            self.wait_exit(Duration::from_millis(50));
        }
    }

    /// Wait up to `wait` for the leader to exit, without reaping it.
    pub(crate) fn wait_exit(&self, wait: Duration) {
        match &self.pidfd {
            Some(fd) => {
                crate::process::pidfd::wait(fd, wait);
            }
            None => std::thread::sleep(wait),
        }
    }
}

impl Drop for LeaderHandle {
    fn drop(&mut self) {
        let reaped = self
            .reaped
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*reaped {
            UNREAPED_LEADERS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
