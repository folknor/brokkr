//! Server lifecycle management for nidhogg.
//!
//! Start, stop, and check the status of the nidhogg serve process.
//! Replaces `serve.sh`, `stop.sh`, `status.sh`, and `serve-tiles.sh`.
//!
//! `stop` signals only the process `serve` recorded, and only after proving
//! it is still that process: the pid file carries the PID's `/proc`
//! starttime and the boot id, the same identity token `crate::process` and
//! `brokkr kill` verify, and the signal goes through a pidfd opened between two
//! verifications, so PID recycling cannot redirect it. There is no
//! name-based fallback (the old `pkill -f "nidhogg serve"` reached every
//! matching process on the host, other checkouts' servers included).

use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::DevError;
use crate::output;
use crate::process::{self, BootId, Pid, ProcIdentity, StartTicks, VerifiedProcess};

/// Default port for the nidhogg server.
pub const DEFAULT_PORT: u16 = 3033;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Start the nidhogg server as a background process.
///
/// Stops the server brokkr previously recorded, refuses if something else
/// still answers on the port, spawns the binary with stdout/stderr
/// redirected to `logs/serve.log`, records its identity in
/// `.brokkr/nidhogg.pid`, and polls the HTTP health endpoint until ready
/// (6s timeout).
pub fn serve(
    binary: &Path,
    data_dir: Option<&str>,
    tiles: Option<&str>,
    port: u16,
    project_root: &Path,
) -> Result<(), DevError> {
    // Stop the server we recorded, if any.
    stop(project_root)?;
    let state = crate::clean::layout::ProjectState::new(project_root);
    let pid_path = state.nidhogg_pid_file();

    // Anything still answering is a server brokkr has no identity record
    // for. Without this check the health poll below would be satisfied by
    // *that* server and report a start that never happened.
    if status(port)? {
        return Err(DevError::Config(format!(
            "a server is already answering on port {port} and it is not one \
             brokkr recorded in {}; stop it by hand or set \
             a different [<host>] port in brokkr.toml",
            pid_path.display()
        )));
    }

    // Ensure logs/ and .brokkr/ directories exist.
    let logs_dir = project_root.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    std::fs::create_dir_all(state.root())?;

    let log_path = logs_dir.join("serve.log");

    // Open log file for stdout and stderr.
    let log_file = std::fs::File::create(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    // Build argument list.
    let mut args = vec!["serve"];
    if let Some(d) = data_dir {
        args.push("--data-dir");
        args.push(d);
    }
    if let Some(t) = tiles {
        args.push("--tiles");
        args.push(t);
    }

    let port_str = port.to_string();

    // Spawn background process.
    let mut cmd = Command::new(binary);
    cmd.args(&args)
        .env("PORT", &port_str)
        .current_dir(project_root)
        .stdout(log_file)
        .stderr(log_file_err)
        .stdin(Stdio::null());
    // Every child of a hold carries its capability: see `crate::hold`.
    crate::hold::stamp(&mut cmd);
    let mut child = cmd.spawn().map_err(|error| DevError::Spawn {
        program: binary.display().to_string(),
        error,
    })?;

    let pid = child.id();

    // Capture the identity while the child is ours and unreaped, so the
    // starttime is guaranteed to be this process's. A record we cannot
    // write is a server `stop` could never verify - don't leave it running.
    let record = match (process::proc_starttime(pid), process::local_boot_id()) {
        (Some(starttime), Some(boot_id)) => ServerRecord {
            pid,
            starttime: starttime.get().to_string(),
            boot_id: boot_id.as_str().to_owned(),
        },
        _ => {
            child.kill().ok();
            child.wait().ok();
            return Err(DevError::Config(format!(
                "could not read the identity of the spawned server (PID {pid}) \
                 from /proc; refusing to leave an unverifiable server running"
            )));
        }
    };
    if let Err(e) = std::fs::write(&pid_path, record.render()) {
        child.kill().ok();
        child.wait().ok();
        return Err(e.into());
    }

    // Poll HTTP health endpoint until the server is ready.
    if !poll_for_ready(port, &mut child)? {
        return Err(DevError::Config(format!(
            "server did not start within {}s (check {})",
            super::policy::SERVER_READY_TIMEOUT.as_secs(),
            log_path.display()
        )));
    }

    output::run_msg(&format!("nidhogg server started (PID {pid}, port {port})"));
    Ok(())
}

/// Stop the nidhogg server recorded in `.brokkr/nidhogg.pid`.
///
/// Verifies the recorded identity, sends SIGTERM through a pidfd, waits up
/// to 5s for the process to exit, then escalates to SIGKILL. A pid file
/// that carries no identity (the pre-starttime format) or whose identity no
/// longer matches is dropped without signalling anything.
pub fn stop(project_root: &Path) -> Result<(), DevError> {
    let pid_path = crate::clean::layout::ProjectState::new(project_root).nidhogg_pid_file();

    let content = match std::fs::read_to_string(&pid_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let result = match ServerRecord::parse(&content) {
        None => {
            output::run_msg(&format!(
                "{} has no identity record (starttime/boot id); not signalling \
                 anything - stop a leftover server by hand",
                pid_path.display()
            ));
            Ok(())
        }
        Some(record) => match stop_verified(&record)? {
            StopOutcome::Stopped => {
                output::run_msg("nidhogg server stopped");
                Ok(())
            }
            StopOutcome::NotRunning => Ok(()),
            StopOutcome::Unverified => {
                output::run_msg(&format!(
                    "recorded server PID {} could not be verified (exited, PID \
                     recycled, or not visible from this namespace); not \
                     signalling it",
                    record.pid
                ));
                Ok(())
            }
        },
    };

    std::fs::remove_file(&pid_path).ok();
    result
}

/// Check if the server is responding to API requests.
///
/// Returns `true` if a health-check query succeeds, `false` if nothing
/// healthy answers. Errors when the check itself could not run (curl
/// missing), which is not the same thing as "server not running".
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

/// Poll the HTTP health endpoint for up to
/// [`SERVER_READY_TIMEOUT`](super::policy::SERVER_READY_TIMEOUT). Returns
/// `Ok(false)` on timeout; errors if the server process exits first (its
/// log names why) or the health check cannot run at all.
fn poll_for_ready(port: u16, child: &mut Child) -> Result<bool, DevError> {
    let deadline = Instant::now() + super::policy::SERVER_READY_TIMEOUT;
    while Instant::now() < deadline {
        std::thread::sleep(super::policy::SERVER_READY_POLL_INTERVAL);
        if let Some(exit) = child.try_wait()? {
            return Err(DevError::Config(format!(
                "server exited during startup ({exit}); see logs/serve.log"
            )));
        }
        if status(port)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The identity of the server `serve` spawned, as stored in the pid file.
/// The fields stay text on the wire; [`ServerRecord::identity`] parses them
/// back into the typed identity `crate::process` verifies.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerRecord {
    pid: u32,
    starttime: String,
    boot_id: String,
}

impl ServerRecord {
    /// The typed identity and boot this record names; `None` when a field
    /// does not parse, which `stop` treats as unverifiable.
    fn identity(&self) -> Option<(ProcIdentity, BootId)> {
        let pid = Pid::new(self.pid)?;
        let start = StartTicks::parse(&self.starttime)?;
        let boot = BootId::new(&self.boot_id)?;
        Some((ProcIdentity::new(pid, start), boot))
    }

    fn render(&self) -> String {
        format!(
            "pid={}\nstarttime={}\nboot_id={}\n",
            self.pid, self.starttime, self.boot_id
        )
    }

    /// Parse a pid file. `None` for anything without all three keys - in
    /// particular the older bare-PID format, which carries no identity and
    /// must never be signalled on. First occurrence of a key wins.
    fn parse(text: &str) -> Option<Self> {
        let mut pid: Option<&str> = None;
        let mut starttime: Option<&str> = None;
        let mut boot_id: Option<&str> = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let slot = match key.trim() {
                "pid" => &mut pid,
                "starttime" => &mut starttime,
                "boot_id" => &mut boot_id,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Some(value.trim());
            }
        }
        let pid: u32 = pid?.parse().ok()?;
        let starttime = starttime?;
        let boot_id = boot_id?;
        if pid == 0 || starttime.is_empty() || boot_id.is_empty() {
            return None;
        }
        Some(Self {
            pid,
            starttime: starttime.to_owned(),
            boot_id: boot_id.to_owned(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StopOutcome {
    /// The verified process was signalled and has exited.
    Stopped,
    /// Identity verified, but the process exited before it could be signalled.
    NotRunning,
    /// The recorded identity does not describe any process visible here.
    Unverified,
}

/// Terminate the recorded process, never anything else.
///
/// verify -> `pidfd_open` -> re-verify, the order `brokkr kill` uses: the
/// numeric PID still described the recorded process generation when the
/// pidfd was opened, and from then on the pidfd cannot be redirected by
/// PID recycling. Exit is observed by polling the pidfd, which becomes
/// readable when the process terminates (a zombie counts, so a server that
/// is our own unreaped child - `verify readonly` - is seen as exited too).
fn stop_verified(record: &ServerRecord) -> Result<StopOutcome, DevError> {
    let Some((identity, boot)) = record.identity() else {
        return Ok(StopOutcome::Unverified);
    };
    let Ok(verified) = VerifiedProcess::open(identity, Some(&boot)) else {
        return Ok(StopOutcome::Unverified);
    };
    let pidfd = verified.into_pidfd();

    if !pidfd_send_signal(&pidfd, libc::SIGTERM)? {
        return Ok(StopOutcome::NotRunning);
    }
    if wait_for_exit(&pidfd, super::policy::SERVER_TERM_GRACE) {
        return Ok(StopOutcome::Stopped);
    }

    output::run_msg(&format!(
        "PID {} did not exit after SIGTERM, sending SIGKILL",
        record.pid
    ));
    if !pidfd_send_signal(&pidfd, libc::SIGKILL)? {
        return Ok(StopOutcome::Stopped);
    }
    if wait_for_exit(&pidfd, super::policy::SERVER_KILL_GRACE) {
        return Ok(StopOutcome::Stopped);
    }
    Err(DevError::Config(format!(
        "nidhogg server PID {} survived SIGKILL for {}s",
        record.pid,
        super::policy::SERVER_KILL_GRACE.as_secs()
    )))
}

/// `pidfd_send_signal(2)`: `Ok(false)` when the process has already exited
/// (ESRCH); any other failure is an error, not a guess.
fn pidfd_send_signal(pidfd: &OwnedFd, signal: libc::c_int) -> Result<bool, DevError> {
    process::pidfd_send_signal(pidfd, signal).map_err(DevError::Io)
}

/// Wait until the pidfd reports process exit, up to `timeout`.
fn wait_for_exit(pidfd: &OwnedFd, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ms = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
        let mut pfd = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, count 1.
        let ret = unsafe { libc::poll(&raw mut pfd, 1, ms) };
        if ret > 0 {
            return true;
        }
        if ret == 0 {
            return false;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
        if remaining.is_zero() {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn record_round_trips() {
        let rec = ServerRecord {
            pid: 4242,
            starttime: "123456".into(),
            boot_id: "abc-def".into(),
        };
        assert_eq!(ServerRecord::parse(&rec.render()), Some(rec));
    }

    #[test]
    fn bare_pid_file_carries_no_identity() {
        // The pre-starttime format: must never be treated as signallable.
        assert_eq!(ServerRecord::parse("4242\n"), None);
        assert_eq!(ServerRecord::parse("pid=4242\n"), None);
        assert_eq!(ServerRecord::parse("pid=4242\nstarttime=1\n"), None);
    }

    #[test]
    fn empty_or_zero_fields_are_rejected() {
        assert_eq!(ServerRecord::parse("pid=0\nstarttime=1\nboot_id=x\n"), None);
        assert_eq!(ServerRecord::parse("pid=5\nstarttime=\nboot_id=x\n"), None);
    }

    #[test]
    fn first_occurrence_wins() {
        let rec = ServerRecord::parse("pid=5\nstarttime=1\nboot_id=x\npid=6\n").unwrap();
        assert_eq!(rec.pid, 5);
    }

    fn spawn_sleeper() -> Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep")
    }

    #[test]
    fn mismatched_starttime_is_never_signalled() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        let rec = ServerRecord {
            pid,
            starttime: "1".into(),
            boot_id: process::local_boot_id().unwrap().as_str().to_owned(),
        };
        assert_eq!(stop_verified(&rec).unwrap(), StopOutcome::Unverified);
        // Still alive: nothing was sent.
        assert!(child.try_wait().unwrap().is_none());
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn verified_process_is_stopped() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        let rec = ServerRecord {
            pid,
            starttime: process::proc_starttime(pid).unwrap().get().to_string(),
            boot_id: process::local_boot_id().unwrap().as_str().to_owned(),
        };
        assert_eq!(stop_verified(&rec).unwrap(), StopOutcome::Stopped);
        let status = child.wait().unwrap();
        assert!(!status.success());
    }
}
