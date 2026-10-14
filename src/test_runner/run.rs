//! The cargo, direct-binary, parallel, and listing runners.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::harness_shim;
use super::{
    Ceilings, HungTest, KillCause, ObsEvent, ObservationSink, StreamSource, StreamTap, StrictPlan,
    TestTracker, WATCHDOG_POLL, WallShape, clone_buffer, clone_hung, drain_libtest_json,
    drain_stderr, drain_to_buffer, join_drains, kill_process_group, spawn_cargo_process_group,
    spawn_process_group, watchdog_loop,
};
use crate::error::DevError;
use crate::output::CapturedOutput;

/// Whether, and how strictly, a cargo-launched run isolates its harnesses.
#[derive(Clone, Default)]
pub(crate) enum ShimMode {
    /// One shared stream.
    #[default]
    Off,
    /// Isolate what can be isolated; a harness that cannot be runs on the
    /// shared stream, as it always has.
    Permissive,
    /// Fail closed: a cargo child that cannot be authenticated, whose
    /// executable is not in the plan or not the planned content, or a rustdoc
    /// on a run that carries no doctests, is refused rather than run
    /// unisolated - an unattributable stream cannot be accounted.
    Strict(StrictPlan),
}

/// What a caller wants observed: the sink, and the isolation policy.
#[derive(Clone, Default)]
pub(crate) struct Observe {
    pub(crate) sink: Option<ObservationSink>,
    pub(crate) shim: ShimMode,
}

/// What the process a run launched is, which decides where a hang snapshot is
/// taken from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Leader {
    /// cargo, whose first child is the test harness.
    Cargo,
    /// The test binary itself, executed directly. Its children are whatever the
    /// tests spawned - snapshotting the first of them examined a test's
    /// subprocess instead of the hung harness.
    TestBinary,
}

/// What [`streaming_run_libtest`] spawns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Launch<'a> {
    /// `cargo` with the caller's args.
    Cargo,
    /// A prebuilt test binary, executed directly with the caller's args.
    Direct { program: &'a str },
}

pub(crate) struct LibtestRun {
    pub(crate) captured: CapturedOutput,
    pub(crate) outcome: LibtestOutcome,
    /// Time from cargo spawn to the `Finished` stderr line (build phase).
    /// `None` if cargo never emitted `Finished` (build failed or no rebuild
    /// happened - though cargo emits `Finished` even on cache hits).
    pub(crate) build_elapsed: Option<Duration>,
    /// (test name, wall-clock duration) for every test the tracker observed
    /// running to completion. Order is observation order (libtest runs
    /// `--test-threads=1`, so this is also start order). Note: the
    /// deferred-observe state machine clears pending on the *next* start
    /// marker or `test result:` summary, so a test whose first println
    /// looks like a bare status (`println!("ok")`) will have its duration
    /// inflated by the gap until the next test starts.
    pub(crate) completed: Vec<(String, Duration)>,
    /// Tests whose start the tracker saw and whose result it never did, when
    /// the process ended. For a harness that died mid-test this names the
    /// SUSPECT, not a proven crash site: the names come from the stream the
    /// test itself writes to, so a lost or forged record can move them.
    pub(crate) in_flight: Vec<String>,
}

// The size difference is real - `HungTest` carries snapshot paths and pid
// lists - and irrelevant: exactly one of these exists per test run, built once
// when a run ends. Boxing it would buy nothing and cost every match site an
// indirection.
#[allow(clippy::large_enum_variant)]
pub(crate) enum LibtestOutcome {
    Completed,
    HungTest(HungTest),
}

/// Run one cargo libtest invocation under the watchdog.
///
/// `obs.shim` isolates each harness (and rustdoc) the invocation runs onto its
/// own stdout pipe and tracker, through `harness_shim`: brokkr is installed as
/// the host target runner and as rustdoc, each harness hands brokkr its pipe and
/// then execs itself. A crashed harness then ends its own stream and cannot
/// leave the shared one with an open suite that bills the next harness for the
/// dead test. The caller decides when that is safe (see
/// `check_cmd::serial_shim_fallback`); without it every harness shares one
/// stream, as before. `obs.sink` receives every observation as it happens.
///
/// `launch` says what is spawned: `cargo` with `args`, or a prebuilt test
/// binary executed directly (no cargo, so no build phase and no
/// `on_build_finished`). The harness shim is a cargo runner, so it applies to
/// the cargo launch only.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(crate) fn streaming_run_libtest<Out, Err, Fin>(
    launch: Launch<'_>,
    args: &[&str],
    cwd: &Path,
    state_root: &Path,
    env: &[(&str, &str)],
    ceilings: Ceilings,
    obs: &Observe,
    forward_stdout_line: Out,
    forward_stderr_line: Err,
    on_build_finished: Fin,
) -> Result<LibtestRun, DevError>
where
    Out: FnMut(&str) + Send + 'static,
    Err: FnMut(&str) + Send + 'static,
    Fin: FnOnce(Duration) + Send + 'static,
{
    enforce_single_threaded(args)?;
    let shim = !matches!(obs.shim, ShimMode::Off);
    if shim && launch != Launch::Cargo {
        return Err(DevError::Build(
            "the harness shim is a cargo runner and cannot isolate a directly executed binary"
                .into(),
        ));
    }
    let strict = match &obs.shim {
        ShimMode::Strict(plan) => Some(plan.clone()),
        _ => None,
    };

    let start = Instant::now();
    // Created before the spawn: an isolated harness's reconstructed text lands
    // in the same buffer the shared stream does.
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let primary = obs
        .sink
        .as_ref()
        .map(|s| StreamTap::new(s, StreamSource::Process));
    let session = shim
        .then(|| {
            harness_shim::Session::new(Arc::clone(&stdout_buf), obs.sink.clone(), strict.clone())
        })
        .transpose()?;
    let runner_env;
    let socket_env;
    let rustdoc_env;
    let mut cargo_env = env.to_vec();
    if strict.is_some() {
        // The shim reads it to fail closed: under a strict session a harness
        // that cannot get its own pipe must not run on the shared stream.
        cargo_env.push((harness_shim::STRICT_ENV, "1"));
    }
    if let Some(session) = &session {
        let host = crate::rustflags::host_triple().ok_or_else(|| {
            DevError::Config("cannot determine host triple for harness runner".into())
        })?;
        // Cargo splits a string-form runner on whitespace and strips no
        // quotes; `own_exe` is a `/proc` path that cannot contain any.
        runner_env = (
            crate::rustflags::runner_env_var(&host),
            format!(
                "{} {}",
                harness_shim::own_exe().display(),
                harness_shim::RUNNER_ARG
            ),
        );
        socket_env = session.socket();
        rustdoc_env = session.rustdoc();
        cargo_env.push((&runner_env.0, &runner_env.1));
        cargo_env.push(("BROKKR_HARNESS_SOCKET", &socket_env));
        cargo_env.push(("RUSTDOC", &rustdoc_env));
    }
    let (mut child, leader) = match launch {
        Launch::Cargo => (
            spawn_cargo_process_group(args, cwd, &cargo_env)?,
            Leader::Cargo,
        ),
        Launch::Direct { program } => (
            spawn_process_group(program, args, cwd, &cargo_env)?,
            Leader::TestBinary,
        ),
    };
    let cargo_pid = child.id();
    if let Some(session) = &session {
        session.set_cargo_pid(cargo_pid);
    }
    let acceptor = session
        .as_ref()
        .map(harness_shim::Session::run_acceptor)
        .transpose()?;
    // Ctrl-C / `brokkr kill` take this group down with brokkr. Released right
    // after the leader is reaped. See `shutdown::GroupReaper`.
    let reaper = crate::shutdown::GroupReaper::register(cargo_pid);

    let Some(stdout_pipe) = child.stdout.take() else {
        return Err(DevError::Build("cargo stdout was not piped".into()));
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        return Err(DevError::Build("cargo stderr was not piped".into()));
    };

    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(Mutex::new(None::<HungTest>));

    let cancel = Arc::new(AtomicBool::new(false));
    let stdout_buf_t = Arc::clone(&stdout_buf);
    let tracker_t = Arc::clone(&tracker);
    let cancel_t = Arc::clone(&cancel);
    let primary_t = primary.clone();
    let stdout_thread = thread::spawn(move || {
        // The same JSON drain the parallel lane uses. Both lanes read libtest's
        // event stream now; the reconstructor renders it back to human text, so
        // the downstream parsers and forwarded output see what they always saw.
        drain_libtest_json(
            stdout_pipe,
            &stdout_buf_t,
            &tracker_t,
            &cancel_t,
            primary_t,
            forward_stdout_line,
        );
    });

    let stderr_buf_t = Arc::clone(&stderr_buf);
    let build_elapsed = Arc::new(Mutex::new(None::<Duration>));
    let build_elapsed_t = Arc::clone(&build_elapsed);
    let cancel_t = Arc::clone(&cancel);
    let stderr_thread = thread::spawn(move || {
        drain_stderr(
            stderr_pipe,
            &stderr_buf_t,
            &cancel_t,
            forward_stderr_line,
            start,
            move |elapsed| {
                if let Ok(mut slot) = build_elapsed_t.lock() {
                    *slot = Some(elapsed);
                }
                on_build_finished(elapsed);
            },
        );
    });

    let state_root_t = state_root.to_path_buf();
    let tracker_t = Arc::clone(&tracker);
    let done_t = Arc::clone(&done);
    let hung_t = Arc::clone(&hung);
    // An isolated run ages every harness's tracker, plus a run-level idle
    // clock for the gaps between them; see `harness_shim::Session::watchdog`.
    let watchdog_thread = if let Some(session) = &session {
        session.watchdog(
            state_root_t,
            cargo_pid,
            tracker_t,
            hung_t,
            &ceilings,
            start,
            primary.clone(),
        )
    } else {
        let primary_w = primary.clone();
        thread::spawn(move || {
            watchdog_loop(
                state_root_t,
                cargo_pid,
                leader,
                tracker_t,
                done_t,
                hung_t,
                ceilings,
                start,
                primary_w,
            );
        })
    };

    let waited = child.wait().map_err(|error| DevError::Spawn {
        program: "cargo".into(),
        error,
    });
    if let Some(session) = &session {
        session.stop_watchdog();
    }
    drop(reaper);
    if let Some(session) = &session {
        session.settle();
    }
    // Before any early return, so the watchdog thread never outlives the
    // group whose id it would signal.
    done.store(true, Ordering::SeqCst);
    if let Some(session) = &session {
        session.finish();
    }

    join_drains(vec![stdout_thread, stderr_thread], &cancel);
    watchdog_thread.join().ok();
    if let Some(acceptor) = acceptor {
        acceptor.join().ok();
    }
    let status = waited?;

    // A `brokkr kill` / Ctrl-C under the command's `SigtermGuard` (or the
    // `check` watchdog) killed this group from the signal handler, which is
    // what released the `wait` above. The run did not fail; it was stopped.
    // Said on the stream first: everything observed so far has already reached
    // the sink, and the stop is what explains its missing terminals.
    if crate::shutdown::is_shutdown_requested() {
        if let Some(tap) = &primary {
            tap.emit(ObsEvent::Killed {
                cause: KillCause::Stopped,
                test: None,
            });
        }
        return Err(DevError::Interrupted);
    }

    let elapsed = start.elapsed();
    let stdout = clone_buffer(&stdout_buf, "stdout")?;
    let stderr = clone_buffer(&stderr_buf, "stderr")?;
    let hung_outcome = clone_hung(&hung)?;
    let build_elapsed = build_elapsed
        .lock()
        .map_err(|_| DevError::Build("build_elapsed mutex poisoned".into()))?
        .take();
    let (mut completed, mut in_flight) = tracker
        .lock()
        .map(|mut t| {
            let mut in_flight: Vec<String> = t.current.keys().cloned().collect();
            in_flight.sort();
            (std::mem::take(&mut t.completed), in_flight)
        })
        .map_err(|_| DevError::Build("test tracker mutex poisoned".into()))?;
    if let Some(session) = &session {
        let (harness_completed, harness_in_flight) = session.totals();
        completed.extend(harness_completed);
        in_flight.extend(harness_in_flight);
    }

    Ok(LibtestRun {
        captured: CapturedOutput {
            status,
            stdout,
            stderr,
            elapsed,
        },
        outcome: match hung_outcome {
            Some(h) => LibtestOutcome::HungTest(h),
            None => LibtestOutcome::Completed,
        },
        build_elapsed,
        completed,
        in_flight,
    })
}

/// Outcome of a parallel test sweep.
pub(crate) struct ParallelRun {
    pub captured: CapturedOutput,
    /// A deadline caught by the watchdog (reason + snapshot), or
    /// `Completed` if none tripped. Same shape as the serial runner.
    pub outcome: LibtestOutcome,
    /// (test name, wall-clock duration) for every test observed running to
    /// completion, reconstructed from libtest's JSON `started`/`ok`/`failed`
    /// event pairs.
    pub completed: Vec<(String, Duration)>,
}

/// Run `cargo test` for a sweep that opted into parallel execution.
///
/// Parallelism forecloses the serial runner's partial-marker watchdog (once
/// tests run concurrently, libtest's human output no longer emits a per-test
/// *start* signal to age). This path instead drives libtest's JSON event
/// stream (`--format json -Z unstable-options`, injected by the caller): each
/// `started` event records a start `Instant` in the shared [`TestTracker`],
/// each `ok`/`failed`/`ignored` clears it, and the *same* [`watchdog_loop`] the
/// serial runner uses ages the in-flight set against `per_test_timeout`. A test
/// that blows the per-test limit is blamed by name and its process group killed
/// - the exact serial guarantee, concurrent.
///
/// The JSON events are reconstructed back into human libtest text in the stdout
/// buffer, so the downstream cargo parsers see the output shape they expect.
/// `timeout` is a coarse whole-sweep backstop for a wedge with no test
/// in-flight; it also honours a cooperative `brokkr kill` / Ctrl-C. `sink`
/// receives the stream's observations, the kill that ended it, and the exit.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn run_libtest_parallel<Out, Err, Fin>(
    program: &str,
    // What `program` is: `Cargo` when it is cargo (the hung harness is its
    // child), `TestBinary` when a prebuilt test binary is executed directly
    // (its children are the tests' own subprocesses). Decides which process
    // the hang snapshot examines.
    leader: Leader,
    args: &[&str],
    cwd: &Path,
    state_root: &Path,
    env: &[(&str, &str)],
    timeout: Duration,
    per_test_timeout: Duration,
    // `abort` is lane-wide cancellation, set by whichever concurrent binary blows
    // its budget first; every other binary still executing sees it within one poll
    // and kills its own process group. Without it, "brokkr stops" was not true of
    // the parallel lane: an abort flag could stop QUEUED binaries from starting,
    // but siblings already running were left to finish, so a timeout was followed
    // by up to another full budget of test execution before the error surfaced.
    abort: Option<&AtomicBool>,
    sink: Option<&ObservationSink>,
    forward_stdout_line: Out,
    forward_stderr_line: Err,
    on_build_finished: Fin,
) -> Result<ParallelRun, DevError>
where
    Out: FnMut(&str) + Send + 'static,
    Err: FnMut(&str) + Send + 'static,
    Fin: FnOnce(Duration) + Send + 'static,
{
    let start = Instant::now();
    let primary = sink.map(|s| StreamTap::new(s, StreamSource::Process));
    let mut child = spawn_process_group(program, args, cwd, env)?;
    // Spawned with `process_group(0)`, so the child's pid is its pgid.
    let cargo_pid = child.id();
    // Ctrl-C / `brokkr kill` take this group down with brokkr, whether or not
    // a `SigtermGuard` is active. See `shutdown::GroupReaper`.
    let reaper = crate::shutdown::GroupReaper::register(cargo_pid);

    let Some(stdout_pipe) = child.stdout.take() else {
        return Err(DevError::Build(format!("{program} stdout was not piped")));
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        return Err(DevError::Build(format!("{program} stderr was not piped")));
    };

    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(Mutex::new(None::<HungTest>));

    let cancel = Arc::new(AtomicBool::new(false));
    let stdout_buf_t = Arc::clone(&stdout_buf);
    let tracker_t = Arc::clone(&tracker);
    let cancel_t = Arc::clone(&cancel);
    let primary_t = primary.clone();
    let stdout_thread = thread::spawn(move || {
        drain_libtest_json(
            stdout_pipe,
            &stdout_buf_t,
            &tracker_t,
            &cancel_t,
            primary_t,
            forward_stdout_line,
        );
    });
    let stderr_buf_t = Arc::clone(&stderr_buf);
    let cancel_t = Arc::clone(&cancel);
    let stderr_thread = thread::spawn(move || {
        drain_stderr(
            stderr_pipe,
            &stderr_buf_t,
            &cancel_t,
            forward_stderr_line,
            start,
            on_build_finished,
        );
    });

    // One watchdog owns both per-test and wall expiry, including diagnostics.
    let state_root_t = state_root.to_path_buf();
    let tracker_w = Arc::clone(&tracker);
    let done_w = Arc::clone(&done);
    let hung_w = Arc::clone(&hung);
    let primary_w = primary.clone();
    let watchdog_thread = thread::spawn(move || {
        watchdog_loop(
            state_root_t,
            cargo_pid,
            leader,
            tracker_w,
            done_w,
            hung_w,
            Ceilings {
                per_test: per_test_timeout,
                wall: Some(timeout),
                shape: WallShape::SharedHarness,
            },
            start,
            primary_w,
        );
    });

    let waited = wait_parallel(&mut child, program, abort);
    // The kill is recorded as what it was, before the stream end it caused.
    if let Some(tap) = &primary {
        let cause = match &waited {
            Ok(ParallelWait::Exited {
                cancelled: true, ..
            }) => Some(KillCause::Cancelled),
            Ok(ParallelWait::Interrupted) => Some(KillCause::Stopped),
            _ => None,
        };
        if let Some(cause) = cause {
            tap.emit(ObsEvent::Killed { cause, test: None });
        }
    }
    drop(reaper);
    // Before any early return: the watchdog thread must stop before this
    // group's id can be recycled, or it could later signal a stranger.
    done.store(true, Ordering::SeqCst);

    join_drains(vec![stdout_thread, stderr_thread], &cancel);
    watchdog_thread.join().ok();

    // A lane-wide cancellation was reported on the stream above, as what it
    // was; nothing in the exit status could say it.
    let status = match waited? {
        ParallelWait::Exited { status, .. } => status,
        ParallelWait::Interrupted => return Err(DevError::Interrupted),
    };
    if let Some(tap) = &primary {
        use std::os::unix::process::ExitStatusExt;
        tap.emit(ObsEvent::Exited {
            code: status.code(),
            signal: status.signal(),
        });
    }

    let elapsed = start.elapsed();
    let stdout = clone_buffer(&stdout_buf, "stdout")?;
    let stderr = clone_buffer(&stderr_buf, "stderr")?;
    let hung_outcome = clone_hung(&hung)?;
    let completed = tracker
        .lock()
        .map(|mut t| std::mem::take(&mut t.completed))
        .map_err(|_| DevError::Build("test tracker mutex poisoned".into()))?;

    Ok(ParallelRun {
        captured: CapturedOutput {
            status,
            stdout,
            stderr,
            elapsed,
        },
        outcome: match hung_outcome {
            Some(h) => LibtestOutcome::HungTest(h),
            None => LibtestOutcome::Completed,
        },
        completed,
    })
}

/// How [`wait_parallel`] ended.
enum ParallelWait {
    /// The leader exited, on its own, after a watchdog kill, or after a
    /// lane-wide abort (`cancelled`). Deadline evidence lives in `HungTest`.
    Exited {
        status: std::process::ExitStatus,
        cancelled: bool,
    },
    /// A cooperative shutdown (`brokkr kill`, Ctrl-C, the `check` watchdog):
    /// the group was killed and reaped, and the run is not a verdict.
    Interrupted,
}

/// The parallel runner's wait loop: poll the process-group leader until it
/// exits, or kill the group on a lane-wide `abort`,
/// or a cooperative shutdown.
fn wait_parallel(
    child: &mut std::process::Child,
    program: &str,
    abort: Option<&AtomicBool>,
) -> Result<ParallelWait, DevError> {
    // Spawned with `process_group(0)`, so the child's pid is its pgid.
    let cargo_pid = child.id();
    let spawn_err = |error: std::io::Error| DevError::Spawn {
        program: program.into(),
        error,
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Killed from the signal handler rather than seen by the poll:
                // same meaning.
                if crate::shutdown::is_shutdown_requested() {
                    return Ok(ParallelWait::Interrupted);
                }
                return Ok(ParallelWait::Exited {
                    status,
                    cancelled: false,
                });
            }
            Ok(None) => {
                let cancelled = abort.is_some_and(|a| a.load(Ordering::SeqCst));
                // `check` and `brokkr test` hold a `SigtermGuard` for their
                // whole run, whose handler already killed this group; the poll
                // covers a request raised without a signal (the watchdog).
                let interrupted = crate::shutdown::is_shutdown_requested();
                if cancelled || interrupted {
                    kill_process_group(cargo_pid).ok();
                    let status = child.wait().map_err(spawn_err)?;
                    if interrupted {
                        return Ok(ParallelWait::Interrupted);
                    }
                    return Ok(ParallelWait::Exited { status, cancelled });
                }
                thread::sleep(WATCHDOG_POLL);
            }
            Err(e) => return Err(spawn_err(e)),
        }
    }
}

/// One listing run of a prebuilt test binary: its exit status, its output, and
/// the two ways it can have failed to finish.
pub(crate) struct ListingRun {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// Killed at the wall deadline. Whatever it printed is not a listing.
    pub(crate) timed_out: bool,
    /// The leader exited but its output did not close within `DRAIN_GRACE`
    /// (something it started still holds a pipe), so capture was cut short and
    /// the output may be truncated.
    pub(crate) unsettled: bool,
}

/// Run a prebuilt test binary to list its tests, under a wall deadline.
///
/// The listing is the binary's own code - static constructors run before
/// `main`, and a custom harness may do anything - so it is launched the way a
/// test run is: own process group, the run token, the hold capability, death
/// with brokkr (see [`spawn_process_group`]). The wall deadline bounds the
/// whole process, and once it exits its output gets `DRAIN_GRACE` to close,
/// so neither a hang nor a leaked descendant holding a pipe can stall the
/// caller. A cooperative shutdown kills the group and is `Interrupted`.
pub(crate) fn run_listing(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    wall: Duration,
) -> Result<ListingRun, DevError> {
    let start = Instant::now();
    let mut child = spawn_process_group(program, args, cwd, env)?;
    let pgid = child.id();
    let reaper = crate::shutdown::GroupReaper::register(pgid);
    let (Some(stdout_pipe), Some(stderr_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        kill_process_group(pgid).ok();
        child.wait().ok();
        return Err(DevError::Build(format!("{program} output was not piped")));
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let (out_buf, out_thread) = drain_to_buffer(stdout_pipe, Arc::clone(&cancel));
    let (err_buf, err_thread) = drain_to_buffer(stderr_pipe, Arc::clone(&cancel));

    let spawn_err = |error: std::io::Error| DevError::Spawn {
        program: program.into(),
        error,
    };
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if crate::shutdown::is_shutdown_requested() {
                    kill_process_group(pgid).ok();
                    child.wait().ok();
                    drop(reaper);
                    cancel.store(true, Ordering::Release);
                    out_thread.join().ok();
                    err_thread.join().ok();
                    return Err(DevError::Interrupted);
                }
                if start.elapsed() >= wall {
                    kill_process_group(pgid).ok();
                    timed_out = true;
                    break child.wait().map_err(spawn_err)?;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(spawn_err(e)),
        }
    };
    drop(reaper);
    let unsettled = join_drains(vec![out_thread, err_thread], &cancel);
    // The signal handler may have killed the group itself, which ends the
    // wait above with an ordinary-looking status; and a shutdown can land
    // while the drains settle. Either way this was a stop, not a listing.
    if crate::shutdown::is_shutdown_requested() {
        return Err(DevError::Interrupted);
    }
    let take = |b: &Arc<Mutex<Vec<u8>>>| b.lock().map(|v| v.clone()).unwrap_or_default();
    Ok(ListingRun {
        status,
        stdout: take(&out_buf),
        stderr: take(&err_buf),
        timed_out,
        unsettled,
    })
}

pub(crate) fn effective_test_threads(args: &[String]) -> Result<Option<u32>, DevError> {
    effective_test_threads_from(args)
}

fn enforce_single_threaded(args: &[&str]) -> Result<(), DevError> {
    match effective_test_threads_from(args)? {
        Some(1) => Ok(()),
        Some(n) => Err(DevError::Config(format!(
            "libtest watchdog requires --test-threads=1, got --test-threads={n}"
        ))),
        None => Err(DevError::Config(
            "libtest watchdog requires --test-threads=1, but no --test-threads flag was passed"
                .into(),
        )),
    }
}

fn effective_test_threads_from<T: AsRef<str>>(args: &[T]) -> Result<Option<u32>, DevError> {
    let mut current = None;
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_ref();
        if let Some(value) = arg.strip_prefix("--test-threads=") {
            current = Some(parse_thread_count(value)?);
        } else if arg == "--test-threads" {
            let Some(value) = args.get(idx + 1) else {
                return Err(DevError::Config(
                    "--test-threads requires a numeric value".into(),
                ));
            };
            current = Some(parse_thread_count(value.as_ref())?);
            idx += 1;
        }
        idx += 1;
    }
    Ok(current)
}

fn parse_thread_count(value: &str) -> Result<u32, DevError> {
    value.parse::<u32>().map_err(|_| {
        DevError::Config(format!(
            "--test-threads requires a numeric value, got {value:?}"
        ))
    })
}
