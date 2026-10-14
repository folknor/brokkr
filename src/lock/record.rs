use std::os::unix::io::RawFd;

use super::{AcqId, AuthHash, LockRecord, LockState};
use crate::process::{BootId, HostScope, NsInode, Pid, ProcIdentity, StartTicks};

const READ_STABILITY_ATTEMPTS: usize = 3;
const READ_STABILITY_RETRY: std::time::Duration = std::time::Duration::from_millis(10);

/// Info read back from the lock file. `starttime` and `boot_id` are the
/// holder's identity tokens; every PID here is meaningful only after
/// [`crate::process::verify_identity`] passes for it - the numbers were written in the
/// holder's PID namespace, which may not be the reader's.
pub struct LockInfo {
    pub pid: u32,
    pub starttime: String,
    pub boot_id: String,
    /// The holder's acquisition id as read from the lock file. The service
    /// reparses it as `AcqId` before constructing a socket path; it stays a
    /// string here because the `brokkr lock` renderer prints it as read.
    /// Empty when absent or unreadable.
    pub acq_id: String,
    pub pid_ns: String,
    pub time_ns: String,
    pub codex_thread: String,
    /// The hash of the holder's capability nonce (`crate::hold`), `None` while
    /// it drains or when unrecorded. What a waiting descendant compares its
    /// inherited nonce against.
    pub auth: Option<AuthHash>,
    pub project: String,
    pub command: String,
    pub args: String,
    pub project_root: String,
    pub child: Option<(u32, String)>,
    pub mocks: Vec<(u32, String)>,
    pub progress: Option<(u32, u32)>,
}
impl LockInfo {
    /// Decode the holder's wire fields into a typed process identity.
    pub fn process_identity(&self) -> Option<ProcIdentity> {
        Some(ProcIdentity::new(
            Pid::new(self.pid)?,
            StartTicks::parse(&self.starttime)?,
        ))
    }

    /// Decode the recorded boot and namespace scope, if every token is
    /// present. Older records can still use the procfs-only compatibility
    /// rule in `process::shares_namespace_tokens`.
    pub fn host_scope(&self) -> Option<HostScope> {
        Some(HostScope::new(
            BootId::new(&self.boot_id)?,
            NsInode::parse(&self.pid_ns)?,
            NsInode::parse(&self.time_ns)?,
        ))
    }
}

/// Publish updated state to the lock file, degrading safely on failure: a
/// state update that cannot be written must not leave the *previous* state
/// standing, because that state may advertise a child or mock PID we no
/// longer track - which `kill --hard` could legitimately verify and signal.
/// So on failure the metadata is invalidated (truncated to zero, readers
/// fail closed) and the run continues; a later successful update republishes
/// in full, since every rewrite carries the complete state.
pub(super) fn publish(fd: RawFd, state: &LockState) {
    if let Err(e) = rewrite_from_state(fd, state) {
        invalidate_mutable_metadata(fd, state);
        crate::output::warn_forced(&format!("lock: failed to write lock metadata: {e}"));
    }
}

/// Clear the mutable record while preserving this hold's authorization.
///
/// Truncating the whole file here - which is what this used to do - would strip
/// `auth` from a hold that is still live and still driving compilers, so every
/// subsequent child of the *current* holder would be refused by the guard: a
/// bookkeeping failure would silently become an inability to compile. So the
/// identity and authorization block is rewritten and only the mutable tail
/// (child, mocks, progress) is cleared, which is the part that could otherwise
/// advertise a PID we no longer track to `kill --hard`.
///
/// If even that write fails there is nothing honest left but full
/// invalidation: readers fail closed, and this holder's own later compiles are
/// refused. That is the correct direction - a reader trusting a stale child PID
/// is worse than a build that stops.
fn invalidate_mutable_metadata(fd: RawFd, state: &LockState) {
    let cleared = LockState {
        project: state.project.clone(),
        command: state.command.clone(),
        args: state.args.clone(),
        project_root: state.project_root.clone(),
        process_identity: state.process_identity,
        scope: state.scope.clone(),
        acq_id: state.acq_id.clone(),
        codex_thread: state.codex_thread.clone(),
        codex_session: state.codex_session.clone(),
        auth: state.auth.clone(),
        draining: state.draining,
        child: None,
        mocks: Vec::new(),
        progress: None,
    };
    if let Err(e) = rewrite_from_state(fd, &cleared) {
        invalidate_metadata(fd);
        crate::output::warn_forced(&format!("lock: failed to preserve lock authorization: {e}"));
    }
}

/// Truncate the metadata to zero length so readers fail closed. Used on
/// release (while still holding the flock) and after a failed publish.
pub(super) fn invalidate_metadata(fd: RawFd) {
    unsafe {
        if libc::ftruncate(fd, 0) == -1 {
            crate::output::warn_forced(&format!(
                "lock: failed to invalidate lock metadata: {}",
                std::io::Error::last_os_error()
            ));
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
/// Textual values are escaped ([`crate::lockfile::escape_value`]) because paths and argv can
/// contain newlines, which would otherwise inject lines into the format.
pub(super) fn rewrite_from_state(fd: RawFd, state: &LockState) -> std::io::Result<()> {
    // `auth` and `draining` ride with the identity block, ahead of every
    // mutable field: the authorization record must never be the line a
    // truncation sacrifices, and it must be byte-identical across a holder's
    // rewrites so a torn read cannot blend two holders' capabilities.
    let contents = LockRecord {
        pid: std::process::id(),
        starttime: state
            .process_identity
            .map(|identity| identity.start().get().to_string())
            .unwrap_or_default(),
        boot_id: state
            .scope
            .as_ref()
            .map(|scope| scope.boot().as_str().to_owned())
            .unwrap_or_default(),
        acq: state.acq_id.as_ref().map_or("", AcqId::as_str).to_owned(),
        pid_ns: state
            .scope
            .as_ref()
            .map(|scope| scope.pid_ns().get().to_string())
            .unwrap_or_default(),
        time_ns: state
            .scope
            .as_ref()
            .map(|scope| scope.time_ns().get().to_string())
            .unwrap_or_default(),
        codex_thread: state.codex_thread.clone(),
        codex_session: state.codex_session.clone(),
        auth: state.auth.as_ref().map_or("", AuthHash::as_str).to_owned(),
        draining: state.draining,
        child: state.child.as_ref().map(super::TrackedProcess::wire),
        mocks: state
            .mocks
            .iter()
            .map(super::TrackedProcess::wire)
            .collect(),
        progress: state.progress,
        project: state.project.clone(),
        command: state.command.clone(),
        root: state.project_root.clone(),
        args: state.args.clone(),
    }
    .encode();

    // Write first, then truncate. The inverse order (truncate → write) gave
    // a concurrent `brokkr lock` reader a window to read 0 bytes and print
    // an unknown holder. Writing first means any reader sees either the old
    // full contents or a valid new prefix (plus stale trailing bytes, which
    // first-occurrence parsing renders harmless). Note this is coherence
    // best-effort, not a snapshot guarantee - readers double-read and fail
    // closed on change (see `read_lock_contents`).
    let bytes = contents.as_slice();
    unsafe {
        if libc::lseek(fd, 0, libc::SEEK_SET) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let mut written = 0usize;
    while written < bytes.len() {
        let n = unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
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

/// Read lock file contents and parse the key=value fields.
///
/// Reads the file **twice** and requires byte-identical contents, retrying a
/// few times, before parsing - stated narrowly, this rejects metadata that
/// changes between reads; it cannot prove identical reads are untorn (a
/// writer descheduled mid-write is stable). The remaining torn shapes are
/// defused by the format instead: identity fields are a byte-identical
/// prefix per holder, every key is always emitted, and parsing is
/// first-occurrence-wins so a stale tail can never override a fresh prefix.
pub(super) fn read_lock_contents(fd: RawFd) -> Option<LockInfo> {
    for attempt in 0..READ_STABILITY_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(READ_STABILITY_RETRY);
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
pub(super) fn parse_lock_contents(text: &str) -> Option<LockInfo> {
    let record = LockRecord::decode(text.as_bytes())?;
    Some(LockInfo {
        pid: record.pid,
        starttime: record.starttime,
        boot_id: record.boot_id,
        acq_id: record.acq,
        pid_ns: record.pid_ns,
        time_ns: record.time_ns,
        codex_thread: record.codex_thread,
        auth: AuthHash::parse(&record.auth),
        project: record.project,
        command: record.command,
        args: record.args,
        project_root: record.root,
        child: record.child,
        mocks: record.mocks,
        progress: record.progress,
    })
}
