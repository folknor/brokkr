use std::num::NonZeroU32;

/// A nonzero process id in the current procfs/PID-namespace view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pid(NonZeroU32);

impl Pid {
    pub fn new(value: u32) -> Option<Self> {
        NonZeroU32::new(value).map(Self)
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// The kernel's process start-time counter, in clock ticks since boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StartTicks(u64);

impl StartTicks {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn parse(value: &str) -> Option<Self> {
        value.parse().ok().map(Self)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The kernel boot identifier associated with process start ticks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BootId(String);

impl BootId {
    pub fn new(value: &str) -> Option<Self> {
        let value = value.trim();
        (!value.is_empty()).then(|| Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An inode identity for a Linux namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NsInode(u64);

impl NsInode {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn parse(value: &str) -> Option<Self> {
        value.parse().ok().map(Self)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The stable identity of one process within a boot and PID namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcIdentity {
    pid: Pid,
    start: StartTicks,
}

impl ProcIdentity {
    pub const fn new(pid: Pid, start: StartTicks) -> Self {
        Self { pid, start }
    }

    pub const fn pid(self) -> Pid {
        self.pid
    }

    pub const fn start(self) -> StartTicks {
        self.start
    }
}

/// The host and namespace scope in which a process identity is meaningful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostScope {
    boot: BootId,
    pid_ns: NsInode,
    time_ns: NsInode,
}

impl HostScope {
    pub fn new(boot: BootId, pid_ns: NsInode, time_ns: NsInode) -> Self {
        Self {
            boot,
            pid_ns,
            time_ns,
        }
    }

    pub fn boot(&self) -> &BootId {
        &self.boot
    }

    pub const fn pid_ns(&self) -> NsInode {
        self.pid_ns
    }

    pub const fn time_ns(&self) -> NsInode {
        self.time_ns
    }

    pub fn current() -> Option<Self> {
        Some(Self::new(
            super::local_boot_id()?,
            super::namespace_inode("/proc/self/ns/pid")?,
            super::namespace_inode("/proc/thread-self/ns/time")?,
        ))
    }
}
