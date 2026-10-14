use super::LockInfo;
use crate::process::{BootId, ProcIdentity};

/// One advisory line when the holder recorded the same codex thread id as
/// this process carries: almost always an earlier exec session of this very
/// agent, which an agent otherwise has no way to recognise from inside its
/// own sandbox. Advisory, because the id is an environment variable.
pub fn same_agent_hint(info: &LockInfo) -> Option<String> {
    let mine = std::env::var("CODEX_THREAD_ID").ok()?;
    if mine.is_empty() || mine != info.codex_thread {
        return None;
    }
    Some(format!(
        "the holder reports the same codex thread as this shell ({mine}): it is very likely one of \
         your own earlier commands, still running in another exec session. Wait for it, stop that \
         session, or run `brokkr kill`"
    ))
}

/// Get how long a verified process has been running, as a human-readable
/// string. `None` when identity verification fails - an unverified PID's
/// uptime is some other process's uptime (in the kthreadd case, the whole
/// machine's).
pub fn verified_uptime_for(identity: ProcIdentity, boot: &BootId) -> Option<String> {
    if !crate::process::verify_identity(identity, boot) {
        return None;
    }
    let uptime = process_uptime_str(identity.pid().get())?;
    crate::process::verify_identity(identity, boot).then_some(uptime)
}

/// Get how long a process has been running, as a human-readable string.
///
/// Reads `/proc/{pid}/stat` starttime and compares against system uptime.
/// Display only - reached through [`verified_uptime_for`] /
/// [`verified_summary_for`], which verify identity on both sides of the read.
fn process_uptime_str(pid: u32) -> Option<String> {
    #[allow(clippy::cast_precision_loss)]
    let clk_tck = crate::process::clock_ticks_per_second()? as f64;
    if clk_tck <= 0.0 {
        return None;
    }

    // System uptime in seconds.
    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;

    // Process start time in clock ticks since boot.
    #[allow(clippy::cast_precision_loss)]
    let starttime = crate::process::proc_starttime(pid)?.get() as f64;

    let start_secs = starttime / clk_tck;
    let elapsed_secs = uptime_secs - start_secs;

    if elapsed_secs < 0.0 {
        return None;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let elapsed = elapsed_secs as u64;
    Some(crate::output::format_duration(
        std::time::Duration::from_secs(elapsed),
    ))
}

/// Build a one-line summary of a running, identity-verified process from
/// `/proc`.
///
/// Returns something like `"running 12m, RSS 4.2 GB, 847 MB read, 4 threads"`.
/// Returns `None` if verification fails, the process is gone, or `/proc` is
/// unreadable. Identity is checked *before and after* collection: the
/// process could exit and its PID be recycled while `/proc/{pid}/status`
/// and `/proc/{pid}/io` are being read, and stats half-read from two
/// different processes must not be displayed.
pub fn verified_summary_for(identity: ProcIdentity, boot: &BootId) -> Option<String> {
    if !crate::process::verify_identity(identity, boot) {
        return None;
    }
    let summary = process_summary_unverified(identity.pid().get())?;
    crate::process::verify_identity(identity, boot).then_some(summary)
}

/// Stat collection body of [`verified_summary_for`]. Never call for display
/// without the verification sandwich around it.
fn process_summary_unverified(pid: u32) -> Option<String> {
    let uptime = process_uptime_str(pid)?;

    // Read /proc/{pid}/status for RSS.
    let status_path = format!("/proc/{pid}/status");
    let status_text = std::fs::read_to_string(&status_path).ok()?;
    let mut rss_kb: i64 = 0;
    let mut threads: i64 = 0;
    for line in status_text.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            let val_str = rest.trim().trim_end_matches(" kB");
            match key {
                "VmRSS" => rss_kb = val_str.parse().unwrap_or(0),
                "Threads" => threads = val_str.parse().unwrap_or(0),
                _ => {}
            }
        }
    }

    // Read /proc/{pid}/io for bytes read.
    let io_path = format!("/proc/{pid}/io");
    let mut read_bytes: i64 = 0;
    let mut write_bytes: i64 = 0;
    if let Ok(io_text) = std::fs::read_to_string(&io_path) {
        for line in io_text.lines() {
            if let Some((key, rest)) = line.split_once(':') {
                let val: i64 = rest.trim().parse().unwrap_or(0);
                match key {
                    "read_bytes" => read_bytes = val,
                    "write_bytes" => write_bytes = val,
                    _ => {}
                }
            }
        }
    }

    let mut parts = Vec::with_capacity(5);
    parts.push(format!("running {uptime}"));

    if rss_kb > 0 {
        parts.push(format_bytes_kb(rss_kb, "RSS"));
    }
    if read_bytes > 0 {
        parts.push(format_bytes(read_bytes, "read"));
    }
    if write_bytes > 0 {
        parts.push(format_bytes(write_bytes, "written"));
    }
    if threads > 1 {
        parts.push(format!("{threads} threads"));
    }

    Some(parts.join(", "))
}

/// Format kB as human-readable (e.g. "RSS 4.2 GB").
fn format_bytes_kb(kb: i64, label: &str) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = kb as f64 / 1024.0;
    if mb >= 1024.0 {
        format!("{label} {:.1} GB", mb / 1024.0)
    } else {
        format!("{label} {mb:.0} MB")
    }
}

/// Format bytes as human-readable (e.g. "847 MB read").
fn format_bytes(bytes: i64, label: &str) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB {label}", mb / 1024.0)
    } else {
        format!("{mb:.0} MB {label}")
    }
}
