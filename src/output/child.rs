use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub use crate::cargo_env::SpawnProfile;
use crate::error::DevError;

/// Why a child stopped. The supervisor assigns this once at the event that
/// ended the run, so callers do not reconstruct it from competing flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    Exited(i32),
    Signalled(i32),
    StopMarker(String),
    Deadline(Duration),
    Interrupted,
    WaitFailed(String),
}

impl Ending {
    fn from_status(status: &ExitStatus) -> Option<Self> {
        use std::os::unix::process::ExitStatusExt;

        status
            .code()
            .map(Self::Exited)
            .or_else(|| status.signal().map(Self::Signalled))
    }

    /// The ending of a child reaped outside the supervisor (a plain
    /// `Command::output`): its exit code or killing signal.
    pub fn from_exit_status(status: &ExitStatus) -> Self {
        Self::from_status(status)
            .unwrap_or_else(|| Self::WaitFailed("wait returned no exit code or signal".to_owned()))
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(*code),
            Self::Signalled(_)
            | Self::StopMarker(_)
            | Self::Deadline(_)
            | Self::Interrupted
            | Self::WaitFailed(_) => None,
        }
    }

    pub fn signal(&self) -> Option<i32> {
        match self {
            Self::Signalled(signal) => Some(*signal),
            Self::Exited(_)
            | Self::StopMarker(_)
            | Self::Deadline(_)
            | Self::Interrupted
            | Self::WaitFailed(_) => None,
        }
    }

    pub fn is_deadline(&self) -> bool {
        matches!(self, Self::Deadline(_))
    }

    /// The child's own exit code or killing signal, when it ended by itself
    /// (or by a signal) rather than by brokkr's supervision.
    pub fn process_ending(&self) -> Option<crate::error::ProcessEnding> {
        match self {
            Self::Exited(code) => Some(crate::error::ProcessEnding::Code(*code)),
            Self::Signalled(signal) => Some(crate::error::ProcessEnding::Signal(*signal)),
            Self::StopMarker(_) | Self::Deadline(_) | Self::Interrupted | Self::WaitFailed(_) => {
                None
            }
        }
    }

    pub fn success(&self) -> bool {
        matches!(self, Self::Exited(0))
    }
}

/// The common classification used by measurement loops and captured commands.
pub enum IterationVerdict {
    Counted,
    Failed(DevError),
    Interrupted,
}

/// Classify an ended child once, retaining its typed ending and captured
/// stderr in a subprocess failure.
pub fn verdict(
    ending: &Ending,
    program: &str,
    stderr: &[u8],
    ok_codes: &[i32],
) -> IterationVerdict {
    match ending {
        Ending::Interrupted => IterationVerdict::Interrupted,
        Ending::WaitFailed(message) => IterationVerdict::Failed(DevError::Spawn {
            program: program.to_owned(),
            error: std::io::Error::other(message.clone()),
        }),
        Ending::Deadline(limit) => IterationVerdict::Failed(DevError::Config(format!(
            "{program} exceeded its {:.1}s deadline",
            limit.as_secs_f64()
        ))),
        Ending::StopMarker(_) => IterationVerdict::Counted,
        Ending::Exited(code) if *code == 0 || ok_codes.contains(code) => IterationVerdict::Counted,
        Ending::Exited(code) => IterationVerdict::Failed(DevError::Subprocess {
            program: program.to_owned(),
            ending: crate::error::ProcessEnding::Code(*code),
            stderr: String::from_utf8_lossy(stderr).into_owned(),
        }),
        Ending::Signalled(signal) => IterationVerdict::Failed(DevError::Subprocess {
            program: program.to_owned(),
            ending: crate::error::ProcessEnding::Signal(*signal),
            stderr: String::from_utf8_lossy(stderr).into_owned(),
        }),
    }
}

// --- Subprocess types ---

/// Captured output from a subprocess.
pub struct CapturedOutput {
    pub ending: Ending,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
}

/// Exit code and elapsed time from a passthrough subprocess.
pub struct PassthroughOutput {
    pub ending: Ending,
    pub elapsed: Duration,
}

impl PassthroughOutput {
    /// The exit code to propagate when the child exited non-zero with a code
    /// not in `ok_codes`. Every other ending a passthrough run returns `Ok`
    /// for counts as success (signal deaths and interrupts are errors already).
    pub fn failed_code(&self, ok_codes: &[i32]) -> Option<i32> {
        match self.ending {
            Ending::Exited(code) if code != 0 && !ok_codes.contains(&code) => Some(code),
            _ => None,
        }
    }
}

impl CapturedOutput {
    /// Whether the child exited by itself with code 0.
    pub fn success(&self) -> bool {
        self.ending.success()
    }

    /// The child's exit code, when it exited by itself.
    pub fn code(&self) -> Option<i32> {
        self.ending.exit_code()
    }

    /// The signal that killed the child, when one did.
    pub fn signal(&self) -> Option<i32> {
        self.ending.signal()
    }

    /// Return `Ok(())` if the process exited successfully, or a `DevError::Subprocess`
    /// with the captured stderr if it failed.
    pub fn check_success(&self, program: &str) -> Result<(), DevError> {
        self.check_success_or(program, &[])
    }

    /// Like `check_success`, but also treats the given exit codes as success.
    /// For example, `diff` uses exit 1 to mean "differences found" (not an error).
    pub fn check_success_or(&self, program: &str, ok_codes: &[i32]) -> Result<(), DevError> {
        match verdict(&self.ending, program, &self.stderr, ok_codes) {
            IterationVerdict::Counted => Ok(()),
            IterationVerdict::Failed(error) => Err(error),
            IterationVerdict::Interrupted => Err(DevError::Interrupted),
        }
    }
}

/// Run a subprocess, capturing stdout and stderr.
///
/// Returns `CapturedOutput` on success (even if the process exited non-zero).
/// Returns `DevError::Spawn` only if the process could not be spawned.
///
/// A plain child: `[cargo_env]` is never evaluated for it. A cargo launch goes
/// through [`run_cargo`], which says so explicitly.
pub fn run_captured(program: &str, args: &[&str], cwd: &Path) -> Result<CapturedOutput, DevError> {
    run_captured_with_env(program, args, cwd, &[])
}

/// Run `cargo` with captured stdio. The spawn carries the cargo profile, so
/// `[cargo_env]` is evaluated for it; `phase` is the `check` phase a recorded
/// evaluation failure is attributed to, when there is one.
pub fn run_cargo(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    phase: Option<crate::cargo_env::Phase>,
) -> Result<CapturedOutput, DevError> {
    let dc = run_captured_limited(
        "cargo",
        args,
        cwd,
        env,
        ChildLimit::Wall(Duration::MAX),
        None,
        false,
        SpawnProfile::Cargo { phase },
        CaptureCap::UNBOUNDED,
    )?;
    Ok(dc.captured)
}

/// As [`run_captured`], but invokes `on_spawn` with the child's PID
/// immediately after `Command::spawn` returns. Lets callers (notably
/// `cargo_build_observed`) publish the live PID into the lockfile so
/// `brokkr kill --hard` during a long cargo build can SIGKILL cargo too.
/// `env` adds variables on top of the inherited environment - the worktree
/// build path uses it to pin `CARGO_TARGET_DIR`.
pub fn run_captured_observed(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
    profile: SpawnProfile,
) -> Result<CapturedOutput, DevError> {
    let dc = run_captured_limited(
        program,
        args,
        cwd,
        env,
        ChildLimit::Wall(Duration::MAX),
        on_spawn,
        isolate_pg,
        profile,
        CaptureCap::UNBOUNDED,
    )?;
    Ok(dc.captured)
}

/// Run a subprocess with extra environment variables, capturing stdout and stderr.
///
/// Variables are added on top of the inherited environment. A plain child;
/// see [`run_cargo`] for cargo.
pub fn run_captured_with_env(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<CapturedOutput, DevError> {
    // Route through the deadline+observed runner so the shutdown-flag
    // poll covers every captured child too. `Duration::MAX`
    // disables the deadline branch in practice (kernel pid lifetime is
    // measured in hours, this in eons).
    let dc =
        run_captured_with_env_and_deadline(program, args, cwd, env, Duration::MAX, None, false)?;
    Ok(dc.captured)
}

/// [`run_captured_with_env_and_deadline`] keeping at most `cap` bytes of each
/// stream, for a caller whose own contract bounds the output it accepts (the
/// `[cargo_env]` evaluator): a runaway child costs that much memory and no
/// more. Plain profile.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_captured_bounded(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
    cap: CaptureCap,
) -> Result<DeadlineCapture, DevError> {
    run_captured_limited(
        program,
        args,
        cwd,
        env,
        ChildLimit::Wall(deadline),
        on_spawn,
        isolate_pg,
        SpawnProfile::Plain,
        cap,
    )
}

/// Captured output plus whether output was truncated after process exit.
///
/// Returned by the wall-time and idle-time captured runners. Regular captured
/// output paths cannot trigger a deadline and keep their plain return type.
pub struct DeadlineCapture {
    pub captured: CapturedOutput,
    /// `true` when the child exited but its output did not close in time and
    /// capture was stopped (see `settle_drains`): `stdout`/`stderr` may be
    /// truncated, whatever ending was observed. A caller whose verdict reads
    /// the output must not trust it when this is set.
    pub output_cut: bool,
}

impl DeadlineCapture {
    /// Whether the supervisor killed the child at its wall or idle limit.
    pub fn killed_on_deadline(&self) -> bool {
        self.captured.ending.is_deadline()
    }
}

/// How often the shared supervisor polls a child with no sidecar observer.
const DEADLINE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A limit applied by the shared child supervisor.
#[derive(Clone, Copy)]
pub(crate) enum ChildLimit {
    None,
    Wall(Duration),
    Idle(Duration),
}

/// Output and termination data produced by the one child supervisor.
pub(crate) struct SupervisedChild {
    pub ending: Ending,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
    pub output_cut: bool,
}

/// Spawn a subprocess with captured stdio and a wall-clock deadline.
///
/// The shared supervisor owns polling, signal forwarding, deadline kills,
/// stdout/stderr drains, and their bounded settle period.
pub fn run_captured_with_env_and_deadline(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
) -> Result<DeadlineCapture, DevError> {
    run_captured_limited(
        program,
        args,
        cwd,
        env,
        ChildLimit::Wall(deadline),
        on_spawn,
        isolate_pg,
        SpawnProfile::Plain,
        CaptureCap::UNBOUNDED,
    )
}

/// As run_captured_with_env_and_deadline, but the limit is on inactivity, and
/// the caller names the spawn profile (a cargo launch passes the cargo one).
pub fn run_captured_with_idle_deadline(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    idle: Duration,
    isolate_pg: bool,
    profile: SpawnProfile,
) -> Result<DeadlineCapture, DevError> {
    run_captured_limited(
        program,
        args,
        cwd,
        env,
        ChildLimit::Idle(idle),
        None,
        isolate_pg,
        profile,
        CaptureCap::UNBOUNDED,
    )
}

/// Read a pipe on a cancellable thread, stamping output time for idle limits.
///
/// At most `cap` bytes are kept; the pipe is still read to its end past the
/// cap, so a chatty child never blocks on a full pipe, but its memory cost is
/// bounded. A caller that needs to know a stream overflowed passes a cap one
/// past its own limit and compares the length.
fn drain_stamped<R>(
    pipe: R,
    start: Instant,
    last_output_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    cap: usize,
) -> Drain
where
    R: std::io::Read + std::os::fd::AsRawFd + Send + 'static,
{
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (buf_t, cancel_t) = (std::sync::Arc::clone(&buf), std::sync::Arc::clone(&cancel));
    let handle = std::thread::spawn(move || {
        let mut reader = pipe;
        let mut chunk = [0u8; 8192];
        while let Some(n) =
            crate::fs_probe::read_unless_cancelled(&mut reader, &mut chunk, &cancel_t)
        {
            if let Ok(mut b) = buf_t.lock() {
                let room = cap.saturating_sub(b.len());
                let take = n.min(room);
                b.extend_from_slice(chunk.get(..take).unwrap_or_default());
            }
            let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            last_output_ms.store(now, Ordering::Relaxed);
        }
    });
    Drain {
        buf,
        cancel,
        handle,
    }
}

struct Drain {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

const DRAIN_SETTLE: Duration = Duration::from_secs(2);

fn settle_drains(drains: Vec<Option<Drain>>, label: &str) -> (Vec<Vec<u8>>, bool) {
    let until = Instant::now() + DRAIN_SETTLE;
    while Instant::now() < until
        && !drains
            .iter()
            .flatten()
            .all(|drain| drain.handle.is_finished())
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let cut = drains
        .iter()
        .flatten()
        .any(|drain| !drain.handle.is_finished());
    if cut {
        super::console::warn(&format!(
            "{label} exited but its output did not close within {}s - something it started \
             still holds the pipe; capture stopped there",
            DRAIN_SETTLE.as_secs()
        ));
    }
    let bufs = drains
        .into_iter()
        .map(|drain| {
            let Some(drain) = drain else {
                return Vec::new();
            };
            drain.cancel.store(true, Ordering::Release);
            drain.handle.join().ok();
            drain.buf.lock().map(|b| b.clone()).unwrap_or_default()
        })
        .collect();
    (bufs, cut)
}

/// Supervise a spawned child. The observer runs once before each child poll and
/// may request an intentional stop-marker ending. All other endings, kills,
/// waiting, and pipe draining stay here.
pub(crate) fn supervise_child(
    child: std::process::Child,
    label: &str,
    start: Instant,
    limit: ChildLimit,
    isolate_pg: bool,
    tick_interval: Duration,
    observe: impl FnMut() -> Option<String>,
) -> SupervisedChild {
    supervise_child_capped(
        child,
        label,
        start,
        limit,
        isolate_pg,
        tick_interval,
        CaptureCap::UNBOUNDED,
        observe,
    )
}

/// How many bytes of each stream a captured run keeps.
#[derive(Clone, Copy)]
pub(crate) struct CaptureCap {
    pub stdout: usize,
    pub stderr: usize,
}

impl CaptureCap {
    pub(crate) const UNBOUNDED: Self = Self {
        stdout: usize::MAX,
        stderr: usize::MAX,
    };
}

#[allow(clippy::too_many_arguments)] // the supervisor's whole contract, one call
fn supervise_child_capped(
    mut child: std::process::Child,
    label: &str,
    start: Instant,
    limit: ChildLimit,
    isolate_pg: bool,
    tick_interval: Duration,
    cap: CaptureCap,
    mut observe: impl FnMut() -> Option<String>,
) -> SupervisedChild {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    let last_output_ms = Arc::new(AtomicU64::new(0));
    let stdout_thread = child
        .stdout
        .take()
        .map(|pipe| drain_stamped(pipe, start, Arc::clone(&last_output_ms), cap.stdout));
    let stderr_thread = child
        .stderr
        .take()
        .map(|pipe| drain_stamped(pipe, start, Arc::clone(&last_output_ms), cap.stderr));

    let expired = || match limit {
        ChildLimit::None => None,
        ChildLimit::Wall(deadline) => (start.elapsed() >= deadline).then_some(deadline),
        ChildLimit::Idle(idle) => {
            let last = Duration::from_millis(last_output_ms.load(Ordering::Relaxed));
            (start.elapsed().saturating_sub(last) >= idle).then_some(idle)
        }
    };

    #[allow(unused_assignments)] // every ending path stamps its observation time
    let mut child_elapsed = None;
    let mut next_tick = Instant::now();
    let ending = loop {
        if let Some(marker) = observe() {
            terminate_child(&mut child, isolate_pg, false);
            let ending = child.wait().map_or_else(
                |error| Ending::WaitFailed(error.to_string()),
                |_| Ending::StopMarker(marker),
            );
            child_elapsed = Some(start.elapsed());
            break ending;
        }

        if crate::shutdown::is_shutdown_requested() {
            terminate_child(&mut child, isolate_pg, true);
            let ending = child.wait().map_or_else(
                |error| Ending::WaitFailed(error.to_string()),
                |_| Ending::Interrupted,
            );
            child_elapsed = Some(start.elapsed());
            break ending;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                child_elapsed = Some(start.elapsed());
                break Ending::from_status(&status).unwrap_or_else(|| {
                    Ending::WaitFailed("wait returned no exit code or signal".to_owned())
                });
            }
            Ok(None) => {
                if let Some(duration) = expired() {
                    terminate_child(&mut child, isolate_pg, false);
                    let ending = child.wait().map_or_else(
                        |error| Ending::WaitFailed(error.to_string()),
                        |_| Ending::Deadline(duration),
                    );
                    child_elapsed = Some(start.elapsed());
                    break ending;
                }
            }
            Err(error) => {
                terminate_child(&mut child, isolate_pg, false);
                child.wait().ok();
                child_elapsed = Some(start.elapsed());
                break Ending::WaitFailed(error.to_string());
            }
        }

        next_tick += tick_interval;
        std::thread::sleep(next_tick.saturating_duration_since(Instant::now()));
    };

    let drains = vec![stdout_thread, stderr_thread];
    let (bufs, output_cut) = settle_drains(drains, label);
    let mut bufs = bufs.into_iter();
    SupervisedChild {
        ending,
        stdout: bufs.next().unwrap_or_default(),
        stderr: bufs.next().unwrap_or_default(),
        elapsed: child_elapsed.unwrap_or_else(|| start.elapsed()),
        output_cut,
    }
}

/// Shared body of the captured runners.
#[allow(clippy::too_many_arguments)]
fn run_captured_limited(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    limit: ChildLimit,
    on_spawn: Option<&dyn Fn(u32)>,
    isolate_pg: bool,
    profile: SpawnProfile,
    cap: CaptureCap,
) -> Result<DeadlineCapture, DevError> {
    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::cargo_env::apply(&mut cmd, profile, args, Some(cwd), env)?;
    crate::oom::protect_child(&mut cmd);
    crate::hold::stamp(&mut cmd);
    use std::os::unix::process::CommandExt;
    if isolate_pg {
        cmd.process_group(0);
    }

    let child = cmd.spawn().map_err(|error| DevError::Spawn {
        program: program.to_owned(),
        error,
    })?;
    if let Some(cb) = on_spawn {
        cb(child.id());
    }
    let run = supervise_child_capped(
        child,
        program,
        start,
        limit,
        isolate_pg,
        DEADLINE_POLL_INTERVAL,
        cap,
        || None,
    );
    let classification = verdict(&run.ending, program, &run.stderr, &[]);
    match (&run.ending, classification) {
        (_, IterationVerdict::Interrupted) => return Err(DevError::Interrupted),
        (Ending::WaitFailed(_), IterationVerdict::Failed(error)) => return Err(error),
        _ => {}
    }
    Ok(DeadlineCapture {
        captured: CapturedOutput {
            ending: run.ending,
            stdout: run.stdout,
            stderr: run.stderr,
            elapsed: run.elapsed,
        },
        output_cut: run.output_cut,
    })
}

/// End a child with SIGKILL, optionally forwarding SIGTERM first.
fn terminate_child(child: &mut std::process::Child, isolate_pg: bool, forward_term: bool) {
    let pid = child.id();
    if forward_term {
        if isolate_pg {
            crate::process::signal_process_group(pid, libc::SIGTERM).ok();
        } else {
            crate::process::signal_pid(pid, libc::SIGTERM).ok();
        }
        let term_sent = Instant::now();
        while term_sent.elapsed() < SIGTERM_FORWARD_BUDGET {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(DEADLINE_POLL_INTERVAL),
                Err(_) => break,
            }
        }
    }
    if isolate_pg {
        crate::process::signal_process_group(pid, libc::SIGKILL).ok();
    }
    child.kill().ok();
}

/// How long to give a captured child to honour SIGTERM after `brokkr kill`
/// before we escalate to SIGKILL. Matches sæhrimnir's
/// [`crate::ratatoskr::saehrimnir::SHUTDOWN_BUDGET`] in spirit: long enough
/// for cooperative cleanup, short enough that the user doesn't think
/// `brokkr kill` hung.
const SIGTERM_FORWARD_BUDGET: Duration = Duration::from_millis(1500);

/// Spawn a subprocess with captured stdio, returning the `Child` handle.
///
/// The caller is responsible for waiting on the child and collecting output.
/// Used by the sidecar to run sampling alongside the child process.
pub fn spawn_captured(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    isolate_pg: bool,
    profile: SpawnProfile,
) -> Result<std::process::Child, DevError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::cargo_env::apply(&mut cmd, profile, args, Some(cwd), env)?;
    crate::oom::protect_child(&mut cmd);
    // Last, after every caller-supplied env var, so a caller cannot displace
    // the capability that lets this child compile under the hold. See
    // `crate::hold` for why the mark is inherited rather than inferred, and why
    // it is stamped on every child rather than only the ones named cargo.
    crate::hold::stamp(&mut cmd);
    // PG isolation is opt-in for the same reason as the deadline
    // runner: the caller asserts a SigtermGuard is active so terminal
    // signals bridge to the PG. The sidecar's own SigtermGuard (around
    // `run_sidecar_with_deadline`) is the typical pairing.
    use std::os::unix::process::CommandExt;
    if isolate_pg {
        cmd.process_group(0);
    }

    cmd.spawn().map_err(|error| DevError::Spawn {
        program: program.to_owned(),
        error,
    })
}

/// Run a subprocess with inherited stdio (passthrough mode), returning timing.
///
/// If the process is killed by a signal (e.g. OOM killer SIGKILL), returns a
/// `DevError::Subprocess` with the signal number instead of silently mapping
/// to exit code 1.
pub fn run_passthrough_timed(
    program: &str,
    args: &[&str],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<PassthroughOutput, DevError> {
    run_passthrough_in(program, args, None, &[], SpawnProfile::Plain, lock)
}

/// [`run_passthrough_timed`] with a working directory and environment.
///
/// Same lifecycle guarantees - that is the point of having it. A caller needing
/// to set `cwd` or an env var previously had to reach for a bare
/// `Command::status()`, which silently gave up the `SigtermGuard`, the OOM
/// marking and the lockfile PID publication. `brokkr bench` did exactly that.
pub fn run_passthrough_in(
    program: &str,
    args: &[&str],
    dir: Option<&std::path::Path>,
    env: &[(&str, &str)],
    profile: SpawnProfile,
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<PassthroughOutput, DevError> {
    let start = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    // A passthrough child (elivagar `regress`, elivagar run-mode dispatch) is a
    // real, long-running workload with no sidecar window around it. Historically
    // this used a bare `cmd.status()`, which left brokkr with the default
    // SIGTERM disposition: a `brokkr kill` (SIGTERM to brokkr's PID only) then
    // terminated brokkr and orphaned the child, which kept running. Install a
    // `SigtermGuard` and spawn+poll instead so a `brokkr kill` (or terminal
    // ctrl-C) reaches the child. The child stays in brokkr's process group -
    // not isolated - so a terminal SIGINT still hits it directly, and the guard
    // covers the `brokkr kill` case where only brokkr's PID is signalled.
    // Installed before the `[cargo_env]` evaluation, which polls the flag too.
    let _guard = crate::shutdown::SigtermGuard::install();
    crate::cargo_env::apply(&mut cmd, profile, args, dir, env)?;
    crate::oom::protect_child(&mut cmd);
    // Last, after every caller-supplied env var, so a caller cannot displace
    // the capability that lets this child compile under the hold. See
    // `crate::hold` for why the mark is inherited rather than inferred, and why
    // it is stamped on every child rather than only the ones named cargo.
    crate::hold::stamp(&mut cmd);
    let child = cmd.spawn().map_err(|error| DevError::Spawn {
        program: program.to_owned(),
        error,
    })?;
    // Publish the child PID into the lockfile so `brokkr kill --hard` (which
    // reads child_pid from the lock file rather than holding the Child handle)
    // can SIGKILL it, and `brokkr lock` can show it. Cleared on every exit path
    // below so a recycled PID can't be killed after this returns.
    if let Some(lock) = lock {
        lock.set_child_pid(child.id());
    }

    let run = supervise_child(
        child,
        program,
        start,
        ChildLimit::None,
        false,
        DEADLINE_POLL_INTERVAL,
        || None,
    );
    if let Some(lock) = lock {
        lock.clear_child_pid();
    }
    match run.ending {
        ending @ Ending::Exited(_) => Ok(PassthroughOutput {
            ending,
            elapsed: run.elapsed,
        }),
        Ending::Interrupted => Err(DevError::Interrupted),
        ending => match verdict(&ending, program, &run.stderr, &[]) {
            IterationVerdict::Failed(error) => Err(error),
            IterationVerdict::Interrupted => Err(DevError::Interrupted),
            IterationVerdict::Counted => Ok(PassthroughOutput {
                ending,
                elapsed: run.elapsed,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::Path;
    use std::time::Duration;

    use super::*;

    fn cwd() -> &'static Path {
        Path::new(".")
    }

    /// A child that keeps printing outlives a wall time several idle windows
    /// long; one that goes quiet is killed one window after its last output.
    #[test]
    fn idle_deadline_resets_on_output_and_fires_on_silence() {
        let idle = Duration::from_millis(600);
        let chatty = run_captured_with_idle_deadline(
            "sh",
            &[
                "-c",
                "for i in 1 2 3 4 5 6 7 8; do echo $i; sleep 0.2; done",
            ],
            cwd(),
            &[],
            idle,
            true,
            SpawnProfile::Plain,
        )
        .unwrap();
        assert!(
            !chatty.captured.ending.is_deadline(),
            "progress must keep resetting the clock"
        );
        assert_eq!(chatty.captured.ending, Ending::Exited(0));
        assert!(chatty.captured.elapsed > idle);

        let quiet = run_captured_with_idle_deadline(
            "sh",
            &["-c", "echo start; sleep 30"],
            cwd(),
            &[],
            idle,
            true,
            SpawnProfile::Plain,
        )
        .unwrap();
        assert!(quiet.captured.ending.is_deadline());
        assert!(quiet.captured.elapsed < Duration::from_secs(10));
        assert_eq!(
            String::from_utf8_lossy(&quiet.captured.stdout).trim(),
            "start"
        );
    }

    #[test]
    fn deadline_lets_short_runs_finish_normally() {
        let result = run_captured_with_env_and_deadline(
            "/bin/true",
            &[],
            cwd(),
            &[],
            Duration::from_secs(5),
            None,
            false,
        )
        .unwrap();
        assert!(!result.captured.ending.is_deadline());
        assert_eq!(result.captured.ending, Ending::Exited(0));
    }

    #[test]
    fn deadline_kills_runaway_child() {
        // /bin/sleep 30 will outlive a 250 ms deadline by orders of
        // magnitude; brokkr should reap it quickly.
        let start = std::time::Instant::now();
        let result = run_captured_with_env_and_deadline(
            "/bin/sleep",
            &["30"],
            cwd(),
            &[],
            Duration::from_millis(250),
            None,
            false,
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(result.captured.ending.is_deadline());
        assert_eq!(
            result.captured.ending,
            Ending::Deadline(Duration::from_millis(250))
        );
        // Should finish well inside one poll interval after the deadline,
        // plus a healthy slack budget for slow CI hardware.
        assert!(
            elapsed < Duration::from_secs(5),
            "deadline kill took too long: {elapsed:?}"
        );
    }

    #[test]
    fn deadline_captures_stdout_from_short_run() {
        let result = run_captured_with_env_and_deadline(
            "/bin/echo",
            &["hello", "world"],
            cwd(),
            &[],
            Duration::from_secs(5),
            None,
            false,
        )
        .unwrap();
        assert!(!result.captured.ending.is_deadline());
        assert_eq!(result.captured.stdout, b"hello world\n");
    }
}
