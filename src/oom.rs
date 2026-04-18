use std::process::Command;

use crate::error::DevError;
use crate::output;

/// Set the child process as the preferred OOM kill target.
///
/// Writes `1000` to `/proc/self/oom_score_adj` in the child process before
/// exec, so the Linux OOM killer targets the benchmark rather than the desktop.
/// Uses raw libc syscalls (async-signal-safe between fork and exec).
/// Silently ignores errors - works in containers, degrades on non-Linux.
pub fn protect_child(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: Between fork and exec only async-signal-safe functions are allowed.
    // libc::open, libc::write, libc::close are all async-signal-safe per POSIX.
    unsafe {
        cmd.pre_exec(|| {
            let path = b"/proc/self/oom_score_adj\0";
            let fd = libc::open(path.as_ptr().cast::<libc::c_char>(), libc::O_WRONLY);
            if fd >= 0 {
                let value = b"1000";
                libc::write(fd, value.as_ptr().cast::<libc::c_void>(), value.len());
                libc::close(fd);
            }
            Ok(())
        });
    }
}

/// How aggressively a command uses memory relative to input size.
pub enum MemoryRisk {
    /// Streaming workload: ~2x input size.
    Normal,
    /// Allocation-tracking mode: ~4x input size.
    AllocTracking,
}

impl MemoryRisk {
    fn multiplier(&self) -> u64 {
        match self {
            MemoryRisk::Normal => 2,
            MemoryRisk::AllocTracking => 4,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            MemoryRisk::Normal => "streaming",
            MemoryRisk::AllocTracking => "allocation-tracking",
        }
    }
}

/// Check whether the estimated memory usage will fit in available RAM.
///
/// - Hard abort if estimated > available.
/// - Warning if estimated > 70% of available.
/// - No-op if `skip` is true or available memory is 0 (unreadable).
pub fn check_memory(input_mb: f64, risk: &MemoryRisk, skip: bool) -> Result<(), DevError> {
    if skip {
        return Ok(());
    }

    let (_total, avail) = crate::env::read_memory();
    if avail == 0 {
        return Ok(());
    }

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let input = input_mb.max(0.0) as u64;
    let estimated = input * risk.multiplier();

    if estimated > avail {
        return Err(DevError::Preflight(vec![format!(
            "estimated memory ~{estimated} MB exceeds available RAM ({avail} MB)\n\
             input: {input} MB, multiplier: {}x ({})\n\
             Use --no-mem-check to override.",
            risk.multiplier(),
            risk.label(),
        )]));
    }

    let threshold = avail * 7 / 10;
    if estimated > threshold {
        let pct = estimated * 100 / avail;
        output::error(&format!(
            "WARNING: estimated memory ~{estimated} MB is {pct}% of available RAM ({avail} MB)"
        ));
    }

    Ok(())
}
