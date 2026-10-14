//! Kernel version and io_uring capability facts.

use std::fmt;

pub(crate) const MIN_IO_URING_MEMLOCK_BYTES: u64 = 16 * 1024 * 1024;

/// Current soft RLIMIT_MEMLOCK, or why it could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemlockLimit {
    Bytes(u64),
    Unlimited,
    Unknown,
}

impl fmt::Display for MemlockLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => write!(f, "memlock={} KB", bytes / 1024),
            Self::Unlimited => f.write_str("memlock=unlimited"),
            Self::Unknown => f.write_str("memlock=unknown"),
        }
    }
}

/// Kernel settings and resource limit that determine whether io_uring is
/// available to the current process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IoUringInfo {
    pub kernel_blockers: Vec<&'static str>,
    pub memlock: MemlockLimit,
}

impl IoUringInfo {
    pub(crate) fn is_supported(&self) -> bool {
        self.failures().is_empty()
    }

    pub(crate) fn status(&self) -> String {
        if self.is_supported() {
            return format!("supported ({})", self.memlock);
        }
        let reason = self
            .kernel_blockers
            .first()
            .copied()
            .or(match self.memlock {
                MemlockLimit::Bytes(bytes) if bytes < MIN_IO_URING_MEMLOCK_BYTES => {
                    Some("RLIMIT_MEMLOCK too low")
                }
                MemlockLimit::Unknown => Some("RLIMIT_MEMLOCK unknown"),
                _ => None,
            });
        match reason {
            Some(reason) => format!("disabled: {reason} ({})", self.memlock),
            None => format!("disabled: unavailable ({})", self.memlock),
        }
    }

    pub(crate) fn failures(&self) -> Vec<String> {
        let mut failures = self
            .kernel_blockers
            .iter()
            .map(|reason| match *reason {
                "kernel kill switch" => "io_uring is disabled by kernel\nFix: sudo sysctl -w kernel.io_uring_disabled=0".to_owned(),
                "AppArmor io_uring restriction" => "AppArmor restricts unprivileged io_uring\nFix: sudo sysctl -w kernel.apparmor_restrict_unprivileged_io_uring=0".to_owned(),
                _ => "AppArmor restricts unprivileged user namespaces\nFix: sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0".to_owned(),
            })
            .collect::<Vec<_>>();
        match self.memlock {
            MemlockLimit::Bytes(bytes) if bytes < MIN_IO_URING_MEMLOCK_BYTES => failures.push(
                format!(
                    "RLIMIT_MEMLOCK too low for io_uring\nFix: sudo prlimit --pid=$$ --memlock=unlimited:unlimited\nCurrent limit: {} KB, need at least {} MB",
                    bytes / 1024,
                    MIN_IO_URING_MEMLOCK_BYTES / (1024 * 1024),
                ),
            ),
            MemlockLimit::Unknown => failures.push(
                "RLIMIT_MEMLOCK could not be read for io_uring".to_owned(),
            ),
            _ => {}
        }
        failures
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KernelInfo {
    pub version: Option<String>,
    pub io_uring: IoUringInfo,
}

impl Default for IoUringInfo {
    fn default() -> Self {
        Self {
            kernel_blockers: Vec::new(),
            memlock: MemlockLimit::Unknown,
        }
    }
}

pub(crate) fn collect() -> KernelInfo {
    let version = kernel_version();
    KernelInfo {
        version,
        io_uring: io_uring(),
    }
}

pub(crate) fn kernel_version() -> Option<String> {
    std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|content| extract_kernel_version(&content))
}

pub(crate) fn io_uring() -> IoUringInfo {
    let kernel_blockers = [
        ("/proc/sys/kernel/io_uring_disabled", "kernel kill switch"),
        (
            "/proc/sys/kernel/apparmor_restrict_unprivileged_io_uring",
            "AppArmor io_uring restriction",
        ),
        (
            "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
            "AppArmor userns restriction",
        ),
    ]
    .into_iter()
    .filter_map(|(path, reason)| kernel_param_nonzero(path).then_some(reason))
    .collect();
    IoUringInfo {
        kernel_blockers,
        memlock: read_memlock_limit(),
    }
}

pub(crate) fn extract_kernel_version(content: &str) -> Option<String> {
    content.split_whitespace().nth(2).map(str::to_owned)
}

fn kernel_param_nonzero(path: &str) -> bool {
    match std::fs::read_to_string(path) {
        Ok(content) => content.trim() != "0",
        Err(_) => false,
    }
}

fn read_memlock_limit() -> MemlockLimit {
    let mut rlim: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut rlim) } != 0 {
        return MemlockLimit::Unknown;
    }
    if rlim.rlim_cur == libc::RLIM_INFINITY {
        MemlockLimit::Unlimited
    } else {
        MemlockLimit::Bytes(rlim.rlim_cur as u64)
    }
}
