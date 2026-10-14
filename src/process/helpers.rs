//! Shared best-effort signalling and process snapshots.
//!
//! These helpers are for brokkr-owned children and diagnostics. Callers that
//! target recorded identities use the verified pidfd APIs in `reap`.
//!
//! - [`signal_pid`] - send a signal to a known PID.
//! - [`signal_process_group`] - send a signal to a known PGID.
//! - [`snapshot_proc`] - copy `/proc/<pid>/{status,wchan,syscall,stack}`
//!   into the artefact dir. Distinguishes "blocked on futex" from
//!   "blocked on closed pipe" without re-running.

use std::fs;
use std::path::Path;
#[cfg(test)]
use std::time::{Duration, Instant};

use crate::error::DevError;

/// Signal a PID that the caller already owns or has independently verified.
///
/// Returns `Ok(false)` when the process has already exited. Callers targeting
/// a stored or otherwise untrusted identity must use
/// [`crate::process::VerifiedProcess`]
/// instead, so PID recycling cannot redirect the signal.
pub fn signal_pid(pid: u32, signal: libc::c_int) -> std::io::Result<bool> {
    let pid = checked_pid(pid)?;
    // SAFETY: libc::kill takes scalar arguments and no pointers.
    let ret = unsafe { libc::kill(pid, signal) };
    signal_result(ret)
}

/// Signal a process group by PGID. `Ok(false)` means the group has exited.
///
/// This is for groups whose membership and lifetime the caller controls. A
/// process group has no pidfd-equivalent handle, so callers that target a
/// recorded group leader must verify its identity immediately before calling.
pub fn signal_process_group(pgid: u32, signal: libc::c_int) -> std::io::Result<bool> {
    let pgid = checked_pid(pgid)?;
    let ret = signal_process_group_raw(pgid, signal);
    signal_result(ret)
}

/// The syscall-only process-group path used by the shutdown signal handler.
/// It performs no allocation or error formatting, so it is safe to call from
/// that handler. `pgid` must be positive and fit in `pid_t`.
pub(crate) fn signal_process_group_raw(pgid: libc::pid_t, signal: libc::c_int) -> libc::c_int {
    // SAFETY: caller supplies a positive PGID; libc::kill is async-signal-safe.
    unsafe { libc::kill(-pgid, signal) }
}

fn checked_pid(pid: u32) -> std::io::Result<libc::pid_t> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "PID does not fit pid_t")
    })?;
    if pid <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "PID must be positive",
        ));
    }
    Ok(pid)
}

fn signal_result(ret: libc::c_int) -> std::io::Result<bool> {
    if ret == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(err)
}

/// Whether `pid` refers to a live process.
///
/// Implemented as `kill(pid, 0)`:
/// - `Ok` -> process exists and we can signal it -> alive
/// - `EPERM` -> process exists but we can't signal it -> still alive
/// - `ESRCH` -> no such process -> dead
///
/// A zombie process counts as alive here (`kill(pid, 0)` succeeds until
/// the parent reaps it). That is the correct answer for most tests:
/// "still occupying a PID slot, has not been waited on yet."
#[cfg(test)]
fn pid_is_alive(pid: u32) -> bool {
    signal_pid(pid, 0).unwrap_or(true)
}

/// Outcome of [`wait_for_sentinel`].
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SentinelOutcome {
    /// File appeared within the backstop window.
    Appeared,
    /// Backstop expired before the file appeared.
    BackstopExpired,
}

/// Block until `path` exists or `backstop` elapses.
///
/// The harness is structurally race-free: every wait is "predicate, OR
/// backstop." This helper is the simplest form of that pattern - it
/// returns [`SentinelOutcome::Appeared`] the moment the file appears,
/// or [`SentinelOutcome::BackstopExpired`] when the wall clock catches
/// the named ceiling. Both outcomes are first-class returns; the caller
/// asserts on whichever they expected.
///
/// Polls the shared 50 ms harness cadence - low enough to be tight on
/// wall-clock budgets, high enough that 200-test soak runs do not melt
/// the inotify-less polling implementation. inotify support could be
/// layered later if a class of tests benefits from sub-50ms latency.
#[cfg(test)]
fn wait_for_sentinel(path: &Path, backstop: Duration) -> Result<SentinelOutcome, DevError> {
    let deadline = Instant::now() + backstop;
    loop {
        if path.exists() {
            return Ok(SentinelOutcome::Appeared);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(SentinelOutcome::BackstopExpired);
        }
        let remaining = deadline - now;
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
}

/// `/proc/<pid>` files copied by [`snapshot_proc`].
///
/// `stack` is included in the list but reading it requires
/// `CAP_SYS_PTRACE` on most modern kernels (`/proc/sys/kernel/yama/
/// ptrace_scope >= 1` blocks it for unprivileged callers); the snapshot
/// continues past read failures rather than aborting, with the failure
/// reason recorded in `proc-stack.txt`.
const PROC_FILES: &[&str] = &["status", "wchan", "syscall", "stack"];

/// Capture `/proc/<pid>/{status,wchan,syscall,stack}` into `out_dir`.
///
/// One file per probe, named `proc-<probe>.txt`. Failures (process
/// already gone, permission denied) are recorded in the file body
/// rather than propagated, because:
///
/// - This helper runs at *failure-declaration* time, when the harness
///   is already on a sad path. Aborting the artefact dump because one
///   `/proc` file is missing would lose the others.
/// - The user reading the artefact dir later wants to see "stack:
///   permission denied" recorded next to "wchan: ep_poll" so they
///   know the missing data is a kernel-policy issue, not a bug in the
///   harness.
///
/// The only error this function returns is from [`fs::write`] failing
/// to create the output file (e.g. `out_dir` not writable) - that is a
/// harness configuration bug worth surfacing.
pub fn snapshot_proc(pid: u32, out_dir: &Path) -> Result<(), DevError> {
    for probe in PROC_FILES {
        let src = Path::new("/proc").join(pid.to_string()).join(probe);
        let body = match fs::read(&src) {
            Ok(bytes) => bytes,
            Err(err) => {
                format!("[brokkr] could not read {}: {}\n", src.display(), err).into_bytes()
            }
        };
        let dst = out_dir.join(format!("proc-{probe}.txt"));
        fs::write(&dst, body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use super::*;

    /// A fresh scratch dir for one test. `test_name` must be unique within
    /// this module - the allocator asserts it.
    fn tmpdir(test_name: &str) -> PathBuf {
        crate::test_scratch::scratch("process", test_name)
    }

    /// Spawn `sleep <seconds>` and return its PID. Caller is
    /// responsible for cleanup (kill or wait).
    fn spawn_sleep(seconds: u64) -> std::process::Child {
        Command::new("sleep")
            .arg(seconds.to_string())
            .spawn()
            .unwrap()
    }

    #[test]
    fn pid_is_alive_true_for_running_process() {
        let mut child = spawn_sleep(30);
        assert!(pid_is_alive(child.id()));
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn pid_is_alive_false_for_unused_pid() {
        // PID 0x7FFF_FFFE - well outside any reasonable kernel.pid_max.
        // Using a large but-still-i32 value avoids the cast_signed
        // edge case while being effectively guaranteed unused.
        assert!(!pid_is_alive(2_147_483_646));
    }

    #[test]
    fn send_signal_kills_process() {
        let mut child = spawn_sleep(60);
        let pid = child.id();
        assert!(signal_pid(pid, libc::SIGKILL).unwrap());
        // Wait briefly for the kernel to reap.
        child.wait().ok();
        // After wait(), the PID is reaped; pid_is_alive should be false
        // (kernel does not reuse PIDs immediately on a quiet system).
        assert!(!pid_is_alive(pid));
    }

    #[test]
    fn signal_pid_reports_dead_pid() {
        let result = signal_pid(2_147_483_646, libc::SIGTERM).unwrap();
        assert!(!result);
    }

    #[test]
    fn wait_for_sentinel_returns_appeared_when_file_exists() {
        let dir = tmpdir("sentinel_existing");
        let path = dir.join("ready");
        fs::write(&path, "").unwrap();
        let outcome = wait_for_sentinel(&path, Duration::from_millis(200)).unwrap();
        assert_eq!(outcome, SentinelOutcome::Appeared);
    }

    #[test]
    fn wait_for_sentinel_returns_appeared_when_file_appears() {
        let dir = tmpdir("sentinel_appears");
        let path = dir.join("ready");

        let path_in_thread = path.clone();
        let appeared_count = std::sync::Arc::new(AtomicUsize::new(0));
        let appeared_count_in_thread = std::sync::Arc::clone(&appeared_count);
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            fs::write(&path_in_thread, "").unwrap();
            appeared_count_in_thread.fetch_add(1, Ordering::SeqCst);
        });

        let outcome = wait_for_sentinel(&path, Duration::from_secs(2)).unwrap();
        writer.join().unwrap();
        assert_eq!(outcome, SentinelOutcome::Appeared);
        assert_eq!(appeared_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn wait_for_sentinel_returns_backstop_when_missing() {
        let dir = tmpdir("sentinel_backstop");
        let path = dir.join("never");
        let start = Instant::now();
        let outcome = wait_for_sentinel(&path, Duration::from_millis(150)).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(outcome, SentinelOutcome::BackstopExpired);
        assert!(elapsed >= Duration::from_millis(150));
        // Generous upper bound to avoid flakes on a busy CI box.
        assert!(elapsed < Duration::from_secs(2), "elapsed = {elapsed:?}");
    }

    #[test]
    fn snapshot_proc_writes_files_for_self() {
        let dir = tmpdir("snapshot_self");
        let pid = std::process::id();
        snapshot_proc(pid, &dir).unwrap();
        for probe in PROC_FILES {
            let path = dir.join(format!("proc-{probe}.txt"));
            assert!(path.exists(), "{path:?} should have been created");
            let body = fs::read(&path).unwrap();
            assert!(!body.is_empty(), "{path:?} should not be empty");
        }
    }

    #[test]
    fn snapshot_proc_records_missing_pid_inline() {
        let dir = tmpdir("snapshot_missing");
        let pid = 2_147_483_646u32;
        snapshot_proc(pid, &dir).unwrap();
        let body = fs::read_to_string(dir.join("proc-status.txt")).unwrap();
        assert!(
            body.contains("could not read"),
            "expected error stub in {body:?}"
        );
    }
}
