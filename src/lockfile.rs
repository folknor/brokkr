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
    /// This process's `/proc/self/stat` starttime (field 22, clock ticks
    /// since boot) as a canonical decimal string. Together with `boot_id`
    /// this is the identity token readers verify before trusting the PID:
    /// a PID number alone is meaningless across PID namespaces (a sandboxed
    /// holder writes pid=2, which is kthreadd on the host) and across PID
    /// recycling. Empty when unreadable - which makes every verification
    /// fail closed, by design.
    starttime: String,
    /// `/proc/sys/kernel/random/boot_id` at acquire time. Discriminates
    /// boots: the lock file lives under `$HOME` and so survives reboot, and a
    /// starttime from a previous boot could coincidentally match an
    /// unrelated process in this one.
    boot_id: String,
    /// PID of the most recent child process brokkr spawned under the lock,
    /// paired with its starttime token (captured when recorded; empty if
    /// unreadable, which fails verification closed). Updated by the harness
    /// each iteration of a bench run; cleared by the orchestrator after the
    /// captured runner returns so a stale PID can't be SIGKILLed by
    /// `--hard` after the kernel has recycled it.
    child: Option<(u32, String)>,
    /// Auxiliary long-running child PIDs - mock-servers (sæhrimnir) that
    /// live across many `child` rotations, each paired with its starttime
    /// token. Plural because `service --all` keeps one mock per distinct
    /// fixture alive for the whole cohort. `brokkr kill --hard` SIGKILLs
    /// each of these alongside `child` so none leak.
    mocks: Vec<(u32, String)>,
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
        // Invalidate the metadata while we still hold the flock. A reader
        // during the release/re-acquire handoff then sees an empty file and
        // fails closed, instead of a complete, still-verifiable record of a
        // holder that no longer holds anything - which `kill` could
        // otherwise legitimately verify and signal. This covers normal
        // release; abnormal termination (SIGKILL) leaves metadata behind,
        // but the kernel has released the flock so `status()`'s probe
        // reports no holder and the next acquirer rewrites it.
        invalidate_metadata(self.fd.as_raw_fd());
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
    /// Record the PID of the child process currently running under the lock
    /// (with its starttime identity token), and rewrite the lock file so
    /// concurrent `brokkr lock` invocations can see it.
    pub fn set_child_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.child = Some((pid, proc_starttime(pid).unwrap_or_default()));
            publish(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Add an auxiliary mock-server PID. `service --all` calls this once
    /// per distinct fixture spawned over the cohort's lifetime; `sync`
    /// (run or bench) and single-script `service` call it once. `brokkr kill --hard`
    /// SIGKILLs every PID in this set alongside the child so no mock
    /// leaks when the workload child is the one written to `child_pid`.
    pub fn add_mock_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            if !state.mocks.iter().any(|(p, _)| *p == pid) {
                state
                    .mocks
                    .push((pid, proc_starttime(pid).unwrap_or_default()));
            }
            publish(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Remove a single mock-server PID. Used when one fixture session
    /// has drained but others remain (`service --all`'s cohort-scoped
    /// fixture reuse model).
    pub fn remove_mock_pid(&self, pid: u32) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.mocks.retain(|(p, _)| *p != pid);
            publish(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Drop all mock-server PIDs (e.g. after the suite has drained every
    /// mock gracefully).
    pub fn clear_mock_pids(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.mocks.clear();
            publish(self.inner.fd.as_raw_fd(), &state);
        }
    }

    /// Drop the workload child PID. Called by orchestrators after the
    /// captured runner returns so a stale PID can't be SIGKILLed by
    /// `--hard` once the kernel has recycled it.
    pub fn clear_child_pid(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.child = None;
            publish(self.inner.fd.as_raw_fd(), &state);
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
            publish(self.inner.fd.as_raw_fd(), &state);
        }
    }
}

/// Context written to the lock file so `brokkr lock` can explain who holds it.
pub struct LockContext<'a> {
    pub project: &'a str,
    pub command: &'a str,
    pub project_root: &'a str,
}

/// Info read back from the lock file. `starttime` and `boot_id` are the
/// holder's identity tokens; every PID here is meaningful only after
/// [`verify_identity`] passes for it - the numbers were written in the
/// holder's PID namespace, which may not be the reader's.
pub struct LockInfo {
    pub pid: u32,
    pub starttime: String,
    pub boot_id: String,
    pub project: String,
    pub command: String,
    pub args: String,
    pub project_root: String,
    pub child: Option<(u32, String)>,
    pub mocks: Vec<(u32, String)>,
    pub progress: Option<(u32, u32)>,
}

/// Resolve the global lock file path: always `$HOME/.brokkr/brokkr.lock`.
///
/// One path, no alternatives. The lock is brokkr's *global* mutual exclusion -
/// two invocations that resolve different paths are not excluding each other,
/// they are two unsynchronised builds sharing one target dir. That is exactly
/// what the previous `$XDG_RUNTIME_DIR`-first rule allowed: the variable is set
/// per *session* by logind and is freely repointed by sandboxes, containers and
/// `systemd-run`, so a brokkr under one and a brokkr in a login shell held two
/// different files and both believed they had the lock.
///
/// Not `~/.cache/brokkr/`: a cache directory is by specification disposable, and
/// unlinking the lock file while a hold is live breaks the mutex outright - the
/// holder keeps its flock on a now-nameless inode while the next invocation
/// creates a fresh file and locks that instead. `~/.brokkr/` is not swept.
///
/// `$HOME` is the one input, and it is per-user by construction, which is what
/// keeps the file writable and the lock scoped to the user whose builds it
/// serialises. If `$HOME` is unset there is no defensible default, so this
/// errors rather than inventing one.
fn lock_path() -> Result<PathBuf, DevError> {
    let home = std::env::var("HOME")
        .map_err(|_| DevError::Lock("$HOME is not set - cannot locate the brokkr lock".into()))?;
    let dir = PathBuf::from(home).join(".brokkr");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("brokkr.lock"))
}

/// Acquire an exclusive lock on the global lock file, blocking until free.
///
/// If the lock is held by *another process*, prints a waiting message
/// describing the holder and blocks until it is released. On success, writes
/// PID + identity tokens + context to the lock file.
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
            let wait_start = std::time::Instant::now();
            // Lead with the rule, not the wait: the holder is the reason
            // brokkr serializes, not competing load. An agent reading this
            // must come away with "the lock protected my measurement", never
            // "the machine was congested so my numbers are suspect".
            crate::output::lock_msg(
                "waiting for the previous brokkr command to finish - brokkr runs one command at a time so measurements never overlap",
            );
            let info = read_lock_contents(fd);
            let mut showed_stats = false;
            match &info {
                Some(i) => {
                    let invocation = if i.args.is_empty() {
                        i.command.clone()
                    } else {
                        i.args.clone()
                    };
                    // No PID in this line: it was written in the holder's
                    // PID namespace and is the one number a reader could
                    // anchor a wrong story to. `brokkr lock` shows it, with
                    // the same verification gate.
                    crate::output::lock_msg(&format!(
                        "holder: {} {} in {}",
                        i.project, invocation, i.project_root
                    ));
                    if let Some(summary) = verified_summary(i.pid, &i.starttime, &i.boot_id) {
                        crate::output::lock_msg(&format!("holder is {summary}"));
                        showed_stats = true;
                    } else {
                        crate::output::lock_msg(
                            "process details unavailable - holder identity could not be verified from this namespace",
                        );
                    }
                }
                None => crate::output::lock_msg("holder: unknown (lock metadata unreadable)"),
            }
            if showed_stats {
                // The lines above are a one-shot snapshot: we block in a single
                // flock() below and never re-read the holder's stats while waiting.
                // Point at `brokkr lock`, which re-samples (live progress, child
                // PID, mock servers, last marker) on every invocation, so a waiter
                // who wants a fresh view has an honest place to get one.
                crate::output::lock_msg(
                    "these numbers won't update here - run 'brokkr lock' in another shell for a live view",
                );
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
            let waited = wait_start.elapsed().as_secs();
            crate::output::lock_msg(&format!(
                "lock acquired after {} - the previous brokkr command has finished. The wait happened before this command's work and measurements began, so it has no effect on their timings or results",
                format_duration(waited),
            ));
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
    // The initial publication must succeed: a hold whose metadata never
    // existed would be indistinguishable from torn metadata for every
    // reader, and `kill` could never target it. Later state updates degrade
    // to invalidate-and-warn instead (see `publish`) - a mid-run bookkeeping
    // failure should not abort a running bench.
    rewrite_from_state(owned.as_raw_fd(), &state).map_err(|e| {
        DevError::Lock(format!("failed to publish lock metadata: {e}"))
    })?;
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
/// The flock is the sole authority on held/not-held: the non-blocking probe
/// either succeeds (no holder - a dead process cannot retain a flock, so a
/// leftover file with no flock is simply not held) or fails (a live process
/// holds it). There is deliberately no PID-liveness fallback and no
/// stale-file deletion: the recorded PID is namespace-local, so "that PID
/// looks dead from here" can only mean the PID is untrustworthy from this
/// namespace, never that the lock is stale - and deleting the path while the
/// old inode is flocked would let the next acquirer lock a *fresh* inode,
/// silently splitting the serialization the lock exists to provide.
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
            starttime: String::new(),
            boot_id: String::new(),
            project: "unknown".into(),
            command: "unknown".into(),
            args: String::new(),
            project_root: "unknown".into(),
            child: None,
            mocks: Vec::new(),
            progress: None,
        }));
    };

    Ok(Some(info))
}

/// Verify that `/proc/{pid}` in *this* namespace describes the process the
/// recorded tokens were captured from. True only when the recorded boot id
/// matches this kernel's and the PID's current starttime equals the recorded
/// one exactly (canonical decimal ticks - fixed at process creation, so it
/// never jitters; exact equality is what defeats PID recycling).
///
/// A failure means "identity could not be verified from this namespace" -
/// PID namespace, time namespace (the kernel offsets displayed starttimes by
/// the reader's timens), another boot, a recycled PID, hidepid, or torn
/// metadata. It deliberately does not distinguish which.
pub fn verify_identity(pid: u32, starttime: &str, boot_id: &str) -> bool {
    if pid == 0 || starttime.is_empty() || boot_id.is_empty() {
        return false;
    }
    match local_boot_id() {
        Some(local) if local == boot_id => {}
        _ => return false,
    }
    proc_starttime(pid).as_deref() == Some(starttime)
}

/// This kernel's boot id, trimmed.
pub fn local_boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_owned())
}

/// A PID's starttime (field 22 of `/proc/{pid}/stat`, clock ticks since
/// boot) as a canonical decimal string. Validated as `u64` and re-rendered,
/// never routed through floating point - this is an identity token, not a
/// duration.
pub fn proc_starttime(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let comm_end = stat.rfind(')')?;
    let fields: Vec<&str> = stat[comm_end + 2..].split_whitespace().collect();
    // Field 19 after comm (index 19 in the post-comm fields) is starttime.
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    Some(ticks.to_string())
}

/// Get how long a verified process has been running, as a human-readable
/// string. `None` when identity verification fails - an unverified PID's
/// uptime is some other process's uptime (in the kthreadd case, the whole
/// machine's).
pub fn verified_uptime(pid: u32, starttime: &str, boot_id: &str) -> Option<String> {
    if !verify_identity(pid, starttime, boot_id) {
        return None;
    }
    process_uptime_str(pid)
}

/// Get how long a process has been running, as a human-readable string.
///
/// Reads `/proc/{pid}/stat` starttime and compares against system uptime.
/// Display only - callers gate on [`verify_identity`] first (or use
/// [`verified_uptime`] / [`verified_summary`]).
fn process_uptime_str(pid: u32) -> Option<String> {
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    if clk_tck <= 0.0 {
        return None;
    }

    // System uptime in seconds.
    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;

    // Process start time in clock ticks since boot.
    let starttime: f64 = proc_starttime(pid)?.parse().ok()?;

    let start_secs = starttime / clk_tck;
    let elapsed_secs = uptime_secs - start_secs;

    if elapsed_secs < 0.0 {
        return None;
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let elapsed = elapsed_secs as u64;
    Some(format_duration(elapsed))
}

/// Format a second count as `3h05m` / `3m12s` / `42s`.
pub fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Build a one-line summary of a running, identity-verified process from
/// `/proc`.
///
/// Returns something like `"running 12m, RSS 4.2 GB, 847 MB read, 4 threads"`.
/// Returns `None` if verification fails, the process is gone, or `/proc` is
/// unreadable. Identity is checked *before and after* collection: the
/// process could exit and its PID be recycled while `/proc/{pid}/status`
/// and `/proc/{pid}/io` are being read, and stats half-read from two
/// different processes must not be displayed.
pub fn verified_summary(pid: u32, starttime: &str, boot_id: &str) -> Option<String> {
    if !verify_identity(pid, starttime, boot_id) {
        return None;
    }
    let summary = process_summary_unverified(pid)?;
    // Re-verify: same starttime means the stats above came from the same
    // process generation.
    if proc_starttime(pid).as_deref() != Some(starttime) {
        return None;
    }
    Some(summary)
}

/// Stat collection body of [`verified_summary`]. Never call for display
/// without the verification sandwich around it.
fn process_summary_unverified(pid: u32) -> Option<String> {
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
/// can show exactly what the user typed, plus the identity tokens readers
/// verify before trusting the PID.
fn build_state(ctx: &LockContext<'_>) -> LockState {
    LockState {
        project: ctx.project.to_owned(),
        command: ctx.command.to_owned(),
        args: current_invocation_args(),
        project_root: ctx.project_root.to_owned(),
        starttime: proc_starttime(std::process::id()).unwrap_or_default(),
        boot_id: local_boot_id().unwrap_or_default(),
        child: None,
        mocks: Vec::new(),
        progress: None,
    }
}

/// Publish updated state to the lock file, degrading safely on failure: a
/// state update that cannot be written must not leave the *previous* state
/// standing, because that state may advertise a child or mock PID we no
/// longer track - which `kill --hard` could legitimately verify and signal.
/// So on failure the metadata is invalidated (truncated to zero, readers
/// fail closed) and the run continues; a later successful update republishes
/// in full, since every rewrite carries the complete state.
fn publish(fd: RawFd, state: &LockState) {
    if let Err(e) = rewrite_from_state(fd, state) {
        eprintln!("[lock] warning: failed to write lock metadata: {e}");
        invalidate_metadata(fd);
    }
}

/// Truncate the metadata to zero length so readers fail closed. Used on
/// release (while still holding the flock) and after a failed publish.
fn invalidate_metadata(fd: RawFd) {
    unsafe {
        if libc::ftruncate(fd, 0) == -1 {
            eprintln!(
                "[lock] warning: failed to invalidate lock metadata: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Rewrite the lock file contents from the given state.
///
/// Fields are newline-separated `key=value` pairs, **every key always
/// emitted** (empty value = unset). Always-emit is load-bearing: readers
/// parse first-occurrence-wins, so a fresh empty `child_pid=` line shadows
/// any stale `child_pid=1234` tail left between the write and the truncate -
/// without it, clearing a field would leave no fresh occurrence to win.
/// Identity fields come first and are byte-identical on every rewrite by one
/// holder, so a torn read can never mix two holders' identities; the
/// unbounded `args=` line is last so truncation only ever sacrifices it.
/// Textual values are escaped ([`escape_value`]) because paths and argv can
/// contain newlines, which would otherwise inject lines into the format.
fn rewrite_from_state(fd: RawFd, state: &LockState) -> std::io::Result<()> {
    let mut contents = format!(
        "pid={}\nstarttime={}\nboot_id={}\n",
        std::process::id(),
        state.starttime,
        state.boot_id,
    );
    match &state.child {
        Some((pid, st)) => {
            contents.push_str(&format!("child_pid={pid}\nchild_starttime={st}\n"));
        }
        None => contents.push_str("child_pid=\nchild_starttime=\n"),
    }
    let mocks = state
        .mocks
        .iter()
        .map(|(p, st)| format!("{p}:{st}"))
        .collect::<Vec<_>>()
        .join(",");
    contents.push_str(&format!("mock_pids={mocks}\n"));
    match state.progress {
        Some((run, total)) => contents.push_str(&format!("progress={run}/{total}\n")),
        None => contents.push_str("progress=\n"),
    }
    contents.push_str(&format!(
        "project={}\ncommand={}\nroot={}\nargs={}\n",
        escape_value(&state.project),
        escape_value(&state.command),
        escape_value(&state.project_root),
        escape_value(&state.args),
    ));

    // Write first, then truncate. The inverse order (truncate → write) gave
    // a concurrent `brokkr lock` reader a window to read 0 bytes and print
    // an unknown holder. Writing first means any reader sees either the old
    // full contents or a valid new prefix (plus stale trailing bytes, which
    // first-occurrence parsing renders harmless). Note this is coherence
    // best-effort, not a snapshot guarantee - readers double-read and fail
    // closed on change (see `read_lock_contents`).
    let bytes = contents.as_bytes();
    unsafe {
        if libc::lseek(fd, 0, libc::SEEK_SET) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let mut written = 0usize;
    while written < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            return Err(std::io::Error::other("zero-byte write to lock file"));
        }
        #[allow(clippy::cast_sign_loss)]
        {
            written += n as usize;
        }
    }
    // Trim any stale tail from a previous longer write - only after the
    // whole buffer landed, and to the full intended length, so a failure
    // above never truncates to a partial record.
    unsafe {
        #[allow(clippy::cast_possible_wrap)]
        if libc::ftruncate(fd, bytes.len() as libc::off_t) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
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

/// Percent-escape the three bytes that would break the line-oriented
/// `key=value` format: `%` (the escape itself), `\n` (line injection) and
/// `\r`. Everything else passes through, keeping the file human-readable.
fn escape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => out.push_str("%25"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(c),
        }
    }
    out
}

/// Inverse of [`escape_value`]. Unknown or truncated escapes pass through
/// literally - this decodes a display string, it must never fail.
fn unescape_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            match &s[i + 1..i + 3] {
                "25" => {
                    out.push('%');
                    i += 3;
                    continue;
                }
                "0A" => {
                    out.push('\n');
                    i += 3;
                    continue;
                }
                "0D" => {
                    out.push('\r');
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        // Advance by one char (values are valid UTF-8; `%` is single-byte).
        let ch_len = s[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Read lock file contents and parse the key=value fields.
///
/// Reads the file **twice** and requires byte-identical contents, retrying a
/// few times, before parsing - stated narrowly, this rejects metadata that
/// changes between reads; it cannot prove identical reads are untorn (a
/// writer descheduled mid-write is stable). The remaining torn shapes are
/// defused by the format instead: identity fields are a byte-identical
/// prefix per holder, every key is always emitted, and parsing is
/// first-occurrence-wins so a stale tail can never override a fresh prefix.
fn read_lock_contents(fd: RawFd) -> Option<LockInfo> {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let first = read_file_bytes(fd)?;
        let second = read_file_bytes(fd)?;
        if first == second && !first.is_empty() {
            let text = std::str::from_utf8(&first).ok()?;
            return parse_lock_contents(text);
        }
    }
    None
}

/// One full read of the lock file from offset 0.
fn read_file_bytes(fd: RawFd) -> Option<Vec<u8>> {
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
    let mut contents: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if n == 0 {
            break;
        }
        let len = usize::try_from(n).ok()?;
        contents.extend_from_slice(&chunk[..len]);
        if len < chunk.len() {
            break;
        }
    }
    Some(contents)
}

/// Parse the `key=value` lines. First occurrence of each key wins (see
/// [`read_lock_contents`]); an empty or unparseable value means unset.
fn parse_lock_contents(text: &str) -> Option<LockInfo> {
    let mut fields: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fields.entry(key).or_insert(value.trim());
        }
    }

    let raw = |key: &str| fields.get(key).map_or("", |v| *v);
    let escaped = |key: &str| unescape_value(raw(key));

    let pid: u32 = raw("pid").parse().unwrap_or(0);
    let project = escaped("project");
    if pid == 0 && project.is_empty() {
        return None;
    }

    let child = raw("child_pid")
        .parse()
        .ok()
        .map(|p: u32| (p, raw("child_starttime").to_owned()));
    let mocks = raw("mock_pids")
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let (p, st) = entry.split_once(':')?;
            Some((p.trim().parse().ok()?, st.trim().to_owned()))
        })
        .collect();
    let progress = raw("progress").split_once('/').and_then(|(r, t)| {
        Some((r.parse::<u32>().ok()?, t.parse::<u32>().ok()?))
    });

    Some(LockInfo {
        pid,
        starttime: raw("starttime").to_owned(),
        boot_id: raw("boot_id").to_owned(),
        project,
        command: escaped("command"),
        args: escaped("args"),
        project_root: escaped("root"),
        child,
        mocks,
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
        crate::test_scratch::scratch_path("lockfile", name)
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
        assert!(contents.contains("mock_pids=5151:"), "got: {contents}");
        assert!(contents.contains("progress=2/5"), "got: {contents}");

        // And the clears forward too - via the outer handle, proving both
        // handles mutate the same state. Every key stays present (always-emit
        // format), but with an empty value.
        outer.clear_child_pid();
        nested.clear_mock_pids();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("child_pid=\n"), "got: {contents}");
        assert!(contents.contains("mock_pids=\n"), "got: {contents}");
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

    /// Release invalidates the metadata (truncate-to-zero under the flock),
    /// so a reader during the handoff fails closed instead of verifying a
    /// holder that no longer holds anything.
    #[test]
    fn release_truncates_metadata() {
        let path = tmp_lock("release-truncate.lock");
        let guard = acquire_at(&path, &ctx()).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().is_empty());
        drop(guard);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    /// The holder's own identity tokens verify against its live /proc entry.
    #[test]
    fn own_identity_verifies() {
        let path = tmp_lock("identity.lock");
        let _guard = acquire_at(&path, &ctx()).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let info = parse_lock_contents(&contents).unwrap();
        assert_eq!(info.pid, std::process::id());
        assert!(
            verify_identity(info.pid, &info.starttime, &info.boot_id),
            "self-identity must verify: starttime={} boot_id={}",
            info.starttime,
            info.boot_id
        );
    }

    /// A wrong starttime or boot id fails verification - the fail-closed
    /// path every namespace/recycling case funnels into.
    #[test]
    fn wrong_tokens_fail_verification() {
        let pid = std::process::id();
        let boot = local_boot_id().unwrap();
        let start = proc_starttime(pid).unwrap();
        assert!(!verify_identity(pid, "1", &boot));
        assert!(!verify_identity(
            pid,
            &start,
            "00000000-0000-0000-0000-000000000000"
        ));
        assert!(!verify_identity(pid, "", &boot));
        assert!(!verify_identity(pid, &start, ""));
    }

    /// First occurrence wins: a stale tail (old longer record surviving
    /// between write and truncate) must not override the fresh prefix.
    #[test]
    fn first_occurrence_parsing_ignores_stale_tail() {
        let text = "pid=100\nstarttime=5\nboot_id=b\nchild_pid=\nchild_starttime=\nmock_pids=\nprogress=\nproject=fresh\ncommand=run\nroot=/r\nargs=\nchild_pid=999\nproject=stale\n";
        let info = parse_lock_contents(text).unwrap();
        assert_eq!(info.pid, 100);
        assert_eq!(info.project, "fresh");
        assert!(info.child.is_none(), "stale child_pid tail must not win");
    }

    /// Escaping round-trips values containing newlines and percent signs -
    /// a path or argv with a newline must not inject format lines.
    #[test]
    fn escape_roundtrip() {
        for v in ["plain", "with\nnewline", "50%\r\n done", "%0A literal"] {
            assert_eq!(unescape_value(&escape_value(v)), v);
        }
        assert!(!escape_value("a\nb").contains('\n'));
    }
}
