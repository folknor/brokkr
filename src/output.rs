use std::path::Path;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::time::Instant;

use crate::error::DevError;

// --- Quiet mode ---

static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

// --- Prefixed output ---
// All output goes to stdout (stderr reserved for panics only).
// Prefix column is 10 chars wide: "[tag]" + padding to align the message.
//
// Quiet mode split: `run_msg`, `verify_msg`, and `download_msg` always print
// because they represent user-facing actions that should be visible even in
// non-verbose mode. The others (`build_msg`, `bench_msg`, `result_msg`,
// `hotpath_msg`) are suppressed in quiet mode because they are internal
// progress messages. Errors are never suppressed.

pub fn build_msg(msg: &str) {
    if !is_quiet() {
        println!("[build]   {msg}");
    }
}

pub fn run_msg(msg: &str) {
    println!("[run]     {msg}");
}

pub fn result_msg(msg: &str) {
    if !is_quiet() {
        println!("[result]  {msg}");
    }
}

pub fn bench_msg(msg: &str) {
    if !is_quiet() {
        println!("[bench]   {msg}");
    }
}

pub fn verify_msg(msg: &str) {
    println!("[verify]  {msg}");
}

pub fn hotpath_msg(msg: &str) {
    if !is_quiet() {
        println!("[hotpath] {msg}");
    }
}

pub fn download_msg(msg: &str) {
    println!("[download] {msg}");
}

pub fn lock_msg(msg: &str) {
    println!("[lock]    {msg}");
}

#[allow(dead_code)]
pub fn history_msg(msg: &str) {
    println!("[history] {msg}");
}

pub fn sidecar_msg(msg: &str) {
    if !is_quiet() {
        // Always stderr - every [sidecar] line is narration (run provenance,
        // "attached to pid X", "showing run N/M"), never the data the caller
        // is asking for. Keeping them off stdout lets `brokkr sidecar …
        // --samples | jq` Just Work.
        eprintln!("[sidecar] {msg}");
    }
}

pub fn litehtml_msg(msg: &str) {
    println!("[litehtml] {msg}");
}

pub fn sluggrs_msg(msg: &str) {
    println!("[sluggrs] {msg}");
}

pub fn ratatoskr_msg(msg: &str) {
    println!("[ratatoskr] {msg}");
}

pub fn corpus_msg(msg: &str) {
    println!("[corpus]  {msg}");
}

pub fn harness_msg(msg: &str) {
    println!("[harness] {msg}");
}

pub fn deps_msg(msg: &str) {
    println!("[deps]    {msg}");
}

pub fn wc_msg(msg: &str) {
    println!("[wc]      {msg}");
}

/// Print an error message. Multi-line messages get each line prefixed.
/// Errors are NEVER suppressed by quiet mode.
pub fn error(msg: &str) {
    for line in msg.lines() {
        println!("[error]   {line}");
    }
}

/// Print a warning message. Multi-line messages get each line prefixed.
/// Warnings are NEVER suppressed by quiet mode.
pub fn warn(msg: &str) {
    for line in msg.lines() {
        println!("[warn]    {line}");
    }
}

// --- Subprocess types ---

/// Captured output from a subprocess.
pub struct CapturedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
}

/// Exit code and elapsed time from a passthrough subprocess.
pub struct PassthroughOutput {
    pub code: i32,
    pub elapsed: Duration,
}

impl CapturedOutput {
    /// Return `Ok(())` if the process exited successfully, or a `DevError::Subprocess`
    /// with the captured stderr if it failed.
    pub fn check_success(&self, program: &str) -> Result<(), DevError> {
        self.check_success_or(program, &[])
    }

    /// Like `check_success`, but also treats the given exit codes as success.
    /// For example, `diff` uses exit 1 to mean "differences found" (not an error).
    pub fn check_success_or(&self, program: &str, ok_codes: &[i32]) -> Result<(), DevError> {
        if self.status.success() {
            return Ok(());
        }
        if let Some(code) = self.status.code()
            && ok_codes.contains(&code)
        {
            return Ok(());
        }
        Err(DevError::Subprocess {
            program: program.to_owned(),
            code: self.status.code(),
            stderr: String::from_utf8_lossy(&self.stderr).into_owned(),
        })
    }
}

/// Run a subprocess, capturing stdout and stderr.
///
/// Returns `CapturedOutput` on success (even if the process exited non-zero).
/// Returns `DevError::Subprocess` only if the process could not be spawned.
pub fn run_captured(program: &str, args: &[&str], cwd: &Path) -> Result<CapturedOutput, DevError> {
    run_captured_with_env(program, args, cwd, &[])
}

/// As [`run_captured`], but invokes `on_spawn` with the child's PID
/// immediately after `Command::spawn` returns. Lets callers (notably
/// `cargo_build_observed`) publish the live PID into the lockfile so
/// `brokkr kill --hard` during a long cargo build can SIGKILL cargo too.
pub fn run_captured_observed(
    program: &str,
    args: &[&str],
    cwd: &Path,
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
) -> Result<CapturedOutput, DevError> {
    let dc = run_captured_with_env_and_deadline(
        program,
        args,
        cwd,
        &[],
        Duration::MAX,
        on_spawn,
        isolate_pg,
    )?;
    Ok(dc.captured)
}

/// Run a subprocess with extra environment variables, capturing stdout and stderr.
///
/// Variables are added on top of the inherited environment.
pub fn run_captured_with_env(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<CapturedOutput, DevError> {
    // Route through the deadline+observed runner so the shutdown-flag
    // poll covers cargo build / cargo metadata too. `Duration::MAX`
    // disables the deadline branch in practice (kernel pid lifetime is
    // measured in hours, this in eons).
    let dc = run_captured_with_env_and_deadline(
        program,
        args,
        cwd,
        env,
        Duration::MAX,
        None,
        false,
    )?;
    Ok(dc.captured)
}

/// Captured output plus a flag indicating whether the deadline fired.
///
/// Returned by [`run_captured_with_env_and_deadline`] only - the regular
/// captured-output paths cannot trigger a deadline kill, so they keep
/// their plain [`CapturedOutput`] return type.
pub struct DeadlineCapture {
    pub captured: CapturedOutput,
    /// `true` when the child was SIGKILL'd because `deadline` elapsed
    /// before it exited on its own. The captured `status` will reflect
    /// the SIGKILL (signal=9 on Linux); this flag is what callers should
    /// branch on to surface "ceiling exceeded" in user output.
    pub killed_on_deadline: bool,
}

/// How often to poll `Child::try_wait` while waiting for a deadline-bounded
/// run. Matches `ServiceClient::observe_child_exit`'s 50 ms cadence so the
/// brokkr-side and runtime-side loops have the same scheduling granularity.
const DEADLINE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Spawn a subprocess with captured stdio and a wall-clock deadline.
///
/// Drains stdout and stderr in background threads (otherwise a child
/// that prints more than the pipe buffer holds - ~64 KiB - would block
/// while we're polling for exit). Polls `Child::try_wait` at the
/// [`DEADLINE_POLL_INTERVAL`] cadence and SIGKILLs the child if `deadline`
/// elapses first.
///
/// The captured `status` reflects whatever the kernel actually reaped:
/// the child's own exit code/signal if it finished within the deadline,
/// or signal=9 if brokkr killed it. Callers should branch on
/// `killed_on_deadline` (not the status alone) to distinguish a child
/// that died on its own from one we killed.
/// `on_spawn` is invoked with the child's PID immediately after
/// `Command::spawn` returns. Callers can use it to publish the live PID
/// into the lockfile so concurrent `brokkr lock` invocations can see what
/// is currently running.
///
/// `isolate_pg` puts the child in its own process group via
/// `process_group(0)` and switches the deadline / cooperative-SIGTERM
/// kill paths to `kill(-pgid, ...)` so descendants (rustc, sæhrimnir
/// listeners, harness helpers, etc.) go down with the leader. Only set
/// to `true` when the caller has a `SigtermGuard` (or equivalent)
/// active for the lifetime of the spawn - otherwise terminal Ctrl-C
/// kills brokkr but leaves the PG-detached child orphaned.
pub fn run_captured_with_env_and_deadline(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
) -> Result<DeadlineCapture, DevError> {
    use std::io::Read;

    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::oom::protect_child(&mut cmd);
    // PG isolation is opt-in: the caller asserts a SigtermGuard (or
    // equivalent) is active so terminal Ctrl-C bridges to the PG via
    // the wait-loop's flag-poll. Without that bridge, isolating the
    // child would orphan it on ctrl-C; without a SigtermGuard tracking
    // children alone (PID published, no PG) keeps them in brokkr's PG
    // so ctrl-C reaches them naturally.
    use std::os::unix::process::CommandExt;
    if isolate_pg {
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| DevError::Subprocess {
        program: program.to_owned(),
        code: None,
        stderr: e.to_string(),
    })?;
    if let Some(cb) = on_spawn {
        cb(child.id());
    }

    fn drain(pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut reader = pipe;
            drop(reader.read_to_end(&mut buf));
            buf
        })
    }

    let stdout_thread = child.stdout.take().map(drain);
    let stderr_thread = child.stderr.take().map(drain);

    let mut killed_on_deadline = false;
    let mut interrupted = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if crate::shutdown::is_shutdown_requested() {
                    // `brokkr kill` (SIGTERM) reached us. Forward SIGTERM
                    // to the child, give it a brief budget to clean up,
                    // then SIGKILL. The Err propagates as Interrupted so
                    // the orchestrator can run its mock-teardown path
                    // before main's scratch-cleanup.
                    forward_sigterm_then_kill(&mut child, isolate_pg);
                    interrupted = true;
                    break child.wait().map_err(|e| DevError::Subprocess {
                        program: program.to_owned(),
                        code: None,
                        stderr: e.to_string(),
                    })?;
                }
                if start.elapsed() >= deadline {
                    // SIGKILL: PG-isolated children get a `kill(-pgid,
                    // ...)` sweep so descendants don't outlive the
                    // deadline; non-isolated children share brokkr's
                    // PG, so `child.kill()` (single-PID SIGKILL) is
                    // the right hammer - sending to -pid would also
                    // signal brokkr itself.
                    if isolate_pg {
                        crate::ratatoskr::process::send_signal_pgrp(child.id(), libc::SIGKILL).ok();
                    }
                    drop(child.kill());
                    killed_on_deadline = true;
                    break child.wait().map_err(|e| DevError::Subprocess {
                        program: program.to_owned(),
                        code: None,
                        stderr: e.to_string(),
                    })?;
                }
                std::thread::sleep(DEADLINE_POLL_INTERVAL);
            }
            Err(e) => {
                return Err(DevError::Subprocess {
                    program: program.to_owned(),
                    code: None,
                    stderr: e.to_string(),
                });
            }
        }
    };
    if interrupted {
        // Drain pipes so the threads exit cleanly before we return.
        drop(stdout_thread.and_then(|h| h.join().ok()));
        drop(stderr_thread.and_then(|h| h.join().ok()));
        return Err(DevError::Interrupted);
    }

    let stdout = stdout_thread
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_thread
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let elapsed = start.elapsed();

    Ok(DeadlineCapture {
        captured: CapturedOutput {
            status,
            stdout,
            stderr,
            elapsed,
        },
        killed_on_deadline,
    })
}

/// Forward SIGTERM to the child, give it [`SIGTERM_FORWARD_BUDGET`] to
/// honour it, then escalate to SIGKILL. Used by
/// [`run_captured_with_env_and_deadline`] when `brokkr kill` reaches us
/// mid-orchestration.
fn forward_sigterm_then_kill(child: &mut std::process::Child, isolate_pg: bool) {
    let pid = child.id();
    // PG-isolated children: SIGTERM the group so sæhrimnir / cargo /
    // rustc helpers get the cooperative shutdown too. Non-isolated
    // children share brokkr's PG (so the same `kill -<pgid>` would also
    // signal brokkr) - SIGTERM only the leader; if the user is doing
    // ctrl-C from the terminal, the child already received SIGINT via
    // the foreground PG anyway.
    if isolate_pg {
        crate::ratatoskr::process::send_signal_pgrp(pid, libc::SIGTERM).ok();
    } else {
        // SAFETY: sending SIGTERM to our own child by PID; ESRCH is
        // benign (handled by the wait loop below).
        unsafe { libc::kill(pid.cast_signed(), libc::SIGTERM) };
    }
    let term_sent = Instant::now();
    while term_sent.elapsed() < SIGTERM_FORWARD_BUDGET {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(DEADLINE_POLL_INTERVAL),
            Err(_) => break,
        }
    }
    // SIGKILL escalation: PG for isolated, single-PID for non-isolated.
    if isolate_pg {
        crate::ratatoskr::process::send_signal_pgrp(pid, libc::SIGKILL).ok();
    }
    drop(child.kill());
}

/// How long to give a captured child to honour SIGTERM after `brokkr kill`
/// before we escalate to SIGKILL. Matches sæhrimnir's
/// [`crate::ratatoskr::saehrimnir::SHUTDOWN_BUDGET`] in spirit: long enough
/// for cooperative cleanup, short enough that the user doesn't think
/// `brokkr kill` hung.
const SIGTERM_FORWARD_BUDGET: Duration = Duration::from_millis(1500);

/// Spawn a subprocess with captured stdio, returning the `Child` handle.
///
/// The caller is responsible for waiting on the child and collecting output.
/// Used by the sidecar to run sampling alongside the child process.
pub fn spawn_captured(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    isolate_pg: bool,
) -> Result<std::process::Child, DevError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::oom::protect_child(&mut cmd);
    // PG isolation is opt-in for the same reason as the deadline
    // runner: the caller asserts a SigtermGuard is active so terminal
    // signals bridge to the PG. The sidecar's own SigtermGuard (around
    // `run_sidecar`) is the typical pairing.
    use std::os::unix::process::CommandExt;
    if isolate_pg {
        cmd.process_group(0);
    }

    cmd.spawn().map_err(|e| DevError::Subprocess {
        program: program.to_owned(),
        code: None,
        stderr: e.to_string(),
    })
}

/// Run a subprocess with inherited stdio (passthrough mode), returning timing.
///
/// If the process is killed by a signal (e.g. OOM killer SIGKILL), returns a
/// `DevError::Subprocess` with the signal number instead of silently mapping
/// to exit code 1.
pub fn run_passthrough_timed(program: &str, args: &[&str]) -> Result<PassthroughOutput, DevError> {
    use std::os::unix::process::ExitStatusExt;

    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args);
    crate::oom::protect_child(&mut cmd);

    let status = cmd.status().map_err(|e| DevError::Subprocess {
        program: program.to_owned(),
        code: None,
        stderr: e.to_string(),
    })?;

    let elapsed = start.elapsed();

    match status.code() {
        Some(code) => Ok(PassthroughOutput { code, elapsed }),
        None => {
            let signal = status.signal().unwrap_or(0);
            let signal_name = match signal {
                9 => " (SIGKILL - possible OOM kill)",
                15 => " (SIGTERM)",
                11 => " (SIGSEGV)",
                _ => "",
            };
            Err(DevError::Subprocess {
                program: program.to_owned(),
                code: None,
                stderr: format!("killed by signal {signal}{signal_name}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::time::Duration;

    use super::*;

    fn cwd() -> &'static Path {
        Path::new(".")
    }

    #[test]
    fn deadline_lets_short_runs_finish_normally() {
        let result =
            run_captured_with_env_and_deadline("/bin/true", &[], cwd(), &[], Duration::from_secs(5), None, false)
                .unwrap();
        assert!(!result.killed_on_deadline);
        assert!(result.captured.status.success());
        assert_eq!(result.captured.status.code(), Some(0));
    }

    #[test]
    fn deadline_kills_runaway_child() {
        // /bin/sleep 30 will outlive a 250 ms deadline by orders of
        // magnitude; brokkr should reap it quickly.
        let start = std::time::Instant::now();
        let result = run_captured_with_env_and_deadline(
            "/bin/sleep",
            &["30"],
            cwd(),
            &[],
            Duration::from_millis(250),
            None,
            false,
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(result.killed_on_deadline);
        // SIGKILL on Linux is signal 9; status.code() is None for
        // signal-killed children.
        assert_eq!(result.captured.status.signal(), Some(9));
        assert!(result.captured.status.code().is_none());
        // Should finish well inside one poll interval after the deadline,
        // plus a healthy slack budget for slow CI hardware.
        assert!(
            elapsed < Duration::from_secs(5),
            "deadline kill took too long: {elapsed:?}"
        );
    }

    #[test]
    fn deadline_captures_stdout_from_short_run() {
        let result = run_captured_with_env_and_deadline(
            "/bin/echo",
            &["hello", "world"],
            cwd(),
            &[],
            Duration::from_secs(5),
            None,
            false,
        )
        .unwrap();
        assert!(!result.killed_on_deadline);
        assert_eq!(result.captured.stdout, b"hello world\n");
    }
}
