use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use crate::error::DevError;

/// Mutable lock-file state - maintained while brokkr holds the lock so
/// `brokkr lock` (from another invocation) can see the current child PID
/// and bench-run progress.
struct LockState {
    project: String,
    command: String,
    /// Full brokkr invocation minus argv[0] (e.g. `add-locations-to-ways
    /// --dataset europe --bench 3`). Captured at acquire time.
    args: String,
    project_root: String,
    /// PID of the most recent child process brokkr spawned under the lock.
    /// Updated by the harness each iteration of a bench run; cleared by
    /// the orchestrator after the captured runner returns so a stale PID
    /// can't be SIGKILLed by `--hard` after the kernel has recycled it.
    child_pid: Option<u32>,
    /// Auxiliary long-running child PIDs - mock-servers (sæhrimnir) that
    /// live across many `child_pid` rotations. Plural because
    /// `service --all` keeps one mock per distinct fixture alive for the
    /// whole cohort. `brokkr kill --hard` SIGKILLs each of these alongside
    /// `child_pid` so none leak.
    mock_pids: Vec<u32>,
    /// Current bench-run progress as `(run, total)` (1-based).
    progress: Option<(u32, u32)>,
}

/// The flock-owning core shared by every [`LockGuard`] handed out for one
/// hold. Exactly one exists per held lock file; nested [`acquire`] calls in
/// the same process get another `Arc` to it (see [`acquire`] for why), so
/// the flock is released only when the *last* guard drops.
struct LockInner {
    fd: OwnedFd,
    /// The lock file this flock is on. Re-entry matches on it, so a test
    /// lock on a scratch path can never alias the real global lock.
    path: PathBuf,
    state: Mutex<LockState>,
    /// Toolchain-disable guard activated under this lock (when `disable_toolchain`
    /// is armed). Restored on drop *before* the flock is released, so the pinned
    /// rust-toolchain is moved aside for exactly the locked window. See
    /// [`crate::toolchain`].
    toolchain: Option<crate::toolchain::DisabledToolchain>,
}

impl Drop for LockInner {
    fn drop(&mut self) {
        // Restore the disabled toolchain (if any) while we still hold the flock,
        // then release. Doing it before LOCK_UN keeps the moved-aside window
        // inside the locked window, so a concurrent brokkr can never observe it.
        drop(self.toolchain.take());
        // The flock is released automatically when the fd is closed, but
        // unlock explicitly for clarity. OwnedFd handles close.
        unsafe {
            libc::flock(self.fd.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// RAII lock guard. The underlying flock is released when the last guard for
/// this hold drops ([`LockInner`]'s drop); in the common non-nested case that
/// is simply this guard's drop, as before.
pub struct LockGuard {
    inner: Arc<LockInner>,
}

/// The locks this process currently holds, weakly. [`acquire`] consults this
/// to re-enter an existing hold instead of opening a second fd on the same
/// file - `flock(2)` treats each fd as an independent contender even within
/// one process, so that second fd would block forever on a lock the process
/// itself holds (the self-deadlock trap that makes naive "hoist an acquire
/// around a loop of acquiring callees" fatal).
///
/// `Weak` so the registry never extends a hold: release is driven purely by
/// guard drops. Dead entries are swept opportunistically on each access.
static HELD: Mutex<Vec<Weak<LockInner>>> = Mutex::new(Vec::new());

/// Lock the registry, treating poison as recoverable. The critical sections
/// are tiny and allocation-only; if one somehow panicked, falling back to the
/// inner value is strictly safer than skipping the re-entry check (which
/// would send a nested acquire into the flock self-deadlock).
fn held_registry() -> std::sync::MutexGuard<'static, Vec<Weak<LockInner>>> {
    HELD.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl LockGuard {
    /// Record the PID of the child process currently running under the lock,
    /// and rewrite the lock file so concurrent `brokkr lock` invocations can
    /// see it.
    pub fn set_child_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.child_pid = Some(pid);
            rewrite_from_state(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Add an auxiliary mock-server PID. `service --all` calls this once
    /// per distinct fixture spawned over the cohort's lifetime; `sync`
    /// (run or bench) and single-script `service` call it once. `brokkr kill --hard`
    /// SIGKILLs every PID in this set alongside `child_pid` so no mock
    /// leaks when the workload child is the one written to `child_pid`.
    pub fn add_mock_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            if !state.mock_pids.contains(&pid) {
                state.mock_pids.push(pid);
            }
            rewrite_from_state(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Remove a single mock-server PID. Used when one fixture session
    /// has drained but others remain (`service --all`'s cohort-scoped
    /// fixture reuse model).
    pub fn remove_mock_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.mock_pids.retain(|p| *p != pid);
            rewrite_from_state(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Drop all mock-server PIDs (e.g. after the suite has drained every
    /// mock gracefully).
    pub fn clear_mock_pids(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.mock_pids.clear();
            rewrite_from_state(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Drop the workload child PID. Called by orchestrators after the
    /// captured runner returns so a stale PID can't be SIGKILLed by
    /// `--hard` once the kernel has recycled it.
    pub fn clear_child_pid(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.child_pid = None;
            rewrite_from_state(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Record current bench-run progress (1-based run index out of total).
    /// Skips the update when `total <= 1` - a lone "run 1/1" line in
    /// `brokkr lock` is noise.
    pub fn set_progress(&self, run: u32, total: u32) {
        if total <= 1 {
            return;
        }
        if let Ok(mut state) = self.inner.state.lock() {
            state.progress = Some((run, total));
            rewrite_from_state(self.inner.fd.as_raw_fd(), &state);
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
    pub args: String,
    pub project_root: String,
    pub child_pid: Option<u32>,
    pub mock_pids: Vec<u32>,
    pub progress: Option<(u32, u32)>,
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

/// Acquire an exclusive lock on the global lock file, blocking until free.
///
/// If the lock is held by *another process*, prints a waiting message
/// describing the holder and blocks until it is released. On success, writes
/// PID + context to the lock file.
///
/// **Re-entrant within the process.** If this process already holds the lock,
/// the call returns immediately with another guard on the same hold, and the
/// flock is released only when the last guard drops. This exists for the
/// gate cohort (`sync --gate all`): it acquires once around the whole sweep
/// so measurement stays serialized end-to-end, while each swept member
/// (`BenchHarness::new`) still contains its own acquire for the
/// single-invocation path. Without re-entrancy the inner acquire would open
/// a second fd on the same file and `flock(2)` would block it on the lock
/// this very process holds - a self-deadlock. (`sync --all` also holds the
/// lock sweep-wide, but no longer nests: its per-script runs share the
/// sweep's prebuilt harness and its single acquire.)
///
/// The trade-off, deliberately accepted: two *threads* of one process
/// first-acquiring concurrently used to serialize on the flock and now may
/// share the hold if one registers before the other checks. Brokkr's command
/// flow is sequential - every acquire on one thread is part of the same
/// logical work unit - so sharing is the correct semantics here. Cross-process
/// serialization (the property benchmarks depend on) is untouched.
///
/// A nested acquire keeps the outer hold's lock-file contents (project /
/// command / args): the outer command is the honest holder for `brokkr lock`
/// to report, and its `ctx` was captured from the same argv anyway.
pub fn acquire(ctx: &LockContext<'_>) -> Result<LockGuard, DevError> {
    let path = lock_path()?;
    acquire_at(&path, ctx)
}

/// Path-explicit body of [`acquire`] - also the unit-test seam, so tests can
/// exercise nesting on a scratch lock file without touching the real one.
fn acquire_at(path: &Path, ctx: &LockContext<'_>) -> Result<LockGuard, DevError> {
    {
        let mut held = held_registry();
        held.retain(|w| w.strong_count() > 0);
        for weak in held.iter() {
            if let Some(inner) = weak.upgrade()
                && inner.path == *path
            {
                return Ok(LockGuard { inner });
            }
        }
    }

    let c_path = path_to_cstring(path)?;
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
                        "PID {} - {} {} ({}{})",
                        i.pid, i.project, i.command, i.project_root, uptime
                    )
                }
                None => "unknown process".into(),
            };
            crate::output::lock_msg(&format!("waiting for {desc}"));
            if let Some(ref i) = info
                && let Some(summary) = process_summary(i.pid)
            {
                crate::output::lock_msg(&summary);
            }
            // The lines above are a one-shot snapshot: we block in a single
            // flock() below and never re-read the holder's stats while waiting.
            // Point at `brokkr lock`, which re-samples (live progress, child
            // PID, mock servers, last marker) on every invocation, so a waiter
            // who wants a fresh view has an honest place to get one.
            crate::output::lock_msg(
                "these numbers won't update here - run 'brokkr lock' in another shell for a live view",
            );

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
    // We now hold the flock. Activate any armed toolchain-disable *inside* the
    // lock so the moved-aside window is exactly the locked window; the guard is
    // stored below and restored on drop before the flock is released. On error,
    // `owned` drops here and releases the flock.
    let toolchain = crate::toolchain::activate_for_lock()?;
    let state = build_state(ctx);
    rewrite_from_state(owned.as_raw_fd(), &state);
    let inner = Arc::new(LockInner {
        fd: owned,
        path: path.to_owned(),
        state: Mutex::new(state),
        toolchain,
    });
    // Register weakly so a later acquire in this process re-enters this hold
    // instead of self-deadlocking on a second fd.
    held_registry().push(Arc::downgrade(&inner));
    Ok(LockGuard { inner })
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

    // Try to acquire - if we succeed, no one holds it.
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
        // Could not parse - report as unknown holder.
        return Ok(Some(LockInfo {
            pid: 0,
            project: "unknown".into(),
            command: "unknown".into(),
            args: String::new(),
            project_root: "unknown".into(),
            child_pid: None,
            mock_pids: Vec::new(),
            progress: None,
        }));
    };

    // Check if the PID is still alive.
    if info.pid > 0 && !pid_alive(info.pid) {
        // Stale lock - the holder crashed. Remove the file so the next
        // flock attempt can succeed (the dead process's flock is already
        // released by the kernel, but removing the file is cleaner).
        std::fs::remove_file(&path).ok();
        return Ok(None);
    }

    Ok(Some(info))
}

/// Public wrapper around `process_uptime_str` for callers outside this module
/// (e.g. `brokkr lock` displaying the brokkr PID's uptime).
pub fn process_uptime(pid: u32) -> Option<String> {
    process_uptime_str(pid)
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

/// Build the initial `LockState` for a freshly-acquired lock. Captures the
/// current brokkr invocation args (argv minus argv[0]) so `brokkr lock`
/// can show exactly what the user typed.
fn build_state(ctx: &LockContext<'_>) -> LockState {
    LockState {
        project: ctx.project.to_owned(),
        command: ctx.command.to_owned(),
        args: current_invocation_args(),
        project_root: ctx.project_root.to_owned(),
        child_pid: None,
        mock_pids: Vec::new(),
        progress: None,
    }
}

/// Rewrite the lock file contents from the given state.
///
/// Fields are newline-separated `key=value` pairs. `child_pid=` and
/// `progress=` are only emitted when set, so a bare acquire without any
/// bench activity produces the minimal file.
fn rewrite_from_state(fd: RawFd, state: &LockState) {
    // Field order: all fixed-size / short fields first, then the unbounded
    // `args=` line last. Truncation from read buffer limits (or a long argv)
    // only sacrifices `args`; `pid`/`child_pid`/`progress`/`root` are safe.
    let mut contents = format!(
        "pid={}\nproject={}\ncommand={}\nroot={}\n",
        std::process::id(),
        state.project,
        state.command,
        state.project_root,
    );
    if let Some(pid) = state.child_pid {
        contents.push_str(&format!("child_pid={pid}\n"));
    }
    if !state.mock_pids.is_empty() {
        let joined = state
            .mock_pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        contents.push_str(&format!("mock_pids={joined}\n"));
    }
    if let Some((run, total)) = state.progress {
        contents.push_str(&format!("progress={run}/{total}\n"));
    }
    contents.push_str(&format!("args={}\n", state.args));

    // Write first, then truncate. The inverse order (truncate → write) gave
    // a concurrent `brokkr lock` reader a window to read 0 bytes and print
    // `PID 0: unknown`. Writing first means any reader sees either the old
    // full contents or a valid new prefix (plus harmless stale trailing
    // bytes that line-prefix parsing ignores).
    unsafe {
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
            return;
        }
        // Trim any stale tail from a previous longer write. In practice
        // we only ever grow (fields are append-only once set), so this is
        // usually a no-op.
        if libc::ftruncate(fd, n as libc::off_t) == -1 {
            eprintln!(
                "[lock] warning: failed to truncate lock file: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Capture `std::env::args()` minus argv[0], shell-quoting any element that
/// contains whitespace or a double-quote so the joined string is unambiguous.
fn current_invocation_args() -> String {
    let args: Vec<String> = std::env::args().skip(1).collect();
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_owned()
    }
}

/// Read lock file contents and parse the key=value fields.
fn read_lock_contents(fd: RawFd) -> Option<LockInfo> {
    // Read the full file, not a fixed-size prefix - a long argv could
    // otherwise push the trailing lines out of range.
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

    let mut contents: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        let len = usize::try_from(n).ok()?;
        contents.extend_from_slice(&chunk[..len]);
        if len < chunk.len() {
            break;
        }
    }
    if contents.is_empty() {
        return None;
    }

    let text = std::str::from_utf8(&contents).ok()?;

    let mut pid: u32 = 0;
    let mut project = String::new();
    let mut command = String::new();
    let mut args = String::new();
    let mut root = String::new();
    let mut child_pid: Option<u32> = None;
    let mut mock_pids: Vec<u32> = Vec::new();
    let mut progress: Option<(u32, u32)> = None;

    for line in text.lines() {
        if let Some(v) = line.strip_prefix("pid=") {
            pid = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("project=") {
            project = v.trim().to_owned();
        } else if let Some(v) = line.strip_prefix("command=") {
            command = v.trim().to_owned();
        } else if let Some(v) = line.strip_prefix("args=") {
            args = v.trim().to_owned();
        } else if let Some(v) = line.strip_prefix("root=") {
            root = v.trim().to_owned();
        } else if let Some(v) = line.strip_prefix("child_pid=") {
            child_pid = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("mock_pids=") {
            mock_pids = v
                .trim()
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect();
        } else if let Some(v) = line.strip_prefix("progress=")
            && let Some((r, t)) = v.trim().split_once('/')
            && let (Ok(r), Ok(t)) = (r.parse::<u32>(), t.parse::<u32>())
        {
            progress = Some((r, t));
        }
    }

    if pid == 0 && project.is_empty() {
        return None;
    }

    Some(LockInfo {
        pid,
        project,
        command,
        args,
        project_root: root,
        child_pid,
        mock_pids,
        progress,
    })
}

/// Convert a `Path` to a `CString`.
fn path_to_cstring(path: &std::path::Path) -> Result<std::ffi::CString, DevError> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| DevError::Lock(format!("lock path contains nul byte: {}", path.display())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Per-test scratch lock file. Each test gets its own path so parallel
    /// test threads never contend (or falsely nest) with each other, and
    /// none of them ever touch the real global lock.
    fn tmp_lock(name: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/lockfile");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        // Stale file from a previous run is fine - flock state died with
        // that process - but remove it so contents assertions start clean.
        std::fs::remove_file(&path).ok();
        path
    }

    fn ctx() -> LockContext<'static> {
        LockContext {
            project: "test",
            command: "lock-test",
            project_root: "/nonexistent",
        }
    }

    /// Probe whether the flock on `path` is held, from a *fresh fd* - which
    /// is exactly the position a second brokkr process (or the naive hoisted
    /// acquire) would be in, since flock treats each fd independently even
    /// within one process.
    fn flock_is_held(path: &Path) -> bool {
        let c = path_to_cstring(path).unwrap();
        let fd = open_lock_file(&c).unwrap();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        let held = ret != 0;
        if !held {
            unsafe {
                libc::flock(fd, libc::LOCK_UN);
            }
        }
        // Close the probe fd either way.
        let _close = unsafe { OwnedFd::from_raw_fd(fd) };
        held
    }

    /// The defect scenario: a cohort acquires, then a swept member acquires
    /// again. Pre-fix this self-deadlocked; post-fix the nested call returns
    /// a second handle on the same hold.
    ///
    /// Run on a worker thread under a watchdog, because the failure mode
    /// being guarded against is a *block*, not a wrong value. Asserting it
    /// inline would mean a reintroduced deadlock hangs the whole suite -
    /// which reads as broken CI rather than as this regression. The
    /// watchdog turns it back into a named failure.
    #[test]
    fn nested_acquire_reenters_the_same_hold() {
        let path = tmp_lock("nested.lock");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            let outer = acquire_at(&worker_path, &ctx()).unwrap();
            let nested = acquire_at(&worker_path, &ctx()).unwrap();
            tx.send((
                Arc::ptr_eq(&outer.inner, &nested.inner),
                Arc::strong_count(&outer.inner),
            ))
            .ok();
        });

        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok((same_hold, strong)) => {
                assert!(same_hold, "nested acquire must share the outer's hold");
                assert_eq!(strong, 2);
                worker.join().unwrap();
            }
            Err(_) => panic!(
                "nested acquire_at() blocked - lockfile re-entrancy is gone, so a \
                 cohort hoisting an acquire around acquiring callees will \
                 self-deadlock on a second flock fd"
            ),
        }
    }

    /// Dropping the inner guard must NOT release the flock - the cohort's
    /// whole point is that the lock spans the sweep, not each member. Only
    /// the last guard's drop releases.
    #[test]
    fn inner_drop_keeps_lock_until_outer_drop() {
        let path = tmp_lock("drop-order.lock");
        let outer = acquire_at(&path, &ctx()).unwrap();
        let nested = acquire_at(&path, &ctx()).unwrap();

        drop(nested);
        assert!(
            flock_is_held(&path),
            "inner drop must not release the sweep's lock"
        );

        drop(outer);
        assert!(
            !flock_is_held(&path),
            "outermost drop must release the flock"
        );
    }

    /// Guard-drop order is refcounted, not stack-ordered: releasing the
    /// *outer* handle first while the nested one lives must also keep the
    /// flock (a swept member is still running under it).
    #[test]
    fn outer_drop_before_inner_keeps_lock() {
        let path = tmp_lock("outer-first.lock");
        let outer = acquire_at(&path, &ctx()).unwrap();
        let nested = acquire_at(&path, &ctx()).unwrap();

        drop(outer);
        assert!(flock_is_held(&path));

        drop(nested);
        assert!(!flock_is_held(&path));
    }

    /// The mutating methods on a nested guard reach the one real hold: the
    /// lock file a concurrent `brokkr lock` reads reflects them.
    #[test]
    fn nested_guard_forwards_state_to_the_lock_file() {
        let path = tmp_lock("forwarding.lock");
        let outer = acquire_at(&path, &ctx()).unwrap();
        let nested = acquire_at(&path, &ctx()).unwrap();

        nested.set_child_pid(4242);
        nested.add_mock_pid(5151);
        nested.set_progress(2, 5);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("child_pid=4242"), "got: {contents}");
        assert!(contents.contains("mock_pids=5151"), "got: {contents}");
        assert!(contents.contains("progress=2/5"), "got: {contents}");

        // And the clears forward too - via the outer handle, proving both
        // handles mutate the same state.
        outer.clear_child_pid();
        nested.clear_mock_pids();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("child_pid="), "got: {contents}");
        assert!(!contents.contains("mock_pids="), "got: {contents}");
    }

    /// Re-entry matches on the lock *path* - a hold on one file must not
    /// satisfy an acquire on another (the test seam depends on this, and it
    /// keeps the registry honest if a second lock file ever exists).
    #[test]
    fn different_paths_do_not_alias() {
        let path_a = tmp_lock("distinct-a.lock");
        let path_b = tmp_lock("distinct-b.lock");
        let a = acquire_at(&path_a, &ctx()).unwrap();
        let b = acquire_at(&path_b, &ctx()).unwrap();
        assert!(!Arc::ptr_eq(&a.inner, &b.inner));
        assert_eq!(Arc::strong_count(&a.inner), 1);
        assert_eq!(Arc::strong_count(&b.inner), 1);
    }

    /// A fresh acquire after full release takes a fresh hold (the dead
    /// registry entry is swept, not resurrected).
    #[test]
    fn reacquire_after_release_is_a_fresh_hold() {
        let path = tmp_lock("reacquire.lock");
        let first = acquire_at(&path, &ctx()).unwrap();
        drop(first);
        assert!(!flock_is_held(&path));

        let second = acquire_at(&path, &ctx()).unwrap();
        assert_eq!(Arc::strong_count(&second.inner), 1);
        assert!(flock_is_held(&path));
    }
}
