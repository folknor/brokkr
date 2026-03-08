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

/// Print an error message. Multi-line messages get each line prefixed.
/// Errors are NEVER suppressed by quiet mode.
pub fn error(msg: &str) {
    for line in msg.lines() {
        println!("[error]   {line}");
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
        if self.status.success() {
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
pub fn run_captured(
    program: &str,
    args: &[&str],
    cwd: &Path,
) -> Result<CapturedOutput, DevError> {
    let start = Instant::now();

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::oom::protect_child(&mut cmd);

    let output = cmd.output().map_err(|e| DevError::Subprocess {
        program: program.to_owned(),
        code: None,
        stderr: e.to_string(),
    })?;

    let elapsed = start.elapsed();

    Ok(CapturedOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
        elapsed,
    })
}

/// Run a subprocess with extra environment variables, capturing stdout and stderr.
///
/// Same as `run_captured` but injects additional environment variables into the
/// subprocess. Variables are added on top of the inherited environment.
pub fn run_captured_with_env(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<CapturedOutput, DevError> {
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

    let output = cmd.output().map_err(|e| DevError::Subprocess {
        program: program.to_owned(),
        code: None,
        stderr: e.to_string(),
    })?;

    let elapsed = start.elapsed();

    Ok(CapturedOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
        elapsed,
    })
}

/// Run a subprocess with inherited stdio (passthrough mode), returning timing.
///
/// If the process is killed by a signal (e.g. OOM killer SIGKILL), returns a
/// `DevError::Subprocess` with the signal number instead of silently mapping
/// to exit code 1.
pub fn run_passthrough_timed(
    program: &str,
    args: &[&str],
) -> Result<PassthroughOutput, DevError> {
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
                9 => " (SIGKILL — possible OOM kill)",
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
