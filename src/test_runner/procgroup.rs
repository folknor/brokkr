//! Process-group setup, output drains, and low-level pipe/process mechanics.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::{HungTest, RunContext};
use crate::error::DevError;

/// Read `pipe` into a shared buffer on a new thread until EOF or `cancel`.
pub(super) fn drain_to_buffer<R>(
    mut pipe: R,
    cancel: Arc<AtomicBool>,
) -> (Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>)
where
    R: Read + std::os::fd::AsRawFd + Send + 'static,
{
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let buf_t = Arc::clone(&buf);
    let handle = thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        while let Some(n) = read_unless_cancelled(&mut pipe, &mut chunk, &cancel) {
            if let Ok(mut b) = buf_t.lock() {
                b.extend_from_slice(&chunk[..n]);
            }
        }
    });
    (buf, handle)
}

pub(super) fn spawn_cargo_process_group(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    context: &RunContext,
) -> Result<std::process::Child, DevError> {
    // The cargo spawn profile, naming the check phase in flight (if any) so a
    // `[cargo_env]` failure is attributed to it.
    let profile = crate::cargo_env::SpawnProfile::Cargo {
        phase: crate::check_cmd::current_phase(),
    };
    spawn_process_group("cargo", args, cwd, env, context, profile)
}

/// Spawn `program` in its own process group with piped output. The general
/// form behind [`spawn_cargo_process_group`]: the parallel test lane executes
/// prebuilt test binaries directly (no cargo re-entry), so the program is a
/// parameter rather than a constant.
pub(super) fn spawn_process_group(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    context: &RunContext,
    profile: crate::cargo_env::SpawnProfile,
) -> Result<std::process::Child, DevError> {
    use std::os::unix::process::CommandExt;

    // A shutdown already requested means the run is unwinding: a queued
    // parallel binary, or the next sweep, must not start a group the handler
    // will never see registered in time.
    if context.stop.is_requested() {
        return Err(DevError::Interrupted);
    }

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::cargo_env::apply(&mut cmd, profile, args, Some(cwd), env)?;
    crate::oom::protect_child(&mut cmd);
    // The group is outside brokkr's own, so nothing aimed at brokkr reaches it.
    // Both runners register it with `shutdown::GroupReaper` for SIGINT/SIGTERM;
    // this covers the direct child when brokkr is SIGKILLed by anything that
    // does not walk its tree first (`brokkr kill --hard` does - see
    // `shutdown::kill_descendants` - and so reaches a test binary under cargo;
    // this reaches only the direct child). Both runners wait on the child from
    // the spawning thread, which the death signal requires.
    crate::shutdown::die_with_parent(&mut cmd);
    // What the death signal cannot reach - the test binary under cargo, a
    // doctest under rustdoc, anything a test spawned - carries this run's
    // token, and the next fresh hold reaps it once this brokkr is gone. See
    // `crate::test_orphans`. After the caller's env, so a sweep env cannot
    // displace it.
    crate::test_orphans::stamp(&mut cmd);
    // Last, after every caller-supplied env var: see `crate::hold`.
    crate::hold::stamp(&mut cmd);

    cmd.spawn().map_err(|error| DevError::Spawn {
        program: program.into(),
        error,
    })
}

pub(super) fn clone_buffer(buf: &Arc<Mutex<Vec<u8>>>, label: &str) -> Result<Vec<u8>, DevError> {
    buf.lock()
        .map(|v| v.clone())
        .map_err(|_| DevError::Build(format!("{label} buffer poisoned")))
}

pub(super) fn clone_hung(
    hung: &Arc<Mutex<Option<HungTest>>>,
) -> Result<Option<HungTest>, DevError> {
    hung.lock()
        .map(|h| h.clone())
        .map_err(|_| DevError::Build("hung-test state poisoned".into()))
}

pub(super) fn drain_stderr<F, G>(
    mut pipe: ChildStderr,
    buf: &Mutex<Vec<u8>>,
    cancel: &AtomicBool,
    mut forward_line: F,
    start: Instant,
    on_build_finished: G,
) where
    F: FnMut(&str),
    G: FnOnce(Duration),
{
    let mut on_build_finished = Some(on_build_finished);
    let mut read_buf = [0_u8; 4096];
    let mut line = Vec::<u8>::new();

    while let Some(n) = read_unless_cancelled(&mut pipe, &mut read_buf, cancel) {
        if let Ok(mut out) = buf.lock() {
            out.extend_from_slice(&read_buf[..n]);
        }
        for &byte in &read_buf[..n] {
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let text = String::from_utf8_lossy(&line).into_owned();
                if on_build_finished.is_some()
                    && is_cargo_finished_line(&text)
                    && let Some(cb) = on_build_finished.take()
                {
                    cb(start.elapsed());
                }
                forward_line(&text);
                line.clear();
            } else {
                line.push(byte);
            }
        }
    }

    if !line.is_empty() {
        let text = String::from_utf8_lossy(&line).into_owned();
        forward_line(&text);
    }
}

/// What one read from a child's output pipe produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chunk {
    Data(usize),
    /// The writer closed the pipe: everything it wrote was read.
    Eof,
    /// The read (or the poll before it) failed.
    Error,
    /// The caller told the reader to stop, so whatever was still unwritten or
    /// unread is lost - the one end after which a stream may be truncated.
    Cancelled,
}

/// One read from a child's output pipe. Waits in `poll` with a short timeout
/// rather than in a blocking `read`, so cancellation is observed within one
/// interval whether the pipe is idle or streaming.
pub(crate) fn read_chunk<R: Read + std::os::fd::AsRawFd>(
    pipe: &mut R,
    buf: &mut [u8],
    cancel: &AtomicBool,
) -> Chunk {
    loop {
        if cancel.load(Ordering::Acquire) {
            return Chunk::Cancelled;
        }
        let mut pfd = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for an fd we own, for the call's duration.
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready == 0 {
            continue;
        }
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Chunk::Error;
        }
        return match pipe.read(buf) {
            Ok(0) => Chunk::Eof,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => Chunk::Error,
            Ok(n) => Chunk::Data(n),
        };
    }
}

/// [`read_chunk`] for a caller that only needs the data: `None` at any end.
pub(crate) fn read_unless_cancelled<R: Read + std::os::fd::AsRawFd>(
    pipe: &mut R,
    buf: &mut [u8],
    cancel: &AtomicBool,
) -> Option<usize> {
    match read_chunk(pipe, buf, cancel) {
        Chunk::Data(n) => Some(n),
        Chunk::Eof | Chunk::Error | Chunk::Cancelled => None,
    }
}

/// How long the output drains get to reach EOF on their own once the child
/// has exited.
pub(super) const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Join a run's output drains after its child exited. They normally reach EOF
/// at once; a process that inherited the child's stdout or stderr (a daemon a
/// test leaked) keeps them open forever, and an unbounded join then hangs
/// brokkr with the lock held. So after [`DRAIN_GRACE`] the drains are told to
/// stop and joined - always joined, so no thread outlives the run to append to
/// its buffers or print into the next one. No signal is sent: the run's
/// process group may be gone and its id reused. A leaked process that keeps
/// writing gets `EPIPE`; one carrying the run token is reaped at the next hold
/// (`crate::test_orphans`). Returns whether capture was cut short.
pub(super) fn join_drains(
    drains: Vec<thread::JoinHandle<()>>,
    cancel: &AtomicBool,
    context: &RunContext,
) -> bool {
    let until = Instant::now() + DRAIN_GRACE;
    while Instant::now() < until && !drains.iter().all(thread::JoinHandle::is_finished) {
        thread::sleep(Duration::from_millis(10));
    }
    let cut = !drains.iter().all(thread::JoinHandle::is_finished);
    if cut {
        cancel.store(true, Ordering::Release);
        context.warn(&format!(
            "the test output did not close within {}s of the test process exiting; capture \
             stopped there. A process the tests started (a leaked daemon, say) may still hold \
             the output pipe",
            DRAIN_GRACE.as_secs()
        ));
    }
    for d in drains {
        d.join().ok();
    }
    cut
}

fn is_cargo_finished_line(line: &str) -> bool {
    line.trim_start().starts_with("Finished ")
}

pub(super) fn direct_child_pids(parent: u32) -> Vec<u32> {
    let task_children = Path::new("/proc")
        .join(parent.to_string())
        .join("task")
        .join(parent.to_string())
        .join("children");
    let mut out = fs::read_to_string(task_children)
        .ok()
        .map(|s| parse_pid_list(&s))
        .unwrap_or_default();
    if out.is_empty() {
        out = scan_proc_children(parent);
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn parse_pid_list(text: &str) -> Vec<u32> {
    text.split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

fn scan_proc_children(parent: u32) -> Vec<u32> {
    // Some kernels/configurations do not expose task/<tid>/children. Walking
    // /proc is slower but only happens on the hang path.
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let pid = name.to_str()?.parse::<u32>().ok()?;
            let status = fs::read_to_string(entry.path().join("status")).ok()?;
            (status_ppid(&status) == Some(parent)).then_some(pid)
        })
        .collect()
}

fn status_ppid(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("PPid:")?;
        rest.trim().parse::<u32>().ok()
    })
}
