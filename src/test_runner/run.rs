//! The shared cargo/direct libtest runner and the test-listing runner.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::harness_shim;
use super::{
    Ceilings, ExecutionOutcome, HungTest, KillCause, ObsEvent, Observation, ObservationSink,
    RunContext, StreamSource, StreamTap, StrictPlan, TestTracker, WATCHDOG_POLL, clone_buffer,
    clone_hung, drain_libtest_json, drain_stderr, drain_to_buffer, join_drains,
    spawn_cargo_process_group, spawn_process_group, watchdog_loop,
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
    /// Fail closed when the run cannot attribute a cargo-launched harness.
    Strict(StrictPlan),
}

/// What a caller wants observed: the journal sink, and the isolation policy.
#[derive(Clone, Default)]
pub(crate) struct Observe {
    pub(crate) sink: Option<ObservationSink>,
    pub(crate) shim: ShimMode,
}

/// What the process a run launched is, which decides where a hang snapshot is
/// taken from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Leader {
    Cargo,
    TestBinary,
}

/// What one libtest run spawns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Launch<'a> {
    Cargo,
    Direct { program: &'a str },
}

/// Per-invocation process inputs shared by all libtest lanes.
pub(crate) struct LibtestInvocation<'a> {
    pub(crate) args: super::LibtestArgs,
    pub(crate) cwd: &'a Path,
    pub(crate) state_root: &'a Path,
    pub(crate) env: &'a [(&'a str, &'a str)],
    pub(crate) context: RunContext,
    /// Set by the parallel lane when a sibling exceeds its budget.
    pub(crate) abort: Option<&'a AtomicBool>,
}

pub(crate) struct LibtestRun {
    pub(crate) captured: CapturedOutput,
    pub(crate) outcome: ExecutionOutcome,
    pub(crate) hung: Option<HungTest>,
    /// Time from cargo spawn to its `Finished` stderr line.
    pub(crate) build_elapsed: Option<Duration>,
    /// Tests the tracker observed running to completion.
    pub(crate) completed: Vec<(String, Duration)>,
    /// Tests whose start was observed but whose result was not.
    pub(crate) in_flight: Vec<String>,
}

/// Run one libtest invocation. Cargo and direct launches share process
/// lifecycle, JSON observation, the watchdog, drains, cancellation and
/// outcome reduction. Reconstructed human text is captured for display only.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) fn run_libtest<Out, Err, Fin>(
    launch: Launch<'_>,
    invocation: LibtestInvocation<'_>,
    ceilings: Ceilings,
    observe: &Observe,
    forward_stdout_line: Out,
    forward_stderr_line: Err,
    on_build_finished: Fin,
) -> Result<LibtestRun, DevError>
where
    Out: FnMut(&str) + Send + 'static,
    Err: FnMut(&str) + Send + 'static,
    Fin: FnOnce(Duration) + Send + 'static,
{
    let LibtestInvocation {
        args,
        cwd,
        state_root,
        env,
        context,
        abort,
    } = invocation;
    let rendered_args = args.render(launch);
    let args: Vec<&str> = rendered_args.iter().map(String::as_str).collect();
    let args = args.as_slice();
    let shim = !matches!(observe.shim, ShimMode::Off);
    if shim && launch != Launch::Cargo {
        return Err(DevError::Build(
            "the harness shim is a cargo runner and cannot isolate a directly executed binary"
                .into(),
        ));
    }
    let strict = match &observe.shim {
        ShimMode::Strict(plan) => Some(plan.clone()),
        _ => None,
    };

    let start = Instant::now();
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(Mutex::new(None::<HungTest>));
    let outcome = Arc::new(Mutex::new(ExecutionOutcome::default()));
    let downstream = observe.sink.clone();
    let outcome_sink = Arc::clone(&outcome);
    let sink: ObservationSink = Arc::new(move |observation: Observation| {
        if let Ok(mut reduced) = outcome_sink.lock() {
            reduced.observe(&observation);
        }
        if let Some(downstream) = &downstream {
            downstream(observation);
        }
    });
    let primary = StreamTap::new(&sink, StreamSource::Process);
    let session = shim
        .then(|| {
            harness_shim::Session::new(
                Arc::clone(&stdout_buf),
                Some(Arc::clone(&sink)),
                strict.clone(),
                context.clone(),
            )
        })
        .transpose()?;

    let runner_env;
    let socket_env;
    let rustdoc_env;
    let mut cargo_env = env.to_vec();
    if strict.is_some() {
        cargo_env.push((harness_shim::STRICT_ENV, "1"));
    }
    if let Some(session) = &session {
        let host = crate::rustflags::host_triple().ok_or_else(|| {
            DevError::Config("cannot determine host triple for harness runner".into())
        })?;
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

    let (mut child, leader, program) = match launch {
        Launch::Cargo => (
            spawn_cargo_process_group(args, cwd, &cargo_env, &context)?,
            Leader::Cargo,
            "cargo",
        ),
        Launch::Direct { program } => (
            spawn_process_group(
                program,
                args,
                cwd,
                &cargo_env,
                &context,
                crate::cargo_env::SpawnProfile::Plain,
            )?,
            Leader::TestBinary,
            program,
        ),
    };
    let leader_pid = child.id();
    if let Some(session) = &session {
        session.set_cargo_pid(leader_pid);
    }
    let acceptor = session
        .as_ref()
        .map(harness_shim::Session::run_acceptor)
        .transpose()?;
    let reaper = crate::shutdown::GroupReaper::register(leader_pid);
    let (Some(stdout_pipe), Some(stderr_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        crate::process::signal_process_group(leader_pid, libc::SIGKILL).ok();
        child.wait().ok();
        return Err(DevError::Build(format!("{program} output was not piped")));
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let stdout_buf_t = Arc::clone(&stdout_buf);
    let tracker_t = Arc::clone(&tracker);
    let cancel_t = Arc::clone(&cancel);
    let stdout_tap = Some(primary.clone());
    let stdout_thread = thread::spawn(move || {
        drain_libtest_json(
            stdout_pipe,
            &stdout_buf_t,
            &tracker_t,
            &cancel_t,
            stdout_tap,
            forward_stdout_line,
        );
    });

    let build_elapsed = Arc::new(Mutex::new(None::<Duration>));
    let build_elapsed_t = Arc::clone(&build_elapsed);
    let stderr_buf_t = Arc::clone(&stderr_buf);
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

    let watchdog_thread = if let Some(session) = &session {
        session.watchdog(
            state_root.to_path_buf(),
            leader_pid,
            Arc::clone(&tracker),
            Arc::clone(&hung),
            &ceilings,
            start,
            Some(primary.clone()),
        )
    } else {
        let context = context.clone();
        let tap = Some(primary.clone());
        let state_root = state_root.to_path_buf();
        let tracker = Arc::clone(&tracker);
        let done = Arc::clone(&done);
        let hung = Arc::clone(&hung);
        thread::spawn(move || {
            watchdog_loop(
                state_root, leader_pid, leader, tracker, done, hung, ceilings, start, tap, context,
            );
        })
    };

    let waited = wait_child(&mut child, program, abort, &context);
    if let Some(session) = &session {
        session.stop_watchdog();
    }
    drop(reaper);
    if let Some(session) = &session {
        session.settle();
    }
    done.store(true, Ordering::SeqCst);
    if let Some(session) = &session {
        session.finish();
    }
    join_drains(vec![stdout_thread, stderr_thread], &cancel, &context);
    watchdog_thread.join().ok();
    if let Some(acceptor) = acceptor {
        acceptor.join().ok();
    }

    let (status, stopped, cancelled) = waited?;
    if stopped {
        primary.emit(ObsEvent::Killed {
            cause: KillCause::Stopped,
            test: None,
        });
    } else if cancelled {
        primary.emit(ObsEvent::Killed {
            cause: KillCause::Cancelled,
            test: None,
        });
    }
    use std::os::unix::process::ExitStatusExt;
    primary.emit(ObsEvent::Exited {
        code: status.code(),
        signal: status.signal(),
    });
    if stopped || context.stop.is_requested() {
        if !stopped {
            primary.emit(ObsEvent::Killed {
                cause: KillCause::Stopped,
                test: None,
            });
        }
        return Err(DevError::Interrupted);
    }

    let elapsed = start.elapsed();
    let stdout = clone_buffer(&stdout_buf, "stdout")?;
    let stderr = clone_buffer(&stderr_buf, "stderr")?;
    let hung = clone_hung(&hung)?;
    let build_elapsed = build_elapsed
        .lock()
        .map_err(|_| DevError::Build("build_elapsed mutex poisoned".into()))?
        .take();
    let (mut completed, mut in_flight) = tracker
        .lock()
        .map(|mut tracker| {
            let mut in_flight: Vec<String> = tracker.current.keys().cloned().collect();
            in_flight.sort();
            (std::mem::take(&mut tracker.completed), in_flight)
        })
        .map_err(|_| DevError::Build("test tracker mutex poisoned".into()))?;
    if let Some(session) = &session {
        let (harness_completed, harness_in_flight) = session.totals();
        completed.extend(harness_completed);
        in_flight.extend(harness_in_flight);
    }
    let outcome = outcome
        .lock()
        .map(|outcome| outcome.clone())
        .map_err(|_| DevError::Build("execution outcome mutex poisoned".into()))?;

    Ok(LibtestRun {
        captured: CapturedOutput {
            ending: crate::output::Ending::from_exit_status(&status),
            stdout,
            stderr,
            elapsed,
        },
        outcome,
        hung,
        build_elapsed,
        completed,
        in_flight,
    })
}

fn wait_child(
    child: &mut std::process::Child,
    program: &str,
    abort: Option<&AtomicBool>,
    context: &RunContext,
) -> Result<(std::process::ExitStatus, bool, bool), DevError> {
    let pgid = child.id();
    let spawn_err = |error| DevError::Spawn {
        program: program.into(),
        error,
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok((status, context.stop.is_requested(), false)),
            Ok(None) => {
                let stopped = context.stop.is_requested();
                let cancelled = abort.is_some_and(|flag| flag.load(Ordering::SeqCst));
                if stopped || cancelled {
                    crate::process::signal_process_group(pgid, libc::SIGKILL).ok();
                    let status = child.wait().map_err(spawn_err)?;
                    return Ok((status, stopped, cancelled && !stopped));
                }
                thread::sleep(WATCHDOG_POLL);
            }
            Err(error) => return Err(spawn_err(error)),
        }
    }
}

/// One listing run of a prebuilt test binary: its exit status, its output, and
/// the two ways it can have failed to finish.
pub(crate) struct ListingRun {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) timed_out: bool,
    pub(crate) unsettled: bool,
}

impl ListingRun {
    pub(crate) fn success(&self) -> bool {
        self.status.success()
    }
}

/// Run a prebuilt test binary to list its tests, under a wall deadline.
pub(crate) fn run_listing(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
    wall: Duration,
    context: &RunContext,
) -> Result<ListingRun, DevError> {
    let start = Instant::now();
    let mut child = spawn_process_group(
        program,
        args,
        cwd,
        env,
        context,
        crate::cargo_env::SpawnProfile::Plain,
    )?;
    let pgid = child.id();
    let reaper = crate::shutdown::GroupReaper::register(pgid);
    let (Some(stdout_pipe), Some(stderr_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        crate::process::signal_process_group(pgid, libc::SIGKILL).ok();
        child.wait().ok();
        return Err(DevError::Build(format!("{program} output was not piped")));
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let (out_buf, out_thread) = drain_to_buffer(stdout_pipe, Arc::clone(&cancel));
    let (err_buf, err_thread) = drain_to_buffer(stderr_pipe, Arc::clone(&cancel));
    let spawn_err = |error| DevError::Spawn {
        program: program.into(),
        error,
    };
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if context.stop.is_requested() {
                    crate::process::signal_process_group(pgid, libc::SIGKILL).ok();
                    child.wait().ok();
                    drop(reaper);
                    cancel.store(true, Ordering::Release);
                    out_thread.join().ok();
                    err_thread.join().ok();
                    return Err(DevError::Interrupted);
                }
                if start.elapsed() >= wall {
                    crate::process::signal_process_group(pgid, libc::SIGKILL).ok();
                    timed_out = true;
                    break child.wait().map_err(spawn_err)?;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(spawn_err(error)),
        }
    };
    drop(reaper);
    let unsettled = join_drains(vec![out_thread, err_thread], &cancel, context);
    if context.stop.is_requested() {
        return Err(DevError::Interrupted);
    }
    let take =
        |buffer: &Arc<Mutex<Vec<u8>>>| buffer.lock().map(|bytes| bytes.clone()).unwrap_or_default();
    Ok(ListingRun {
        status,
        stdout: take(&out_buf),
        stderr: take(&err_buf),
        timed_out,
        unsettled,
    })
}

pub(crate) fn effective_test_threads(args: &[String]) -> Result<Option<u32>, DevError> {
    let mut current = None;
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if let Some(value) = arg.strip_prefix("--test-threads=") {
            current = Some(super::invocation::parse_thread_count(value)?);
        } else if arg == "--test-threads" {
            let Some(value) = args.get(idx + 1) else {
                return Err(DevError::Config(
                    "--test-threads requires a numeric value".into(),
                ));
            };
            current = Some(super::invocation::parse_thread_count(value.as_str())?);
            idx += 1;
        }
        idx += 1;
    }
    Ok(current)
}
