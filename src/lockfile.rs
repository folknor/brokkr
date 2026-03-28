use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use crate::error::DevError;

/// RAII lock guard. Releases the flock on drop; `OwnedFd` closes the fd.
pub struct LockGuard {
    fd: OwnedFd,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // The flock is released automatically when the fd is closed, but
        // unlock explicitly for clarity. OwnedFd handles close.
        unsafe {
            libc::flock(self.fd.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Context written to the lock file so `brokkr lock` can explain who holds it.
pub struct LockContext<'a> {
    pub project: &'a str,
    pub command: &'a str,
    pub project_root: &'a str,
}

/// Info read back from the lock file.
pub struct LockInfo {
    pub pid: u32,
    pub project: String,
    pub command: String,
    pub project_root: String,
}

/// Resolve the global lock file path.
///
/// Uses `$XDG_RUNTIME_DIR/brokkr.lock` (typically `/run/user/$UID/brokkr.lock`).
/// Falls back to `$HOME/.cache/brokkr/brokkr.lock` if `XDG_RUNTIME_DIR` is unset.
fn lock_path() -> Result<PathBuf, DevError> {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir).join("brokkr.lock"));
    }

    // Fallback: ~/.cache/brokkr/
    let home = std::env::var("HOME")
        .map_err(|_| DevError::Lock("neither XDG_RUNTIME_DIR nor HOME is set".into()))?;
    let dir = PathBuf::from(home).join(".cache").join("brokkr");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("brokkr.lock"))
}

/// Acquire an exclusive non-blocking lock on the global lock file.
///
/// On success, writes PID + context to the lock file.
/// On `EWOULDBLOCK`, reads the file to report who holds the lock.
pub fn acquire(ctx: &LockContext<'_>) -> Result<LockGuard, DevError> {
    let path = lock_path()?;
    let c_path = path_to_cstring(&path)?;
    let fd = open_lock_file(&c_path)?;

    match try_flock(fd) {
        Ok(()) => {
            // SAFETY: `fd` is a valid open file descriptor returned by `open_lock_file`,
            // and we take unique ownership here — it is not used elsewhere.
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            write_lock_contents(owned.as_raw_fd(), ctx);
            Ok(LockGuard { fd: owned })
        }
        Err(held_by) => {
            // flock failed — close the fd before returning the error.
            // SAFETY: same as above — valid fd, unique ownership.
            let _close = unsafe { OwnedFd::from_raw_fd(fd) };
            Err(held_by)
        }
    }
}

/// Acquire an exclusive blocking lock on the global lock file.
///
/// If the lock is held, prints a waiting message and blocks until it is
/// released. On success, writes PID + context to the lock file.
pub fn acquire_blocking(ctx: &LockContext<'_>) -> Result<LockGuard, DevError> {
    let path = lock_path()?;
    let c_path = path_to_cstring(&path)?;
    let fd = open_lock_file(&c_path)?;

    // Try non-blocking first to print a message if waiting.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let info = read_lock_contents(fd);
            let desc = match &info {
                Some(i) => {
                    let uptime = process_uptime_str(i.pid)
                        .map(|u| format!(", running {u}"))
                        .unwrap_or_default();
                    format!(
                        "PID {} — {} {} ({}{})",
                        i.pid, i.project, i.command, i.project_root, uptime
                    )
                }
                None => "unknown process".into(),
            };
            crate::output::lock_msg(&format!("waiting for {desc}"));
            if let Some(ref i) = info {
                if let Some(summary) = process_summary(i.pid) {
                    crate::output::lock_msg(&summary);
                }
            }

            // Block until the lock is released. Retry on EINTR.
            loop {
                let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
                if ret == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                let _close = unsafe { OwnedFd::from_raw_fd(fd) };
                return Err(DevError::Lock(format!("blocking flock failed: {err}")));
            }
            crate::output::lock_msg("lock acquired");
        } else {
            let _close = unsafe { OwnedFd::from_raw_fd(fd) };
            return Err(DevError::Lock(format!("flock failed: {err}")));
        }
    }

    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    write_lock_contents(owned.as_raw_fd(), ctx);
    Ok(LockGuard { fd: owned })
}

/// Check the global lock status. Returns `None` if no lock is held.
///
/// If the lock file exists and a flock is held, reads the context.
/// If the PID in the file is dead, releases the stale lock and returns `None`.
pub fn status() -> Result<Option<LockInfo>, DevError> {
    let path = lock_path()?;

    if !path.exists() {
        return Ok(None);
    }

    let c_path = path_to_cstring(&path)?;
    let fd = open_lock_file(&c_path)?;

    // Try to acquire — if we succeed, no one holds it.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    if ret == 0 {
        // We got the lock → no one was holding it. Release and close.
        // SAFETY: valid fd from open_lock_file, unique ownership.
        let _close = unsafe { OwnedFd::from_raw_fd(fd) };
        return Ok(None);
    }

    // Someone holds it. Read the contents.
    let info = read_lock_contents(fd);
    // SAFETY: valid fd from open_lock_file, unique ownership.
    let _close = unsafe { OwnedFd::from_raw_fd(fd) };

    let Some(info) = info else {
        // Could not parse — report as unknown holder.
        return Ok(Some(LockInfo {
            pid: 0,
            project: "unknown".into(),
            command: "unknown".into(),
            project_root: "unknown".into(),
        }));
    };

    // Check if the PID is still alive.
    if info.pid > 0 && !pid_alive(info.pid) {
        // Stale lock — the holder crashed. Remove the file so the next
        // flock attempt can succeed (the dead process's flock is already
        // released by the kernel, but removing the file is cleaner).
        std::fs::remove_file(&path).ok();
        return Ok(None);
    }

    Ok(Some(info))
}

/// Get how long a process has been running, as a human-readable string.
///
/// Reads `/proc/{pid}/stat` starttime and compares against system uptime.
fn process_uptime_str(pid: u32) -> Option<String> {
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    if clk_tck <= 0.0 {
        return None;
    }

    // System uptime in seconds.
    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;

    // Process start time in clock ticks since boot.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let comm_end = stat.rfind(')')?;
    let fields: Vec<&str> = stat[comm_end + 2..].split_whitespace().collect();
    // Field 19 after comm (index 19 in the post-comm fields) is starttime.
    let starttime: f64 = fields.get(19)?.parse().ok()?;

    let start_secs = starttime / clk_tck;
    let elapsed_secs = uptime_secs - start_secs;

    if elapsed_secs < 0.0 {
        return None;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let elapsed = elapsed_secs as u64;
    let hours = elapsed / 3600;
    let minutes = (elapsed % 3600) / 60;

    Some(if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{elapsed}s")
    })
}

/// Build a one-line summary of a running process from `/proc`.
///
/// Returns something like `"running 12m, RSS 4.2 GB, 847 MB read, 4 threads"`.
/// Returns `None` if the process is gone or `/proc` is unreadable.
pub fn process_summary(pid: u32) -> Option<String> {
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

/// Check whether a PID is still running.
fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0) checks existence without sending a signal.
    // Returns 0 if signalable, or -1 with errno:
    //   EPERM  = process exists but we can't signal it → alive
    //   ESRCH  = no such process → dead
    let ret = unsafe { libc::kill(pid.cast_signed(), 0) };
    if ret == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    err.raw_os_error() != Some(libc::ESRCH)
}

/// Open (or create) the lock file, returning the raw fd.
fn open_lock_file(c_path: &std::ffi::CString) -> Result<RawFd, DevError> {
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC,
            0o644,
        )
    };

    if fd < 0 {
        return Err(DevError::Lock(format!(
            "failed to open lock file: {}",
            std::io::Error::last_os_error()
        )));
    }

    Ok(fd)
}

/// Try a non-blocking exclusive flock. Returns `Ok(())` on success, or a
/// `DevError::Lock` describing the holder on `EWOULDBLOCK`.
fn try_flock(fd: RawFd) -> Result<(), DevError> {
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };

    if ret == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        let info = read_lock_contents(fd);
        match info {
            Some(info) => {
                let uptime = process_uptime_str(info.pid)
                    .map(|u| format!(", running {u}"))
                    .unwrap_or_default();
                Err(DevError::Lock(format!(
                    "already locked by PID {} — {} {} ({}{})\nuse --wait to queue behind the lock",
                    info.pid, info.project, info.command, info.project_root, uptime
                )))
            }
            None => Err(DevError::Lock(
                "already locked by unknown process\nuse --wait to queue behind the lock".into(),
            )),
        }
    } else {
        Err(DevError::Lock(format!("flock failed: {err}")))
    }
}

/// Write PID + context to the lock file as newline-separated fields:
///
/// ```text
/// pid=12345
/// project=pbfhogg
/// command=bench read
/// root=/home/user/Projects/pbfhogg
/// ```
fn write_lock_contents(fd: RawFd, ctx: &LockContext<'_>) {
    let contents = format!(
        "pid={}\nproject={}\ncommand={}\nroot={}\n",
        std::process::id(),
        ctx.project,
        ctx.command,
        ctx.project_root,
    );

    unsafe {
        if libc::ftruncate(fd, 0) == -1 {
            eprintln!(
                "[lock] warning: failed to truncate lock file: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        if libc::lseek(fd, 0, libc::SEEK_SET) == -1 {
            eprintln!(
                "[lock] warning: failed to seek lock file: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        let n = libc::write(fd, contents.as_ptr().cast(), contents.len());
        if n == -1 {
            eprintln!(
                "[lock] warning: failed to write lock metadata: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Read lock file contents and parse the key=value fields.
fn read_lock_contents(fd: RawFd) -> Option<LockInfo> {
    let mut buf = [0u8; 512];

    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return None;
    }

    let len = usize::try_from(n).ok()?;
    let text = std::str::from_utf8(&buf[..len]).ok()?;

    let mut pid: u32 = 0;
    let mut project = String::new();
    let mut command = String::new();
    let mut root = String::new();

    for line in text.lines() {
        if let Some(v) = line.strip_prefix("pid=") {
            pid = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("project=") {
            project = v.trim().to_owned();
        } else if let Some(v) = line.strip_prefix("command=") {
            command = v.trim().to_owned();
        } else if let Some(v) = line.strip_prefix("root=") {
            root = v.trim().to_owned();
        }
    }

    if pid == 0 && project.is_empty() {
        return None;
    }

    Some(LockInfo {
        pid,
        project,
        command,
        project_root: root,
    })
}

/// Convert a `Path` to a `CString`.
fn path_to_cstring(path: &std::path::Path) -> Result<std::ffi::CString, DevError> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| DevError::Lock(format!("lock path contains nul byte: {}", path.display())))
}
