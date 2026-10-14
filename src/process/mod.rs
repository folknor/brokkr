//! Shared process identity, `/proc` snapshots and verified signalling.

mod identity;
mod reap;
mod table;

pub use self::identity::{BootId, HostScope, NsInode, Pid, ProcIdentity, StartTicks};
pub use self::reap::{
    Delivery, PidfdRefusal, ReaperExemptions, VerifiedProcess, kill_verified, open_legacy_pidfd,
    pidfd_send_signal,
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

/// Compare the namespace tokens from a lock record with this process.
/// Records predating namespace fields retain the previous procfs-only rule.
pub fn shares_namespace_tokens(pid_ns: &str, time_ns: &str) -> bool {
    let Some(local_scope) = HostScope::current() else {
        return false;
    };
    if namespace_inode("/proc/1/ns/pid") != Some(local_scope.pid_ns()) {
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
