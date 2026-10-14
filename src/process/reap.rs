use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::{BootId, Pid, ProcIdentity, ProcStat};

/// Why an identity could not be bound to a pidfd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidfdRefusal {
    /// Recorded tokens do not describe a process visible in this scope.
    Unverified,
    /// The process disappeared or pidfd creation was unavailable.
    OpenFailed,
    /// The PID changed generation while the pidfd was being opened.
    IdentityChanged,
}

/// A process identity pinned to its kernel pidfd.
pub struct VerifiedProcess {
    identity: ProcIdentity,
    pidfd: OwnedFd,
}

impl VerifiedProcess {
    /// Verify a process around pidfd creation. When supplied, the boot token
    /// must name this kernel before the numeric identity is examined.
    pub fn open(
        identity: ProcIdentity,
        expected_boot: Option<&BootId>,
    ) -> Result<Self, PidfdRefusal> {
        if let Some(expected) = expected_boot
            && super::local_boot_id().as_ref() != Some(expected)
        {
            return Err(PidfdRefusal::Unverified);
        }
        if !matches_now(identity) {
            return Err(PidfdRefusal::Unverified);
        }
        let pidfd = pidfd_open(identity.pid()).ok_or(PidfdRefusal::OpenFailed)?;
        if !matches_now(identity) {
            return Err(PidfdRefusal::IdentityChanged);
        }
        Ok(Self { identity, pidfd })
    }

    pub const fn identity(&self) -> ProcIdentity {
        self.identity
    }

    pub fn send_signal(&self, signal: libc::c_int) -> std::io::Result<bool> {
        pidfd_send_signal(&self.pidfd, signal)
    }

    pub fn into_pidfd(self) -> OwnedFd {
        self.pidfd
    }
}

/// The result of signalling a verified process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Signalled,
    Exited,
    Unverified,
}

/// Bind a process identity to a pidfd and deliver a signal through that fd.
/// A process that could not be verified is reported without sending a signal.
pub fn kill_verified(identity: ProcIdentity, signal: libc::c_int) -> std::io::Result<Delivery> {
    let Ok(process) = VerifiedProcess::open(identity, None) else {
        return Ok(Delivery::Unverified);
    };
    process.send_signal(signal).map(|sent| {
        if sent {
            Delivery::Signalled
        } else {
            Delivery::Exited
        }
    })
}

/// Open a pidfd for a legacy lock-record identity, preserving the old API at
/// `lockfile::open_verified_pidfd` while sharing the verification sequence.
pub fn open_legacy_pidfd(
    pid: u32,
    starttime: &str,
    boot_id: &str,
) -> Result<OwnedFd, PidfdRefusal> {
    let (Some(pid), Some(start), Some(boot)) = (
        Pid::new(pid),
        super::StartTicks::parse(starttime),
        BootId::new(boot_id),
    ) else {
        return Err(PidfdRefusal::Unverified);
    };
    VerifiedProcess::open(ProcIdentity::new(pid, start), Some(&boot))
        .map(VerifiedProcess::into_pidfd)
}

/// Send a signal through an already owned pidfd. `Ok(false)` means ESRCH.
pub fn pidfd_send_signal(pidfd: &OwnedFd, signal: libc::c_int) -> std::io::Result<bool> {
    // SAFETY: the pidfd is owned and live; a null siginfo is permitted.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if ret == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(err)
}

fn matches_now(identity: ProcIdentity) -> bool {
    ProcStat::read(identity.pid()).is_ok_and(|stat| stat.identity() == identity)
}

fn pidfd_open(pid: Pid) -> Option<OwnedFd> {
    let pid = libc::pid_t::try_from(pid.get()).ok()?;
    // SAFETY: `pidfd_open` takes scalar arguments and returns a new fd.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    let fd = i32::try_from(ret).ok()?;
    if fd < 0 {
        return None;
    }
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Shared process-tree exclusions for reapers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaperExemptions {
    pids: HashSet<u32>,
}

impl ReaperExemptions {
    /// Build the self/ancestor/descendant set from `(pid, ppid)` pairs.
    pub fn from_parent_pairs(me: u32, parent_pairs: impl IntoIterator<Item = (u32, u32)>) -> Self {
        let parents: HashMap<u32, u32> = parent_pairs.into_iter().collect();
        let mut pids = HashSet::from([me]);

        let mut cursor = me;
        while let Some(&parent) = parents.get(&cursor) {
            if parent == 0 || parent == cursor || !pids.insert(parent) {
                break;
            }
            cursor = parent;
        }

        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (&pid, &parent) in &parents {
            if parent != 0 && parent != pid {
                children.entry(parent).or_default().push(pid);
            }
        }
        let mut pending = vec![me];
        while let Some(parent) = pending.pop() {
            if let Some(descendants) = children.get(&parent) {
                for &child in descendants {
                    if pids.insert(child) {
                        pending.push(child);
                    }
                }
            }
        }
        Self { pids }
    }

    pub fn contains(&self, pid: u32) -> bool {
        self.pids.contains(&pid)
    }
}
