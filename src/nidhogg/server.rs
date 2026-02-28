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
/// redirected to `logs/serve.log`, saves the PID to `.dev/nidhogg.pid`,
/// and polls the log file until "Listening" appears (6s timeout).
pub fn serve(
    binary: &Path,
    data_dir: &str,
    tiles: Option<&str>,
    port: u16,
    project_root: &Path,
) -> Result<(), DevError> {
    // Kill any existing server first.
    stop(project_root)?;

    // Ensure logs/ and .dev/ directories exist.
    let logs_dir = project_root.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let dev_dir = project_root.join(".dev");
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

    // Poll log file for "Listening" text.
    if !poll_for_listening(&log_path) {
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
/// Reads PID from `.dev/nidhogg.pid` and sends SIGTERM. Falls back to
/// pkill as a safety net. Sleeps 500ms for graceful shutdown.
pub fn stop(project_root: &Path) -> Result<(), DevError> {
    let pid_path = project_root.join(".dev").join("nidhogg.pid");

    let mut stopped = false;

    // Try PID file first.
    if pid_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = content.trim().parse::<i32>() {
                // SAFETY: sending SIGTERM to a process is safe; the worst case
                // is the PID no longer exists and we get ESRCH (ignored below).
                let ret = unsafe { libc::kill(pid, libc::SIGTERM) };
                if ret == 0 {
                    stopped = true;
                }
            }
        }
        let _ = std::fs::remove_file(&pid_path);
    }

    // Fallback: pkill any remaining nidhogg serve processes.
    let _ = Command::new("pkill")
        .args(["-f", "nidhogg serve"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if stopped {
        // Brief pause for graceful shutdown.
        std::thread::sleep(std::time::Duration::from_millis(500));
        output::run_msg("nidhogg server stopped");
    }

    Ok(())
}

/// Check if the server is responding to API requests.
///
/// Returns `true` if a health-check query succeeds, `false` otherwise.
pub fn status(port: u16) -> Result<bool, DevError> {
    let url = format!("http://localhost:{port}/api/query");
    let body = r#"{"bbox":[0,0,0,0],"query":[]}"#;

    let result = Command::new("curl")
        .args([
            "-s",
            "-o", "/dev/null",
            "-w", "%{http_code}",
            "-X", "POST",
            &url,
            "-H", "Content-Type: application/json",
            "-d", body,
            "--connect-timeout", "2",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match result {
        Ok(output) if output.status.success() => {
            let code = String::from_utf8_lossy(&output.stdout);
            Ok(code.trim() == "200")
        }
        _ => Ok(false),
    }
}

/// Check that the server is running and return an error if not.
pub fn check_running(port: u16) -> Result<(), DevError> {
    let running = status(port)?;
    if !running {
        return Err(DevError::Config(format!(
            "nidhogg server is not running on port {port}\n\
             Start it with: dev serve"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Poll the log file for "Listening" text, up to 30 attempts x 200ms = 6s.
fn poll_for_listening(log_path: &Path) -> bool {
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if let Ok(contents) = std::fs::read_to_string(log_path) {
            if contents.contains("Listening") {
                return true;
            }
        }
    }
    false
}
