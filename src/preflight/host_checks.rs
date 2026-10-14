use crate::error::DevError;

/// A single requirement that must be satisfied before a subcommand runs.
///
/// Only requirements some command actually checks live here; a new kind of
/// preflight check adds its variant together with its first caller.
pub enum Check {
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
        Check::IoUring => {
            let failures = crate::host::kernel::io_uring().failures();
            (!failures.is_empty()).then(|| failures.join("\n"))
        }
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
