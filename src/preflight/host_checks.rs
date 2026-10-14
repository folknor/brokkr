use std::path::{Path, PathBuf};

use crate::error::DevError;

/// A single requirement that must be satisfied before a subcommand runs.
///
/// Some variants (File, DiskSpace, KernelParam) are dispatched in `run_check`
/// but not yet constructed by any caller - they exist for future preflight checks.
#[allow(dead_code)]
pub enum Check {
    /// Binary must exist in PATH.
    Binary { name: String, help: String },
    /// File must exist at path.
    File { path: PathBuf, description: String },
    /// Minimum free disk space in bytes.
    DiskSpace { path: PathBuf, min_bytes: u64 },
    /// Read a /proc or /sys file and check it contains expected value.
    KernelParam {
        path: &'static str,
        expected: &'static str,
        description: &'static str,
    },
    /// Read an integer from /proc or /sys and check it is at most `max_value`.
    KernelParamAtMost {
        path: &'static str,
        max_value: i32,
        description: &'static str,
    },
    /// Resource limit (rlimit) must be at least `min_bytes`.
    Rlimit {
        resource: libc::__rlimit_resource_t,
        min_bytes: u64,
        description: &'static str,
    },
    /// io_uring must be usable by this process (`crate::host::kernel::io_uring`).
    IoUring,
}

/// Run all checks, collecting failures. If any fail, return `DevError::Preflight`
/// with all failure messages (not just the first).
pub fn run_preflight(checks: &[Check]) -> Result<(), DevError> {
    let mut failures = Vec::new();

    for check in checks {
        if let Some(msg) = run_single(check) {
            failures.push(msg);
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(DevError::Preflight(failures))
    }
}

/// Run a single check. Returns `Some(message)` on failure, `None` on success.
fn run_single(check: &Check) -> Option<String> {
    match check {
        Check::Binary { name, help } => check_binary(name, help),
        Check::File { path, description } => check_file(path, description),
        Check::DiskSpace { path, min_bytes } => check_disk_space(path, *min_bytes),
        Check::KernelParam {
            path,
            expected,
            description,
        } => check_kernel_param(path, expected, description),
        Check::KernelParamAtMost {
            path,
            max_value,
            description,
        } => check_kernel_param_at_most(path, *max_value, description),
        Check::Rlimit {
            resource,
            min_bytes,
            description,
        } => check_rlimit(*resource, *min_bytes, description),
        Check::IoUring => {
            let failures = crate::host::kernel::io_uring().failures();
            (!failures.is_empty()).then(|| failures.join("\n"))
        }
    }
}

fn check_binary(name: &str, help: &str) -> Option<String> {
    if crate::tools::find_executable(name).is_some() {
        None
    } else {
        Some(format!("'{name}' not found in PATH ({help})"))
    }
}

fn check_file(path: &Path, description: &str) -> Option<String> {
    if path.exists() {
        None
    } else {
        Some(format!("{description}: {}", path.display()))
    }
}

fn check_disk_space(path: &Path, min_bytes: u64) -> Option<String> {
    match crate::host::storage::available_bytes(path) {
        Some(avail) if avail >= min_bytes => None,
        Some(avail) => Some(format!(
            "insufficient disk space at {}: {} MB available, {} MB required",
            path.display(),
            avail / (1024 * 1024),
            min_bytes / (1024 * 1024),
        )),
        None => Some(format!("could not check disk space at {}", path.display())),
    }
}

fn check_kernel_param(path: &str, expected: &str, description: &str) -> Option<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        // Not on Linux, or procfs not mounted - skip the check.
        Err(_) => return None,
    };

    let trimmed = content.trim();
    if trimmed == expected {
        None
    } else {
        Some(format!(
            "{description}: expected '{expected}', got '{trimmed}' (in {path})"
        ))
    }
}

fn check_kernel_param_at_most(path: &str, max_value: i32, description: &str) -> Option<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        // Not on Linux, or procfs not mounted - skip the check.
        Err(_) => return None,
    };

    let value: i32 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return Some(format!("{description}: could not parse {path}")),
    };

    if value <= max_value {
        None
    } else {
        Some(format!(
            "{description}: {path} is {value}, need <= {max_value}"
        ))
    }
}

fn check_rlimit(
    resource: libc::__rlimit_resource_t,
    min_bytes: u64,
    description: &str,
) -> Option<String> {
    let mut rlim: libc::rlimit = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::getrlimit(resource, &mut rlim) };
    if ret != 0 {
        return Some(format!("{description}: could not read resource limit"));
    }
    if rlim.rlim_cur >= min_bytes {
        None
    } else {
        let cur_mb = rlim.rlim_cur / (1024 * 1024);
        let min_mb = min_bytes / (1024 * 1024);
        Some(format!(
            "{description}: current {cur_mb} MB, need >= {min_mb} MB"
        ))
    }
}

// ---------------------------------------------------------------------------
// Convenience check sets
// ---------------------------------------------------------------------------

/// Preflight checks for io_uring: the kernel kill switch, the two AppArmor
/// restrictions and `RLIMIT_MEMLOCK`, read by the one host fact that `brokkr
/// env` reports too (`crate::host::kernel::io_uring`), so the report and the
/// refusal cannot disagree.
pub fn uring_checks() -> Vec<Check> {
    vec![Check::IoUring]
}

// ---------------------------------------------------------------------------
