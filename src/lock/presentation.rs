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
/// Display only - reached through [`verified_uptime_for`], which verifies
/// identity on both sides of the read. Process rows (CPU, RSS, IO) come from
/// the shared sampler in `collector.rs`, for the holder and the reader alike.
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
