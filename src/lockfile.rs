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
    let home = std::env::var("HOME").map_err(|_| {
        DevError::Lock("neither XDG_RUNTIME_DIR nor HOME is set".into())
    })?;
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

/// Check whether a PID is still running.
fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0) checks existence without sending a signal.
    unsafe { libc::kill(pid.cast_signed(), 0) == 0 }
}

/// Open (or create) the lock file, returning the raw fd.
fn open_lock_file(c_path: &std::ffi::CString) -> Result<RawFd, DevError> {
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_CREAT | libc::O_RDWR,
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
            Some(info) => Err(DevError::Lock(format!(
                "already locked by PID {} — {} {} ({})",
                info.pid, info.project, info.command, info.project_root
            ))),
            None => Err(DevError::Lock("already locked by unknown process".into())),
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
            return;
        }
        if libc::lseek(fd, 0, libc::SEEK_SET) == -1 {
            return;
        }
        // Best-effort write; ignore failure.
        let _ = libc::write(fd, contents.as_ptr().cast(), contents.len());
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

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        DevError::Lock(format!(
            "lock path contains nul byte: {}",
            path.display()
        ))
    })
}
