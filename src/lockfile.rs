use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

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

/// Acquire an exclusive non-blocking lock on `{dir}/.brokkr.lock`.
///
/// On success, writes the current PID to the lock file.
/// On `EWOULDBLOCK`, reads the file to report which PID holds the lock.
pub fn acquire(dir: &Path) -> Result<LockGuard, DevError> {
    let lock_path = dir.join(".brokkr.lock");
    let c_path = path_to_cstring(&lock_path)?;
    let fd = open_lock_file(&c_path)?;

    match try_flock(fd) {
        Ok(()) => {
            // SAFETY: `fd` is a valid open file descriptor returned by `open_lock_file`,
            // and we take unique ownership here — it is not used elsewhere.
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            write_pid(owned.as_raw_fd());
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
        let pid = read_holder_pid(fd);
        let cmd = read_holder_command(&pid);
        if cmd.is_empty() {
            Err(DevError::Lock(format!("already locked by PID {pid}")))
        } else {
            Err(DevError::Lock(format!(
                "already locked by PID {pid}: {cmd}"
            )))
        }
    } else {
        Err(DevError::Lock(format!("flock failed: {err}")))
    }
}

/// Read the existing file contents to discover the PID of the current holder.
fn read_holder_pid(fd: RawFd) -> String {
    let mut buf = [0u8; 32];

    // Seek to start before reading.
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return "unknown".to_owned();
    }

    // n is positive here, so the cast is safe.
    let len: usize = match usize::try_from(n) {
        Ok(v) => v,
        Err(_) => return "unknown".to_owned(),
    };

    let s = String::from_utf8_lossy(&buf[..len]);
    let trimmed = s.trim();
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Read the command line of a process from `/proc/{pid}/cmdline`.
fn read_holder_command(pid: &str) -> String {
    let path = format!("/proc/{pid}/cmdline");
    match std::fs::read(&path) {
        Ok(bytes) => {
            // cmdline is nul-separated; join args with spaces.
            bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s))
                .collect::<Vec<_>>()
                .join(" ")
        }
        Err(_) => String::new(),
    }
}

/// Truncate the lock file and write the current PID.
///
/// Best-effort for diagnostics — if any step fails we simply return early.
fn write_pid(fd: RawFd) {
    let pid = std::process::id().to_string();

    unsafe {
        if libc::ftruncate(fd, 0) == -1 {
            return;
        }
        if libc::lseek(fd, 0, libc::SEEK_SET) == -1 {
            return;
        }
        if libc::write(fd, pid.as_ptr().cast(), pid.len()) == -1 {
            return;
        }
    }
}

/// Convert a `Path` to a `CString`.
fn path_to_cstring(path: &Path) -> Result<std::ffi::CString, DevError> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        DevError::Lock(format!(
            "lock path contains nul byte: {}",
            path.display()
        ))
    })
}
