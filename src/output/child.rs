use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub use crate::cargo_env::SpawnProfile;
use crate::error::DevError;
use crate::shutdown::ChildIsolation;

const DEFAULT_CAPTURE_BYTES: usize = 16 * 1024 * 1024;
const LARGE_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
const CARGO_CAPTURE_BYTES: usize = 128 * 1024 * 1024;

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
        Ending::Deadline(limit) => IterationVerdict::Failed(DevError::ChildFailure {
            program: program.to_owned(),
            failure: crate::error::ChildFailureKind::Deadline(*limit),
        }),
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

    /// Return `Ok(())` if the process exited successfully, a
    /// `DevError::Subprocess` for an unsuccessful exit, or `ChildFailure` for
    /// a supervised deadline.
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
/// Returns `DevError::Spawn` if the process could not be spawned, or a
/// `DevError::ChildFailure` if capture exceeded its bound or did not close.
///
/// A plain child: `[cargo_env]` is never evaluated for it. A cargo launch goes
/// through [`run_cargo`], which says so explicitly.
pub fn run_captured(program: &str, args: &[&str], cwd: &Path) -> Result<CapturedOutput, DevError> {
    run_captured_with_env(program, args, cwd, &[])
}

/// Capture a plain child while replaying selected streams as pipe chunks arrive.
/// Captured bytes and ending classification retain the ordinary capture contract.
pub fn run_streamed(
    program: &str,
    args: &[&str],
    cwd: &Path,
    replay: Replay,
) -> Result<CapturedOutput, DevError> {
    let options = ChildOptions {
        replay,
        ..ChildOptions::DEFAULT
    };
    let dc = run_captured_limited(
        program,
        args,
        cwd,
        &[],
        ChildLimit::None,
        None,
        ChildIsolation::Inherit,
        SpawnProfile::Plain,
        options,
        false,
    )?;
    captured_without_incomplete_output(program, dc, options)
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
        ChildIsolation::Inherit,
        SpawnProfile::Cargo { phase },
        ChildOptions::CARGO,
        false,
    )?;
    captured_without_incomplete_output("cargo", dc, ChildOptions::CARGO)
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
    isolation: ChildIsolation<'_>,
    profile: SpawnProfile,
) -> Result<CapturedOutput, DevError> {
    let dc = run_captured_limited(
        program,
        args,
        cwd,
        env,
        ChildLimit::Wall(Duration::MAX),
        on_spawn,
        isolation,
        profile,
        ChildOptions::CARGO,
        false,
    )?;
    captured_without_incomplete_output(program, dc, ChildOptions::CARGO)
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
    let dc = run_captured_with_env_and_deadline(
        program,
        args,
        cwd,
        env,
        Duration::MAX,
        None,
        ChildIsolation::Inherit,
    )?;
    captured_without_incomplete_output(program, dc, ChildOptions::DEFAULT)
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
    isolation: ChildIsolation<'_>,
    cap: CaptureCap,
) -> Result<DeadlineCapture, DevError> {
    run_captured_bounded_unsetting(
        program,
        args,
        cwd,
        env,
        &[],
        deadline,
        on_spawn,
        isolation,
        cap,
    )
}

/// [`run_captured_bounded`] with `unset` removed from the inherited
/// environment, for a child whose contract says a variable must not reach it
/// (the library harness's describe mode, which must see no marker FIFO).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_captured_bounded_unsetting(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    unset: &[&str],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolation: ChildIsolation<'_>,
    cap: CaptureCap,
) -> Result<DeadlineCapture, DevError> {
    let options = ChildOptions {
        capture_cap: cap,
        ..ChildOptions::DEFAULT
    };
    run_captured_limited_unsetting(
        program,
        args,
        cwd,
        env,
        unset,
        ChildLimit::Wall(deadline),
        on_spawn,
        isolation,
        SpawnProfile::Plain,
        options,
        true,
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
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// Spawn a subprocess with captured stdio and a wall-clock deadline.
///
/// The shared supervisor owns polling, signal forwarding, deadline kills,
/// bounded stdout/stderr drains, and their settle period. An over-limit
/// stream or a pipe that stays open past the grace returns `ChildFailure`.
pub fn run_captured_with_env_and_deadline(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolation: ChildIsolation<'_>,
) -> Result<DeadlineCapture, DevError> {
    run_captured_with_env_and_deadline_with_options(
        program,
        args,
        cwd,
        env,
        deadline,
        on_spawn,
        isolation,
        ChildOptions::DEFAULT,
    )
}

/// As [`run_captured_with_env_and_deadline`], with an explicit capture bound
/// and supervision grace periods for callers with a different output or
/// shutdown contract.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_captured_with_env_and_deadline_with_options(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolation: ChildIsolation<'_>,
    options: ChildOptions,
) -> Result<DeadlineCapture, DevError> {
    let capture = run_captured_limited(
        program,
        args,
        cwd,
        env,
        ChildLimit::Wall(deadline),
        on_spawn,
        isolation,
        SpawnProfile::Plain,
        options,
        false,
    )?;
    deadline_capture_without_incomplete_output(program, capture, options)
}

/// Like [`run_captured_with_env_and_deadline_with_options`], but returns a
/// capture whose output pipe stayed open past the drain grace. This is for
/// callers that record incomplete output as part of their own integrity
/// assessment.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_captured_with_env_and_deadline_allow_output_cut(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    deadline: Duration,
    on_spawn: Option<&dyn Fn(u32)>,
    isolation: ChildIsolation<'_>,
    options: ChildOptions,
) -> Result<DeadlineCapture, DevError> {
    run_captured_limited(
        program,
        args,
        cwd,
        env,
        ChildLimit::Wall(deadline),
        on_spawn,
        isolation,
        SpawnProfile::Plain,
        options,
        false,
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
    isolation: ChildIsolation<'_>,
    profile: SpawnProfile,
) -> Result<DeadlineCapture, DevError> {
    run_captured_limited(
        program,
        args,
        cwd,
        env,
        ChildLimit::Idle(idle),
        None,
        isolation,
        profile,
        ChildOptions::CARGO,
        false,
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
    replay: Option<fn(&[u8])>,
) -> Drain
where
    R: std::io::Read + std::os::fd::AsRawFd + Send + 'static,
{
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let truncated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (buf_t, cancel_t, truncated_t) = (
        std::sync::Arc::clone(&buf),
        std::sync::Arc::clone(&cancel),
        std::sync::Arc::clone(&truncated),
    );
    let handle = std::thread::spawn(move || {
        let mut reader = pipe;
        let mut chunk = [0u8; 8192];
        while let Some(n) =
            crate::fs_probe::read_unless_cancelled(&mut reader, &mut chunk, &cancel_t)
        {
            if let Some(replay) = replay {
                replay(&chunk[..n]);
            }
            match buf_t.lock() {
                Ok(mut b) => {
                    let room = cap.saturating_sub(b.len());
                    let take = n.min(room);
                    b.extend_from_slice(chunk.get(..take).unwrap_or_default());
                    if take < n {
                        truncated_t.store(true, Ordering::Release);
                    }
                }
                Err(_) => truncated_t.store(true, Ordering::Release),
            }
            let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            last_output_ms.store(now, Ordering::Relaxed);
        }
    });
    Drain {
        buf,
        cancel,
        truncated,
        handle,
    }
}

struct Drain {
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    truncated: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

#[derive(Default)]
struct DrainOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

const DRAIN_SETTLE: Duration = Duration::from_secs(2);

fn settle_drains(
    drains: Vec<Option<Drain>>,
    label: &str,
    drain_grace: Duration,
    poll_interval: Duration,
) -> (Vec<DrainOutput>, bool) {
    let until = Instant::now() + drain_grace;
    while Instant::now() < until
        && !drains
            .iter()
            .flatten()
            .all(|drain| drain.handle.is_finished())
    {
        std::thread::sleep(Duration::from_millis(10).min(poll_interval));
    }
    let cut = drains
        .iter()
        .flatten()
        .any(|drain| !drain.handle.is_finished());
    if cut {
        super::console::warn(&format!(
            "{label} exited but its output did not close within {} - something it started \
             still holds the pipe; capture stopped there",
            super::console::format_duration(drain_grace)
        ));
    }
    let outputs = drains
        .into_iter()
        .map(|drain| {
            let Some(drain) = drain else {
                return DrainOutput::default();
            };
            drain.cancel.store(true, Ordering::Release);
            drain.handle.join().ok();
            DrainOutput {
                bytes: drain.buf.lock().map(|b| b.clone()).unwrap_or_default(),
                truncated: drain.truncated.load(Ordering::Acquire),
            }
        })
        .collect();
    (outputs, cut)
}

/// Supervise a spawned child. The observer runs once before each child poll and
/// may request an intentional stop-marker ending. All other endings, kills,
/// waiting, and pipe draining stay here.
pub(crate) fn supervise_child(
    child: std::process::Child,
    label: &str,
    start: Instant,
    limit: ChildLimit,
    isolation: ChildIsolation<'_>,
    options: ChildOptions,
    observe: impl FnMut() -> Option<String>,
) -> SupervisedChild {
    supervise_child_capped(child, label, start, limit, isolation, options, observe)
}

/// How many bytes of each stream a captured run keeps.
#[derive(Clone, Copy)]
pub(crate) struct CaptureCap {
    pub stdout: usize,
    pub stderr: usize,
}

impl CaptureCap {
    pub(crate) const DEFAULT: Self = Self {
        stdout: DEFAULT_CAPTURE_BYTES,
        stderr: DEFAULT_CAPTURE_BYTES,
    };

    pub(crate) const LARGE: Self = Self {
        stdout: LARGE_CAPTURE_BYTES,
        stderr: LARGE_CAPTURE_BYTES,
    };

    pub(crate) const CARGO: Self = Self {
        stdout: CARGO_CAPTURE_BYTES,
        stderr: CARGO_CAPTURE_BYTES,
    };

    pub(crate) const MEASUREMENT: Self = Self {
        stdout: DEFAULT_CAPTURE_BYTES,
        stderr: DEFAULT_CAPTURE_BYTES,
    };
}

/// Which captured child streams are also replayed live by the console channel.
#[derive(Clone, Copy)]
pub(crate) enum Replay {
    None,
    Stderr,
    Both,
}

impl Replay {
    fn stdout_printer(self) -> Option<fn(&[u8])> {
        match self {
            Self::Both => Some(super::console::raw_stdout_bytes),
            Self::None | Self::Stderr => None,
        }
    }

    fn stderr_printer(self) -> Option<fn(&[u8])> {
        match self {
            Self::Both | Self::Stderr => Some(super::console::raw_stderr_bytes),
            Self::None => None,
        }
    }
}

/// Per-run capture and shutdown policy for the shared child supervisor.
/// Callers that own a separate child-lifecycle budget can override the grace
/// periods here instead of inheriting the shared defaults.
#[derive(Clone, Copy)]
pub(crate) struct ChildOptions {
    pub replay: Replay,
    pub capture_cap: CaptureCap,
    pub poll_interval: Duration,
    pub sigterm_grace: Duration,
    pub drain_grace: Duration,
}

impl ChildOptions {
    pub(crate) const DEFAULT: Self = Self {
        replay: Replay::None,
        capture_cap: CaptureCap::DEFAULT,
        poll_interval: DEADLINE_POLL_INTERVAL,
        sigterm_grace: SIGTERM_FORWARD_BUDGET,
        drain_grace: DRAIN_SETTLE,
    };

    pub(crate) const CARGO: Self = Self {
        capture_cap: CaptureCap::CARGO,
        ..Self::DEFAULT
    };

    pub(crate) const LARGE: Self = Self {
        capture_cap: CaptureCap::LARGE,
        ..Self::DEFAULT
    };

    pub(crate) const MEASUREMENT: Self = Self {
        capture_cap: CaptureCap::MEASUREMENT,
        ..Self::DEFAULT
    };
}

/// Start the two captured pipe readers under one replay and capture policy.
fn start_drains(
    child: &mut std::process::Child,
    start: Instant,
    options: ChildOptions,
    last_output_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> (Option<Drain>, Option<Drain>) {
    let stdout_thread = child.stdout.take().map(|pipe| {
        drain_stamped(
            pipe,
            start,
            std::sync::Arc::clone(last_output_ms),
            options.capture_cap.stdout,
            options.replay.stdout_printer(),
        )
    });
    let stderr_thread = child.stderr.take().map(|pipe| {
        drain_stamped(
            pipe,
            start,
            std::sync::Arc::clone(last_output_ms),
            options.capture_cap.stderr,
            options.replay.stderr_printer(),
        )
    });

    (stdout_thread, stderr_thread)
}

#[allow(clippy::too_many_arguments)] // the supervisor's whole contract, one call
fn supervise_child_capped(
    mut child: std::process::Child,
    label: &str,
    start: Instant,
    limit: ChildLimit,
    isolation: ChildIsolation<'_>,
    options: ChildOptions,
    mut observe: impl FnMut() -> Option<String>,
) -> SupervisedChild {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    let last_output_ms = Arc::new(AtomicU64::new(0));
    let (stdout_thread, stderr_thread) = start_drains(&mut child, start, options, &last_output_ms);

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
            let ending = terminate_and_wait(
                &mut child,
                isolation,
                false,
                &options,
                Ending::StopMarker(marker),
            );
            child_elapsed = Some(start.elapsed());
            break ending;
        }

        if crate::shutdown::is_shutdown_requested() {
            let ending =
                terminate_and_wait(&mut child, isolation, true, &options, Ending::Interrupted);
            child_elapsed = Some(start.elapsed());
            break ending;
        }

        // Reaped before the expiry test on purpose: a wall limit is a kill
        // ceiling, and a child found exited after the limit (brokkr
        // descheduled across it) may well have exited before it, which no
        // later reading can rule out. Its own ending stands.
        match child.try_wait() {
            Ok(Some(status)) => {
                child_elapsed = Some(start.elapsed());
                break Ending::from_status(&status).unwrap_or_else(|| {
                    Ending::WaitFailed("wait returned no exit code or signal".to_owned())
                });
            }
            Ok(None) => {
                if let Some(duration) = expired() {
                    let ending = terminate_and_wait(
                        &mut child,
                        isolation,
                        false,
                        &options,
                        Ending::Deadline(duration),
                    );
                    child_elapsed = Some(start.elapsed());
                    break ending;
                }
            }
            Err(error) => {
                terminate_child(
                    &mut child,
                    isolation,
                    false,
                    options.sigterm_grace,
                    options.poll_interval,
                );
                child.wait().ok();
                child_elapsed = Some(start.elapsed());
                break Ending::WaitFailed(error.to_string());
            }
        }

        next_tick += options.poll_interval;
        std::thread::sleep(next_tick.saturating_duration_since(Instant::now()));
    };

    let drains = vec![stdout_thread, stderr_thread];
    let (bufs, output_cut) =
        settle_drains(drains, label, options.drain_grace, options.poll_interval);
    let mut bufs = bufs.into_iter();
    let stdout = bufs.next().unwrap_or_default();
    let stderr = bufs.next().unwrap_or_default();
    SupervisedChild {
        ending,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        elapsed: child_elapsed.unwrap_or_else(|| start.elapsed()),
        output_cut,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    }
}

/// [`run_cargo`], with `unset` removed from the inherited environment - for a
/// caller whose question is what cargo does when a variable is ABSENT, which
/// an empty value does not answer. `[cargo_env]` still applies.
pub(crate) fn run_cargo_unsetting(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    unset: &[&str],
    phase: Option<crate::cargo_env::Phase>,
) -> Result<CapturedOutput, DevError> {
    let dc = run_captured_limited_unsetting(
        "cargo",
        args,
        cwd,
        env,
        unset,
        ChildLimit::Wall(Duration::MAX),
        None,
        ChildIsolation::Inherit,
        SpawnProfile::Cargo { phase },
        ChildOptions::CARGO,
        false,
    )?;
    captured_without_incomplete_output("cargo", dc, ChildOptions::CARGO)
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
    isolation: ChildIsolation<'_>,
    profile: SpawnProfile,
    options: ChildOptions,
    allow_capture_truncation: bool,
) -> Result<DeadlineCapture, DevError> {
    run_captured_limited_unsetting(
        program,
        args,
        cwd,
        env,
        &[],
        limit,
        on_spawn,
        isolation,
        profile,
        options,
        allow_capture_truncation,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_captured_limited_unsetting(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    unset: &[&str],
    limit: ChildLimit,
    on_spawn: Option<&dyn Fn(u32)>,
    isolation: ChildIsolation<'_>,
    profile: SpawnProfile,
    options: ChildOptions,
    allow_capture_truncation: bool,
) -> Result<DeadlineCapture, DevError> {
    let start = Instant::now();
    let policy = crate::process::spawn::ChildPolicy::from_cargo_profile(profile);
    let mut cmd = policy.command(program, args, Some(cwd), env, isolation)?;
    for name in unset {
        cmd.env_remove(name);
    }
    crate::process::spawn::capture_stdio(&mut cmd);
    let child = crate::process::spawn::spawn(cmd, on_spawn)?;
    let run = supervise_child_capped(child, program, start, limit, isolation, options, || None);
    let classification = verdict(&run.ending, program, &run.stderr, &[]);
    match (&run.ending, classification) {
        (_, IterationVerdict::Interrupted) => return Err(DevError::Interrupted),
        (Ending::WaitFailed(_), IterationVerdict::Failed(error)) => return Err(error),
        _ => {}
    }
    if !allow_capture_truncation {
        let failure = if run.stdout_truncated {
            Some(crate::error::ChildFailureKind::CaptureLimit {
                stream: "stdout",
                limit: options.capture_cap.stdout,
            })
        } else if run.stderr_truncated {
            Some(crate::error::ChildFailureKind::CaptureLimit {
                stream: "stderr",
                limit: options.capture_cap.stderr,
            })
        } else {
            None
        };
        if let Some(failure) = failure {
            return Err(DevError::ChildFailure {
                program: program.to_owned(),
                failure,
            });
        }
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

/// A run whose stdout went to a file, not a pipe: the ending always comes
/// back, with stderr's bounded capture and whether it was cut short reported
/// beside it rather than in place of it.
pub struct FileSinkRun {
    /// `stdout` is empty: the child wrote it to the sink file.
    pub captured: CapturedOutput,
    /// stderr exceeded the default capture cap and was truncated there.
    pub stderr_truncated: bool,
    /// The child exited but its stderr did not close within the drain grace.
    pub output_cut: bool,
}

impl FileSinkRun {
    /// The capture failure a caller that trusts stderr must refuse, if any:
    /// a truncated stream or an output-drain cut. Separate from the ending so
    /// a caller can classify the ending first (a SIGKILL that came after a
    /// stderr flood is still a SIGKILL).
    pub fn capture_failure(&self, program: &str) -> Option<DevError> {
        let failure = if self.stderr_truncated {
            crate::error::ChildFailureKind::CaptureLimit {
                stream: "stderr",
                limit: ChildOptions::DEFAULT.capture_cap.stderr,
            }
        } else if self.output_cut {
            crate::error::ChildFailureKind::OutputDrain {
                grace: ChildOptions::DEFAULT.drain_grace,
            }
        } else {
            return None;
        };
        Some(DevError::ChildFailure {
            program: program.to_owned(),
            failure,
        })
    }
}

/// Run a plain child with its stdout written straight to `stdout` (a file
/// the caller created) and stderr captured under the default cap. For
/// output whose size scales with the data - a diff listing of a planet - so
/// that no byte of it is held in memory.
///
/// The child dies with brokkr (`PR_SET_PDEATHSIG`, refused at spawn when it
/// cannot be armed) and carries the orphan token: unlike a pipe, a file gives
/// a writer that outlives its reader no reason to stop. Nothing here proves
/// the file complete beyond the child having been reaped; a descendant that
/// inherited the descriptor is the caller's to rule out (pbfhogg's launch
/// lease does that for pbfhogg).
pub(crate) fn run_stdout_to_file(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    stdout: std::fs::File,
) -> Result<FileSinkRun, DevError> {
    let start = Instant::now();
    let policy = crate::process::spawn::ChildPolicy::from_cargo_profile(SpawnProfile::Plain);
    let mut cmd = policy.command(program, args, Some(cwd), env, ChildIsolation::Inherit)?;
    crate::process::spawn::capture_stdio(&mut cmd);
    cmd.stdout(std::process::Stdio::from(stdout));
    crate::shutdown::die_with_parent_strict(&mut cmd);
    crate::test_orphans::stamp(&mut cmd);
    let child = crate::process::spawn::spawn(cmd, None)?;
    let options = ChildOptions::DEFAULT;
    let run = supervise_child_capped(
        child,
        program,
        start,
        ChildLimit::None,
        ChildIsolation::Inherit,
        options,
        || None,
    );
    match (&run.ending, verdict(&run.ending, program, &run.stderr, &[])) {
        (_, IterationVerdict::Interrupted) => return Err(DevError::Interrupted),
        (Ending::WaitFailed(_), IterationVerdict::Failed(error)) => return Err(error),
        _ => {}
    }
    Ok(FileSinkRun {
        captured: CapturedOutput {
            ending: run.ending,
            stdout: Vec::new(),
            stderr: run.stderr,
            elapsed: run.elapsed,
        },
        stderr_truncated: run.stderr_truncated,
        output_cut: run.output_cut,
    })
}

fn captured_without_incomplete_output(
    program: &str,
    capture: DeadlineCapture,
    options: ChildOptions,
) -> Result<CapturedOutput, DevError> {
    Ok(deadline_capture_without_incomplete_output(program, capture, options)?.captured)
}

fn deadline_capture_without_incomplete_output(
    program: &str,
    capture: DeadlineCapture,
    options: ChildOptions,
) -> Result<DeadlineCapture, DevError> {
    if capture.output_cut {
        return Err(DevError::ChildFailure {
            program: program.to_owned(),
            failure: crate::error::ChildFailureKind::OutputDrain {
                grace: options.drain_grace,
            },
        });
    }
    Ok(capture)
}

/// End a child with SIGKILL, optionally forwarding SIGTERM first.
/// Terminate the child, wait for it, and report `ending` unless the wait
/// itself failed.
fn terminate_and_wait(
    child: &mut std::process::Child,
    isolation: ChildIsolation<'_>,
    forward_term: bool,
    options: &ChildOptions,
    ending: Ending,
) -> Ending {
    terminate_child(
        child,
        isolation,
        forward_term,
        options.sigterm_grace,
        options.poll_interval,
    );
    child
        .wait()
        .map_or_else(|error| Ending::WaitFailed(error.to_string()), |_| ending)
}

fn terminate_child(
    child: &mut std::process::Child,
    isolation: ChildIsolation<'_>,
    forward_term: bool,
    sigterm_grace: Duration,
    poll_interval: Duration,
) {
    let pid = child.id();
    if forward_term {
        if isolation.is_process_group() {
            crate::process::signal_process_group(pid, libc::SIGTERM).ok();
        } else {
            crate::process::signal_pid(pid, libc::SIGTERM).ok();
        }
        let term_sent = Instant::now();
        while term_sent.elapsed() < sigterm_grace {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(poll_interval),
                Err(_) => break,
            }
        }
    }
    if isolation.is_process_group() {
        crate::process::signal_process_group(pid, libc::SIGKILL).ok();
    }
    child.kill().ok();
}

/// Default time to give a captured child to honour SIGTERM after `brokkr kill`
/// before escalating to SIGKILL. Project runners can pass their own budget
/// through [`ChildOptions`].
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
    isolation: ChildIsolation<'_>,
    profile: SpawnProfile,
) -> Result<std::process::Child, DevError> {
    let policy = crate::process::spawn::ChildPolicy::from_cargo_profile(profile);
    let mut cmd = policy.command(program, args, Some(cwd), env, isolation)?;
    crate::process::spawn::capture_stdio(&mut cmd);
    crate::process::spawn::spawn(cmd, None)
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
    // Install before the `[cargo_env]` evaluation, which polls the flag too.
    let _guard = crate::shutdown::SigtermGuard::install();
    let policy = crate::process::spawn::ChildPolicy::from_cargo_profile(profile);
    let cmd = policy.command(program, args, dir, env, ChildIsolation::Inherit)?;
    // A passthrough child (elivagar `regress`, elivagar run-mode dispatch) is a
    // real, long-running workload with no sidecar window around it. Historically
    // this used a bare `cmd.status()`, which left brokkr with the default
    // SIGTERM disposition: a `brokkr kill` (SIGTERM to brokkr's PID only) then
    // terminated brokkr and orphaned the child, which kept running. Install a
    // `SigtermGuard` and spawn+poll instead so a `brokkr kill` (or terminal
    // ctrl-C) reaches the child. The child stays in brokkr's process group -
    // not isolated - so a terminal SIGINT still hits it directly, and the guard
    // covers the `brokkr kill` case where only brokkr's PID is signalled.
    let child = crate::process::spawn::spawn(cmd, None)?;
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
        ChildIsolation::Inherit,
        ChildOptions::DEFAULT,
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

/// Capture a caller-configured plain command under the shared supervisor.
/// Keep the std::process return shape for byte-oriented probes and git readers.
pub(crate) fn command_output(
    command: &mut std::process::Command,
) -> std::io::Result<std::process::Output> {
    command.stdin(std::process::Stdio::null());
    crate::process::spawn::capture_stdio(command);
    // These include per-request HTTP probes. The workload runner's 50ms
    // cadence would add that delay to every short request in a tiles sample.
    run_prepared_plain(
        command,
        ChildOptions {
            poll_interval: Duration::from_millis(1),
            ..ChildOptions::DEFAULT
        },
    )
}

/// Run a caller-configured plain command with its chosen inherited streams.
pub(crate) fn command_status(command: &mut std::process::Command) -> std::io::Result<ExitStatus> {
    run_prepared_plain(command, ChildOptions::DEFAULT).map(|output| output.status)
}

fn run_prepared_plain(
    command: &mut std::process::Command,
    options: ChildOptions,
) -> std::io::Result<std::process::Output> {
    use std::os::unix::process::ExitStatusExt;
    let _guard = crate::shutdown::SigtermGuard::install();
    let start = Instant::now();
    let label = command.get_program().to_string_lossy().into_owned();
    crate::process::spawn::ChildPolicy::plain().apply_runtime_policy(command);
    // Move the configured command out: the spawn choke point owns it.
    let command = std::mem::replace(
        command,
        crate::process::spawn::ChildPolicy::plain_command(&label),
    );
    let child = crate::process::spawn::spawn(command, None).map_err(|error| match error {
        DevError::Spawn { error, .. } => error,
        error => std::io::Error::other(error.to_string()),
    })?;
    let run = supervise_child(
        child,
        &label,
        start,
        ChildLimit::None,
        ChildIsolation::Inherit,
        options,
        || None,
    );
    if run.output_cut || run.stdout_truncated || run.stderr_truncated {
        return Err(std::io::Error::other(format!(
            "{label}: captured output was incomplete"
        )));
    }
    let status = match run.ending {
        Ending::Exited(code) => ExitStatus::from_raw(code << 8),
        Ending::Signalled(signal) => ExitStatus::from_raw(signal),
        Ending::Interrupted => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "child interrupted",
            ));
        }
        ending => return Err(std::io::Error::other(format!("{label}: {ending:?}"))),
    };
    Ok(std::process::Output {
        status,
        stdout: run.stdout,
        stderr: run.stderr,
    })
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
        let signal_guard = crate::shutdown::SigtermGuard::install();
        let chatty = run_captured_with_idle_deadline(
            "sh",
            &[
                "-c",
                "for i in 1 2 3 4 5 6 7 8; do echo $i; sleep 0.2; done",
            ],
            cwd(),
            &[],
            idle,
            ChildIsolation::ProcessGroup(&signal_guard),
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
            ChildIsolation::ProcessGroup(&signal_guard),
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
            ChildIsolation::Inherit,
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
            ChildIsolation::Inherit,
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
            ChildIsolation::Inherit,
        )
        .unwrap();
        assert!(!result.captured.ending.is_deadline());
        assert_eq!(result.captured.stdout, b"hello world\n");
    }
}
