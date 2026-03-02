//! Server lifecycle management for nidhogg.
//!
//! Start, stop, and check the status of the nidhogg serve process.
//! Replaces `serve.sh`, `stop.sh`, `status.sh`, and `serve-tiles.sh`.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::DevError;
use crate::output;

/// Default port for the nidhogg server.
pub const DEFAULT_PORT: u16 = 3033;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the nidhogg server as a background process.
///
/// Kills any existing server first, spawns the binary with stdout/stderr
/// redirected to `logs/serve.log`, saves the PID to `.brokkr/nidhogg.pid`,
/// and polls the HTTP health endpoint until ready (6s timeout).
pub fn serve(
    binary: &Path,
    data_dir: &str,
    tiles: Option<&str>,
    port: u16,
    project_root: &Path,
) -> Result<(), DevError> {
    // Kill any existing server first.
    stop(project_root)?;

    // Ensure logs/ and .brokkr/ directories exist.
    let logs_dir = project_root.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let dev_dir = project_root.join(".brokkr");
    std::fs::create_dir_all(&dev_dir)?;

    let log_path = logs_dir.join("serve.log");
    let pid_path = dev_dir.join("nidhogg.pid");

    // Open log file for stdout and stderr.
    let log_file = std::fs::File::create(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    // Build argument list.
    let mut args = vec!["serve", data_dir];
    if let Some(t) = tiles {
        args.push("--tiles");
        args.push(t);
    }

    let port_str = port.to_string();

    // Spawn background process.
    let child = Command::new(binary)
        .args(&args)
        .env("PORT", &port_str)
        .current_dir(project_root)
        .stdout(log_file)
        .stderr(log_file_err)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| DevError::Subprocess {
            program: binary.display().to_string(),
            code: None,
            stderr: e.to_string(),
        })?;

    let pid = child.id();

    // Save PID to file.
    std::fs::write(&pid_path, pid.to_string())?;

    // Poll HTTP health endpoint until the server is ready.
    if !poll_for_ready(port) {
        return Err(DevError::Config(format!(
            "server did not start within 6s (check {})",
            log_path.display()
        )));
    }

    output::run_msg(&format!("nidhogg server started (PID {pid}, port {port})"));
    Ok(())
}

/// Stop the nidhogg server.
///
/// Reads PID from `.brokkr/nidhogg.pid`, sends SIGTERM, waits up to 5s for
/// the process to exit, then escalates to SIGKILL if still alive.
pub fn stop(project_root: &Path) -> Result<(), DevError> {
    let pid_path = project_root.join(".brokkr").join("nidhogg.pid");

    let mut stopped = false;

    // Try PID file first.
    if pid_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_path)
            && let Ok(pid) = content.trim().parse::<i32>() {
                stopped = stop_pid(pid);
            }
        std::fs::remove_file(&pid_path).ok();
    }

    if !stopped {
        // Fallback: pkill any remaining nidhogg serve processes.
        Command::new("pkill")
            .args(["-f", "nidhogg serve"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    }

    if stopped {
        output::run_msg("nidhogg server stopped");
    }

    Ok(())
}

/// Send SIGTERM to a process, wait up to 5s for it to die, escalate to
/// SIGKILL if it's still alive. Returns `true` if the process was running.
fn stop_pid(pid: i32) -> bool {
    // SAFETY: sending signals to a process is safe; the worst case is the
    // PID no longer exists and we get ESRCH.
    let ret = unsafe { libc::kill(pid, libc::SIGTERM) };
    if ret != 0 {
        return false;
    }

    // Poll for up to 5s (25 x 200ms) to see if the process exited.
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let alive = unsafe { libc::kill(pid, 0) };
        if alive != 0 {
            return true;
        }
    }

    // Still alive after 5s — check if the PID was recycled before escalating.
    if !is_nidhogg_process(pid) {
        // PID was recycled to a different process; the original nidhogg exited.
        return true;
    }

    output::run_msg(&format!("PID {pid} did not exit after SIGTERM, sending SIGKILL"));
    unsafe { libc::kill(pid, libc::SIGKILL) };
    std::thread::sleep(std::time::Duration::from_millis(100));
    true
}

/// Check whether the given PID is still a nidhogg process by reading
/// `/proc/{pid}/cmdline`. Returns `false` if the PID doesn't exist or
/// belongs to a different program (i.e., was recycled).
fn is_nidhogg_process(pid: i32) -> bool {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    match std::fs::read(&cmdline_path) {
        Ok(bytes) => {
            // /proc/pid/cmdline uses NUL as argument separator.
            let cmdline = String::from_utf8_lossy(&bytes);
            cmdline.contains("nidhogg")
        }
        Err(_) => false,
    }
}

/// Check if the server is responding to API requests.
///
/// Returns `true` if a health-check query succeeds, `false` otherwise.
pub fn status(port: u16) -> Result<bool, DevError> {
    super::client::health_check(port)
}

/// Check that the server is running and return an error if not.
pub fn check_running(port: u16) -> Result<(), DevError> {
    let running = status(port)?;
    if !running {
        return Err(DevError::Config(format!(
            "nidhogg server is not running on port {port}\n\
             Start it with: brokkr serve"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Poll the HTTP health endpoint, up to 30 attempts x 200ms = 6s.
pub(crate) fn poll_for_ready(port: u16) -> bool {
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if let Ok(true) = status(port) {
            return true;
        }
    }
    false
}
