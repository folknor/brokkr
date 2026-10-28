//! Shared process identity, `/proc` snapshots and verified signalling.

mod helpers;
mod identity;
pub(crate) mod inflight;
mod reap;
pub(crate) mod spawn;
mod table;

pub(crate) use self::helpers::signal_process_group_raw;
pub use self::helpers::{signal_pid, signal_process_group, snapshot_proc};
pub use self::identity::{BootId, HostScope, NsInode, Pid, ProcIdentity, StartTicks};
pub use self::reap::{
    Delivery, Generation, PidfdRefusal, ReaperExemptions, VerifiedProcess, generation,
    kill_verified, pidfd_send_signal,
};
pub use self::table::{ProcStat, ProcTable};

/// Read this kernel's boot identifier.
pub fn local_boot_id() -> Option<BootId> {
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    BootId::new(value.trim())
}

/// Read an inode number from a namespace link.
pub fn namespace_inode(path: &str) -> Option<NsInode> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| NsInode::new(metadata.ino()))
}

/// Capture the start ticks from a process's current `/proc` record.
pub fn proc_starttime(pid: u32) -> Option<StartTicks> {
    let pid = Pid::new(pid)?;
    ProcStat::read(pid).ok().map(|stat| stat.identity().start())
}

/// Whether this `/proc` numbers processes in this process's own PID namespace.
///
/// `NSpid` in `/proc/self/status` lists this process's PID in every namespace
/// from the procfs's own down to ours, so a procfs mounted for an ancestor
/// namespace shows more than one entry, and one for an unrelated namespace has
/// no `/proc/self` at all. Readable by anyone, unlike `/proc/1/ns/pid`, which
/// needs ptrace access to init: comparing that link was the earlier rule, and
/// on a host it failed for every non-root user, so no recorded PID ever
/// verified there.
pub fn procfs_is_own_namespace() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .is_ok_and(|status| nspid_is_own(&status, std::process::id()))
}

fn nspid_is_own(status: &str, pid: u32) -> bool {
    let Some(line) = status.lines().find_map(|l| l.strip_prefix("NSpid:")) else {
        return false;
    };
    let mut entries = line.split_whitespace();
    entries.next().and_then(|p| p.parse::<u32>().ok()) == Some(pid) && entries.next().is_none()
}

/// Compare the namespace tokens from a lock record with this process.
/// Records predating namespace fields retain the previous procfs-only rule.
pub fn shares_namespace_tokens(pid_ns: &str, time_ns: &str) -> bool {
    let Some(local_scope) = HostScope::current() else {
        return false;
    };
    if !procfs_is_own_namespace() {
        return false;
    }
    if pid_ns.is_empty() && time_ns.is_empty() {
        return true;
    }
    let (Some(holder_pid), Some(holder_time)) = (NsInode::parse(pid_ns), NsInode::parse(time_ns))
    else {
        return false;
    };
    local_scope.pid_ns() == holder_pid && local_scope.time_ns() == holder_time
}

/// Return the holder's namespace when it differs from this process's.
pub fn foreign_pid_namespace(pid_ns: &str) -> Option<NsInode> {
    let holder = NsInode::parse(pid_ns)?;
    let own = namespace_inode("/proc/self/ns/pid")?;
    (holder != own).then_some(holder)
}

/// The system clock tick rate used to convert `/proc` CPU/start values.
pub fn clock_ticks_per_second() -> Option<u64> {
    // SAFETY: `sysconf` receives a constant selector and no pointers.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks).ok().filter(|ticks| *ticks > 0)
}

/// Whether a recorded process identity is still the same visible process.
pub fn identity_matches(identity: ProcIdentity) -> bool {
    ProcStat::read(identity.pid()).is_ok_and(|stat| stat.identity() == identity)
}

/// Verify the process generation and the boot that gives its start ticks
/// meaning.
pub fn verify_identity(identity: ProcIdentity, boot: &BootId) -> bool {
    local_boot_id().as_ref() == Some(boot) && identity_matches(identity)
}

#[cfg(test)]
mod tests {
    use super::nspid_is_own;

    /// One `NSpid` entry equal to our PID: the procfs is ours. An ancestor's
    /// procfs lists the outer PIDs first; a missing line proves nothing.
    #[test]
    fn nspid_decides_whose_procfs_this_is() {
        assert!(nspid_is_own("Name:\tx\nNSpid:\t4242\n", 4242));
        assert!(!nspid_is_own("NSpid:\t9001\t2\n", 2));
        assert!(!nspid_is_own("NSpid:\t9001\t2\n", 9001));
        assert!(!nspid_is_own("NSpid:\t17\n", 4242));
        assert!(!nspid_is_own("Name:\tx\nPid:\t4242\n", 4242));
    }
}
