use std::process::Command;

/// Set the child process as the preferred OOM kill target.
///
/// Writes `1000` to `/proc/self/oom_score_adj` in the child process before
/// exec, so the Linux OOM killer targets the benchmark rather than the desktop.
/// Uses raw libc syscalls (async-signal-safe between fork and exec).
/// Silently ignores errors - works in containers, degrades on non-Linux.
pub fn protect_child(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: Between fork and exec only async-signal-safe functions are allowed.
    // libc::open, libc::write, libc::close are all async-signal-safe per POSIX.
    unsafe {
        cmd.pre_exec(|| {
            let path = b"/proc/self/oom_score_adj\0";
            let fd = libc::open(path.as_ptr().cast::<libc::c_char>(), libc::O_WRONLY);
            if fd >= 0 {
                let value = b"1000";
                libc::write(fd, value.as_ptr().cast::<libc::c_void>(), value.len());
                libc::close(fd);
            }
            Ok(())
        });
    }
}
