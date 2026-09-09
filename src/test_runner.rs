//! Shared streaming runner for cargo libtest invocations.
//!
//! The runner keeps the captured stdout/stderr buffers used by the existing
//! cargo parsers, while also watching libtest's partial `test name ... `
//! progress marker before the terminating newline arrives.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::DevError;
use crate::output::CapturedOutput;
use crate::ratatoskr::process::snapshot_proc;

/// **The hard cap.** Every test brokkr runs gets this much wall time and no more;
/// exceeding it fails the run and stops it. The single exception is
/// `brokkr test --timeout`, which raises the cap for one resolved test and is
/// itself limited to 280s by the CLI parser.
///
/// The clock is fed by libtest's announcements, which share a stream with test
/// output, so a lost event can blur *which* test is named - see [`Ceilings`].
/// It cannot excuse the overrun: if no test has been seen completing for longer
/// than the budget, the budget is blown whoever spent it.
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const WATCHDOG_POLL: Duration = Duration::from_millis(250);

/// How long a libtest run may go with no test in flight before the watchdog
/// kills it as a wedge. The per-test ceiling ages a test only after libtest
/// announces it, so it is blind to everything around the tests: the compile
/// and link inside `cargo test`, and the binary's teardown after the last
/// result. Observed: a `brokkr test` parked for over an hour on cargo's
/// "Blocking waiting for file lock on build directory" - a rust-analyzer
/// `cargo check` held the target lock - with nothing for the per-test clock
/// to age. Five minutes matches `check`'s clippy ceiling: a cold build of
/// the largest consuming workspace fits, a wedge does not.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The name the watchdog blames when [`IDLE_TIMEOUT`] fires: no test was
/// in flight, so there is nothing to name.
pub(crate) const IDLE_WEDGE: &str = "(no test in flight)";

/// The name the watchdog blames when execution is under way but nothing has
/// completed within the per-test cap. Either a test is over budget or its
/// lifecycle records were lost; brokkr cannot tell which, and the budget is blown
/// regardless.
pub(crate) const NO_COMPLETION: &str = "(no test completed within the budget)";

/// The name the watchdog blames when the wall deadline fires. There is no
/// offender to name: "active when the deadline expired" is not "caused the
/// timeout", and the budget can just as well expire during the hundredth
/// perfectly normal test.
pub(crate) const WALL_WEDGE: &str = "(sweep wall deadline)";

/// Wall-clock backstop for a whole run, enforced from the watchdog's own clock
/// and reachable by nothing a test prints.
///
/// Not the primary bound - [`TEST_TIMEOUT`] is - but the one that catches a wedge
/// neither the per-test cap nor [`IDLE_TIMEOUT`] can charge to anything: no test
/// in flight to bill, and enough parsed activity to keep the idle window
/// resetting. Inside `brokkr check` the 15-minute test-phase watchdog
/// (`check_cmd/watchdog.rs`) always fires first, so in practice this governs
/// `brokkr test`.
pub(crate) const SWEEP_WALL_TIMEOUT: Duration = Duration::from_secs(1800);

/// The ceilings one libtest run is subject to.
///
/// `per_test` is the hard cap and it terminates. `wall` is a backstop for a wedge
/// the cap cannot charge to any test. `shape` says only what a kill may be
/// *called*, which is the one thing a corrupted stream can take away: with a
/// caller-resolved single test the name is exact, and on a shared harness it is
/// the tracker's best-known suspect.
/// What the caller knows about the run's shape, and therefore what a wall expiry
/// means.
///
/// This exists so the *reason* for a kill comes from the caller rather than from
/// the tracker. Deriving it from the tracker was a real defect: at a shared
/// sweep's 1800s wall deadline an empty tracker made the verdict read "idle
/// ceiling", and a one-test invocation with a ceiling above `IDLE_TIMEOUT` whose
/// start event was never observed reported an idle timeout even though only the
/// per-test wall clock had killed it. The untrusted stream did not pull the
/// trigger, but it still wrote the explanation.
#[derive(Clone)]
pub(crate) enum WallShape {
    /// Many tests in one process. A wall expiry names no offender, because none
    /// is knowable.
    SharedHarness,
    /// One process running exactly one test, resolved by the caller. A wall
    /// expiry *is* that test's ceiling, and the name comes from the caller - not
    /// from anything the process printed.
    OneTest { name: String },
}

#[derive(Clone)]
pub(crate) struct Ceilings {
    /// Per-test ceiling for blame, aged from announced starts. Advisory.
    pub(crate) per_test: Duration,
    /// Wall-clock ceiling for the whole run. Authoritative; unreachable by test
    /// output. `None` leaves the run bounded only by the phase watchdog above
    /// it.
    pub(crate) wall: Option<Duration>,
    /// What a wall expiry means here.
    pub(crate) shape: WallShape,
}

impl Ceilings {
    /// A shared-harness sweep: many tests in one process, so the per-test clock
    /// can only ever name a suspect, and the wall clock is the real bound.
    pub(crate) fn shared_harness() -> Self {
        Self {
            per_test: TEST_TIMEOUT,
            wall: Some(SWEEP_WALL_TIMEOUT),
            shape: WallShape::SharedHarness,
        }
    }

    /// A shared harness with a caller-chosen per-test ceiling for blame.
    pub(crate) fn shared_harness_with(per_test: Duration) -> Self {
        Self { per_test, wall: Some(SWEEP_WALL_TIMEOUT), shape: WallShape::SharedHarness }
    }

    /// One process running exactly one test, named by the caller.
    pub(crate) fn one_test(ceiling: Duration, name: impl Into<String>) -> Self {
        Self {
            per_test: ceiling,
            wall: Some(ceiling),
            shape: WallShape::OneTest { name: name.into() },
        }
    }
}

/// Whole-sweep wall-clock backstop for a parallel test sweep, enforced from the
/// runner's own clock in the `try_wait` loop.
///
/// A parallel sweep enforces the same [`TEST_TIMEOUT`] cap per test as the serial
/// path, aging each in-flight test from libtest's JSON `started`/`ok`/`failed`
/// events. This ceiling covers what that cannot bill to any test: a stall with
/// nothing in flight, or a wedge outside the test bodies entirely. Generous, so it
/// only trips on something the per-test cap genuinely cannot see.
pub(crate) const PARALLEL_SWEEP_TIMEOUT: Duration = Duration::from_secs(1800);

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

/// Why a run was killed.
///
/// An enum rather than a magic value in `test`, because the two are not the same
/// kind of claim and conflating them let a wall expiry render as
/// "test (sweep wall deadline) ran 1800s, exceeding the 1800s per-test timeout,
/// after libtest started it" - a sentence that contradicts itself. Keeping the
/// distinction in the type also makes it hard to reintroduce the worse bug of
/// letting attribution terminate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TimeoutReason {
    /// The authoritative wall clock expired. There is no offender to name:
    /// "active when the deadline expired" is not "caused the timeout", and the
    /// budget can just as well expire during the hundredth healthy test.
    /// `blamed` carries whatever was in flight, as unverified context only.
    SweepWall { blamed: Vec<String> },
    /// One process ran one test and that test crossed its ceiling. The only
    /// case where a per-test ceiling is a guarantee rather than a guess.
    PerTest { name: String },
    /// Nothing was in flight for the idle window - cargo wedged before the
    /// first test or after the last.
    Idle,
}

impl TimeoutReason {
    /// The name to file this under in reports and snapshot paths.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::SweepWall { .. } => WALL_WEDGE.to_owned(),
            Self::PerTest { name } => name.clone(),
            Self::Idle => IDLE_WEDGE.to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HungTest {
    pub(crate) reason: TimeoutReason,
    pub(crate) test: String,
    pub(crate) elapsed: Duration,
    pub(crate) ceiling: Duration,
    pub(crate) snapshot_dir: PathBuf,
    pub(crate) cargo_pid: u32,
    pub(crate) test_pids: Vec<u32>,
    pub(crate) snapshot_pid: Option<u32>,
    pub(crate) wchan: Option<String>,
    pub(crate) stack: Option<String>,
    pub(crate) snapshot_error: Option<String>,
}

struct TestTracker {
    current: HashMap<String, Instant>,
    completed: Vec<(String, Duration)>,
    /// Whether any lifecycle event has ever been observed.
    ///
    /// While this is false nothing has been parsed, so `idle_since` still holds
    /// the run's start and the idle measurement is plain wall time - trustworthy
    /// because no input has touched it. Once an event arrives, output can reset
    /// the window and the idle clock becomes advisory like the per-test one.
    ever_observed: bool,
    /// Whether a suite has announced itself, i.e. test execution has begun.
    /// From that point the per-test cap applies to the whole run, not only to
    /// tests brokkr happens to be watching.
    executing: bool,
    /// When brokkr last saw *new* lifecycle progress: a suite announcing itself,
    /// or the first sighting of a test starting or completing. Bounded by the
    /// per-test cap - see [`TestTracker::timed_out`] for why a no-progress clock
    /// is needed on top of the per-test one, and `observe_start` for why only
    /// first sightings count.
    last_progress: Instant,
    /// Names whose start has been counted, so a repeated start cannot refresh the
    /// clock. A test can write lifecycle-shaped records to the same stdout the
    /// protocol uses; this bounds how many refreshes such a forgery can buy to the
    /// number of distinct names it invents.
    seen: HashSet<String>,
    /// Names whose completion has been counted, for the same reason.
    finished: HashSet<String>,
    /// When the in-flight set last changed (or the run began): the start of
    /// the current no-test-in-flight window, which [`IDLE_TIMEOUT`] bounds.
    idle_since: Instant,
}

impl Default for TestTracker {
    fn default() -> Self {
        Self {
            current: HashMap::new(),
            completed: Vec::new(),
            ever_observed: false,
            executing: false,
            last_progress: Instant::now(),
            seen: HashSet::new(),
            finished: HashSet::new(),
            idle_since: Instant::now(),
        }
    }
}

impl TestTracker {
    fn observe_start(&mut self, name: String) {
        // Refreshes the no-completion clock, and only the FIRST time this name is
        // seen. Two reasons, and they pull in opposite directions.
        //
        // It must refresh at all, or the clock false-kills honest runs: a suite
        // that announces itself at t=0, starts its first test at t=2 and finishes
        // it at t=19 has broken no budget, yet a clock armed only by the suite
        // event fires at t=20 and kills a healthy test.
        //
        // It must refresh only once per name, or it is a forgery channel: a test
        // writing straight to the process's stdout can emit lifecycle records, and
        // one that re-emits the same start (or completion) on a timer would reset
        // the clock forever and run until the sweep backstop. Once-per-name means
        // a forger can spend only as many refreshes as there are real tests.
        let first_sighting = self.seen.insert(name.clone());
        self.current.entry(name).or_insert_with(Instant::now);
        self.idle_since = Instant::now();
        self.ever_observed = true;
        self.executing = true;
        if first_sighting {
            self.last_progress = Instant::now();
        }
    }

    fn observe_result(&mut self, name: &str) {
        let first_completion = self.finished.insert(name.to_owned());
        if let Some(started) = self.current.remove(name) {
            self.completed.push((name.to_owned(), started.elapsed()));
        }
        self.idle_since = Instant::now();
        self.ever_observed = true;
        self.executing = true;
        // Once per name, for the forgery reason above.
        if first_completion {
            self.last_progress = Instant::now();
        }
    }

    /// A suite announced itself: test execution has begun, so the per-test cap
    /// applies from here even before any individual test is seen starting.
    fn observe_suite_start(&mut self) {
        self.executing = true;
        self.last_progress = Instant::now();
        self.idle_since = Instant::now();
    }

    /// What has blown its budget, if anything.
    ///
    /// Two clocks, because a test can burn wall time in two ways brokkr sees
    /// differently:
    ///
    /// 1. **A test we are watching** runs past `timeout`. The obvious case.
    /// 2. **Nothing completes** for `timeout` while execution is under way. This
    ///    is the case a lost *start* record produces, and it used to escape
    ///    entirely: with no start, `current` stayed empty, so the per-test clock
    ///    had nothing to age and the only remaining bound was the five-minute
    ///    idle ceiling - a test could run for nearly five minutes under a
    ///    twenty-second contract. Once a suite has announced itself, brokkr must
    ///    see a completion at least every `timeout`; if it does not, either a test
    ///    is over budget or its records were lost, and the budget is blown either
    ///    way.
    ///
    /// Before any suite announces itself there is no test to bill, so the idle
    /// ceiling covers that window instead (cargo wedged on a build-directory
    /// lock, say).
    fn timed_out(&self, timeout: Duration) -> Option<(String, Duration)> {
        // 1. A test we can see, over budget. Prefer this: it can be named.
        let watched = self
            .current
            .iter()
            .filter_map(|(name, started)| {
                let elapsed = started.elapsed();
                (elapsed >= timeout).then(|| (name.clone(), elapsed))
            })
            .max_by_key(|(_, elapsed)| *elapsed);
        if watched.is_some() {
            return watched;
        }

        // 2. Execution under way and nothing completing.
        if self.executing {
            let stalled = self.last_progress.elapsed();
            if stalled >= timeout {
                return Some((NO_COMPLETION.to_owned(), stalled));
            }
        }

        // 3. Nothing running yet: the idle window.
        if self.current.is_empty() {
            let idle = self.idle_since.elapsed();
            return (idle >= IDLE_TIMEOUT).then(|| (IDLE_WEDGE.to_owned(), idle));
        }
        None
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub(crate) fn streaming_run_libtest<Out, Err, Fin>(
    args: &[&str],
    cwd: &Path,
    state_root: &Path,
    env: &[(&str, &str)],
    ceilings: Ceilings,
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

    let start = Instant::now();
    let mut child = spawn_cargo_process_group(args, cwd, env)?;
    let cargo_pid = child.id();

    let Some(stdout_pipe) = child.stdout.take() else {
        return Err(DevError::Build("cargo stdout was not piped".into()));
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        return Err(DevError::Build("cargo stderr was not piped".into()));
    };

    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let tracker = Arc::new(Mutex::new(TestTracker::default()));
    let done = Arc::new(AtomicBool::new(false));
    let hung = Arc::new(Mutex::new(None::<HungTest>));

    let stdout_buf_t = Arc::clone(&stdout_buf);
    let tracker_t = Arc::clone(&tracker);
    let stdout_thread = thread::spawn(move || {
        drain_stdout(stdout_pipe, &stdout_buf_t, &tracker_t, forward_stdout_line);
    });

    let stderr_buf_t = Arc::clone(&stderr_buf);
    let build_elapsed = Arc::new(Mutex::new(None::<Duration>));
    let build_elapsed_t = Arc::clone(&build_elapsed);
    let stderr_thread = thread::spawn(move || {
        drain_stderr(
            stderr_pipe,
            &stderr_buf_t,
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
    let watchdog_thread = thread::spawn(move || {
        watchdog_loop(state_root_t, cargo_pid, tracker_t, done_t, hung_t, ceilings, start);
    });

    let status = child.wait().map_err(|e| DevError::Subprocess {
        program: "cargo".into(),
        code: None,
        stderr: e.to_string(),
    })?;
    done.store(true, Ordering::SeqCst);

    stdout_thread.join().ok();
    stderr_thread.join().ok();
    watchdog_thread.join().ok();

    let elapsed = start.elapsed();
    let stdout = clone_buffer(&stdout_buf, "stdout")?;
    let stderr = clone_buffer(&stderr_buf, "stderr")?;
    let hung_outcome = clone_hung(&hung)?;
    let build_elapsed = build_elapsed
        .lock()
        .map_err(|_| DevError::Build("build_elapsed mutex poisoned".into()))?
        .take();
    let completed = tracker
        .lock()
        .map(|mut t| std::mem::take(&mut t.completed))
        .map_err(|_| DevError::Build("test tracker mutex poisoned".into()))?;

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
    })
}

/// Outcome of a parallel test sweep.
pub(crate) struct ParallelRun {
    pub captured: CapturedOutput,
    /// A per-test hang caught by the watchdog (blamed test + snapshot), or
    /// `Completed` if none tripped. Same shape as the serial runner.
    pub outcome: LibtestOutcome,
    /// True when the coarse whole-sweep backstop was hit and the process group
    /// killed (an un-attributable wedge, not a single hung test).
    pub timed_out: bool,
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
/// in-flight; it also honours a cooperative `brokkr kill` / Ctrl-C.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_libtest_parallel<Out, Err, Fin>(
    program: &str,
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
    let mut child = spawn_process_group(program, args, cwd, env)?;
    // Spawned with `process_group(0)`, so the child's pid is its pgid.
    let cargo_pid = child.id();

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

    let stdout_buf_t = Arc::clone(&stdout_buf);
    let tracker_t = Arc::clone(&tracker);
    let stdout_thread = thread::spawn(move || {
        drain_libtest_json(stdout_pipe, &stdout_buf_t, &tracker_t, forward_stdout_line);
    });
    let stderr_buf_t = Arc::clone(&stderr_buf);
    let stderr_thread = thread::spawn(move || {
        drain_stderr(
            stderr_pipe,
            &stderr_buf_t,
            forward_stderr_line,
            start,
            on_build_finished,
        );
    });

    // The per-test hang watchdog - identical to the serial path, aging the
    // JSON-fed tracker. It names an in-flight test that crosses
    // `per_test_timeout`. Attribution only: this path already has an
    // independent whole-sweep clock in the `try_wait` loop below, which is the
    // authoritative bound, so the watchdog carries no `wall` of its own.
    let state_root_t = state_root.to_path_buf();
    let tracker_w = Arc::clone(&tracker);
    let done_w = Arc::clone(&done);
    let hung_w = Arc::clone(&hung);
    let watchdog_thread = thread::spawn(move || {
        watchdog_loop(
            state_root_t,
            cargo_pid,
            tracker_w,
            done_w,
            hung_w,
            Ceilings { per_test: per_test_timeout, wall: None, shape: WallShape::SharedHarness },
            start,
        );
    });

    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let overtime = start.elapsed() >= timeout;
                let cancelled = abort
                    .as_ref()
                    .is_some_and(|a| a.load(Ordering::SeqCst));
                if overtime || cancelled || crate::shutdown::is_shutdown_requested() {
                    timed_out = overtime;
                    kill_process_group(cargo_pid).ok();
                    let status = child.wait().map_err(|e| DevError::Subprocess {
                        program: program.into(),
                        code: None,
                        stderr: e.to_string(),
                    })?;
                    break status;
                }
                thread::sleep(WATCHDOG_POLL);
            }
            Err(e) => {
                return Err(DevError::Subprocess {
                    program: program.into(),
                    code: None,
                    stderr: e.to_string(),
                });
            }
        }
    };
    done.store(true, Ordering::SeqCst);

    stdout_thread.join().ok();
    stderr_thread.join().ok();
    watchdog_thread.join().ok();

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
        timed_out,
        completed,
    })
}

/// Line-buffered drain of the parallel runner's stdout, which is libtest's
/// JSON event stream (plus, in `--json` mode, cargo's own `{"reason":...}`
/// message lines).
///
/// Each line is fed to a [`JsonReconstructor`], which updates the shared
/// [`TestTracker`] (so the watchdog can age in-flight tests) and turns the JSON
/// events back into the human libtest text the downstream cargo parsers expect.
/// The reconstructed text - not the raw JSON - is what lands in `buf`; cargo
/// message lines and any non-JSON output pass through verbatim.
fn drain_libtest_json<F>(
    mut pipe: ChildStdout,
    buf: &Mutex<Vec<u8>>,
    tracker: &Mutex<TestTracker>,
    mut forward_line: F,
) where
    F: FnMut(&str),
{
    let mut recon = JsonReconstructor::default();
    let emit = |out: &[String], buf: &Mutex<Vec<u8>>, forward_line: &mut F| {
        for text in out {
            if let Ok(mut b) = buf.lock() {
                b.extend_from_slice(text.as_bytes());
                b.push(b'\n');
            }
            forward_line(text);
        }
    };

    let mut read_buf = [0_u8; 4096];
    let mut line = Vec::<u8>::new();
    while let Ok(n) = pipe.read(&mut read_buf) {
        if n == 0 {
            break;
        }
        for &byte in &read_buf[..n] {
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let out = recon.observe(&String::from_utf8_lossy(&line), tracker);
                emit(&out, buf, &mut forward_line);
                line.clear();
            } else {
                line.push(byte);
            }
        }
    }
    if !line.is_empty() {
        let out = recon.observe(&String::from_utf8_lossy(&line), tracker);
        emit(&out, buf, &mut forward_line);
    }
}

/// Turns libtest's JSON event stream back into the human libtest text the
/// downstream cargo parsers ([`crate::cargo_filter::parse_test_output`],
/// [`crate::cargo_filter::filter_test`]) expect, while driving the
/// [`TestTracker`] the watchdog ages.
///
/// libtest (nightly, `--format json`) emits one JSON object per line:
/// - `{"type":"suite","event":"started","test_count":N}`
/// - `{"type":"test","event":"started","name":"..."}`
/// - `{"type":"test","event":"ok"|"failed"|"ignored","name":"...","stdout":"..."}`
/// - `{"type":"suite","event":"ok"|"failed","passed":P,"failed":F,...}`
///
/// Failing tests are buffered until the suite summary, then rendered as
/// libtest's two-part `failures:` block (detail `---- name stdout ----` blocks
/// followed by the name list) so the parser recovers the panic messages exactly
/// as it does from a serial run.
#[derive(Default)]
struct JsonReconstructor {
    /// (test name, captured stdout) for failures seen since the last suite
    /// summary, held back so they render as one block in libtest order.
    failures: Vec<(String, Option<String>)>,
}

/// Split a stdout line into any leading test output and a trailing libtest
/// event, or `None` when the line carries no event.
///
/// # Why a line can carry both
///
/// libtest writes each JSON record, newline included, as one write, and reserves
/// no boundary against whatever preceded it. A test that prints without a
/// trailing newline - `print!("breadcrumb")` - therefore produces
/// `breadcrumb{"type":"test","event":"ok",...}` on a single line. Requiring the
/// line to *start* with `{` dropped that event on the floor: the test stayed in
/// the tracker as in-flight, its duration inflated to the next transition, and
/// the per-test clock aged the wrong thing. This is reachable without
/// `--nocapture` and without malice, because libtest's capture installs Rust's
/// capture mechanism rather than redirecting the process's stdout descriptor, so
/// a `write!(std::io::stdout(), ..)` or a subprocess that inherited the
/// descriptor writes straight past it.
///
/// # Why right to left
///
/// Candidate boundaries are tried from the last `{` backwards. If test output
/// itself ends in JSON immediately before the real record, only the real
/// record's `{` parses cleanly through end-of-line, so the later boundary is the
/// correct one. Scanning left to right would hand the earlier, wrong object back.
///
/// # What is deliberately not attempted
///
/// A candidate is accepted only if the suffix parses whole as one JSON value and
/// carries a *recognised* libtest `type`/`event` pair. Arbitrary JSON is not an
/// event merely because it has a `type` field, and cargo's own
/// `--message-format=json` records (`reason`, no `type`) pass through untouched.
///
/// This cannot be made exact. A test can print a byte-for-byte valid libtest
/// record on its own line, and no amount of scanning distinguishes that from the
/// real thing - the limitation is in sharing one stream between a protocol and
/// arbitrary output, not in this function. It is tolerable precisely because
/// these events no longer decide when anything is killed: being wrong costs a
/// wrong name in a report, not a terminated run.
fn split_trailing_event(line: &str) -> Option<(&str, Value)> {
    if !line.contains('{') {
        return None;
    }
    let mut candidates: Vec<usize> = line.match_indices('{').map(|(i, _)| i).collect();
    candidates.reverse();
    for start in candidates {
        let Ok(val) = serde_json::from_str::<Value>(line[start..].trim_end()) else {
            continue;
        };
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        let event = val.get("event").and_then(Value::as_str).unwrap_or("");
        if !is_known_libtest_event(kind, event) {
            // Parsed, but not one of ours. Do not keep scanning left for a
            // "better" object: a valid non-event JSON tail means the line is not
            // an event line.
            return None;
        }
        // Verbatim, deliberately not trimmed: trailing spaces before the record
        // are the test's own output, and downstream status-line filtering
        // classifies a prefix by its exact text.
        return Some((&line[..start], val));
    }
    None
}

/// The `type`/`event` pairs libtest actually emits. Anything else is not an
/// event, however JSON-shaped.
fn is_known_libtest_event(kind: &str, event: &str) -> bool {
    matches!(
        (kind, event),
        ("suite", "started" | "ok" | "failed")
            | ("test", "started" | "ok" | "failed" | "ignored" | "timeout")
    )
}

impl JsonReconstructor {
    /// Consume one raw stdout line, updating `tracker` and returning the human
    /// libtest lines to emit for it (possibly none, or several).
    fn observe(&mut self, line: &str, tracker: &Mutex<TestTracker>) -> Vec<String> {
        let Some((prefix, val)) = split_trailing_event(line) else {
            // No libtest event on this line: test output, a stray print, or
            // cargo's own `--message-format=json` record (which has `reason`
            // and no `type`). Pass through untouched.
            return vec![line.to_owned()];
        };
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        let event = val.get("event").and_then(Value::as_str).unwrap_or("");
        // Output that ran into the event keeps its own line, verbatim and first.
        let mut out = if prefix.is_empty() { Vec::new() } else { vec![prefix.to_owned()] };
        out.extend(self.observe_event(kind, event, &val, tracker));
        out
    }

    /// Handle one recognised libtest event.
    fn observe_event(
        &mut self,
        kind: &str,
        event: &str,
        val: &Value,
        tracker: &Mutex<TestTracker>,
    ) -> Vec<String> {
        match (kind, event) {
            ("suite", "started") => {
                self.failures.clear();
                // Execution has begun: the per-test cap now bounds the whole run,
                // so a lost start record cannot buy a test the idle ceiling.
                if let Ok(mut t) = tracker.lock() {
                    t.observe_suite_start();
                }
                let count = val.get("test_count").and_then(Value::as_u64).unwrap_or(0);
                vec![String::new(), format!("running {count} tests")]
            }
            ("suite", _) => self.render_suite_summary(val, event),
            ("test", "started") => {
                if let Some(name) = val.get("name").and_then(Value::as_str)
                    && let Ok(mut t) = tracker.lock()
                {
                    t.observe_start(name.to_owned());
                }
                Vec::new()
            }
            ("test", "ok") => {
                let name = self.finish(val, tracker);
                name.map(|n| vec![format!("test {n} ... ok")]).unwrap_or_default()
            }
            ("test", "ignored") => {
                let name = self.finish(val, tracker);
                name.map(|n| vec![format!("test {n} ... ignored")])
                    .unwrap_or_default()
            }
            ("test", "failed") => {
                let Some(name) = self.finish(val, tracker) else {
                    return Vec::new();
                };
                let captured = val
                    .get("stdout")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.failures.push((name.clone(), captured));
                vec![format!("test {name} ... FAILED")]
            }
            _ => Vec::new(),
        }
    }

    /// Clear a completed test from the tracker and return its name.
    fn finish(&self, val: &Value, tracker: &Mutex<TestTracker>) -> Option<String> {
        let name = val.get("name").and_then(Value::as_str)?.to_owned();
        if let Ok(mut t) = tracker.lock() {
            t.observe_result(&name);
        }
        Some(name)
    }

    /// Render the end-of-suite `failures:` block (if any) plus the
    /// `test result:` summary line from a `suite` `ok`/`failed` event.
    fn render_suite_summary(&mut self, val: &Value, event: &str) -> Vec<String> {
        let count = |key: &str| val.get(key).and_then(Value::as_u64).unwrap_or(0);
        let (passed, failed) = (count("passed"), count("failed"));
        let (ignored, measured) = (count("ignored"), count("measured"));
        let filtered_out = count("filtered_out");
        let exec_time = val.get("exec_time").and_then(Value::as_f64).unwrap_or(0.0);

        let mut out = Vec::new();
        if !self.failures.is_empty() {
            out.push(String::new());
            out.push("failures:".to_owned());
            for (name, captured) in &self.failures {
                if let Some(cap) = captured
                    && !cap.is_empty()
                {
                    out.push(String::new());
                    out.push(format!("---- {name} stdout ----"));
                    for l in cap.lines() {
                        out.push(l.to_owned());
                    }
                }
            }
            out.push(String::new());
            out.push("failures:".to_owned());
            for (name, _) in &self.failures {
                out.push(format!("    {name}"));
            }
            out.push(String::new());
        }
        self.failures.clear();

        let verb = if failed > 0 || event == "failed" {
            "FAILED"
        } else {
            "ok"
        };
        out.push(format!(
            "test result: {verb}. {passed} passed; {failed} failed; \
             {ignored} ignored; {measured} measured; {filtered_out} filtered out; \
             finished in {exec_time:.2}s"
        ));
        out
    }
}

fn spawn_cargo_process_group(
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<std::process::Child, DevError> {
    spawn_process_group("cargo", args, cwd, env)
}

/// Spawn `program` in its own process group with piped output. The general
/// form behind [`spawn_cargo_process_group`]: the parallel test lane executes
/// prebuilt test binaries directly (no cargo re-entry), so the program is a
/// parameter rather than a constant.
fn spawn_process_group(
    program: &str,
    args: &[&str],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Result<std::process::Child, DevError> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    for &(key, value) in env {
        cmd.env(key, value);
    }
    crate::oom::protect_child(&mut cmd);
    // Last, after every caller-supplied env var: see `crate::hold`.
    crate::hold::stamp(&mut cmd);

    cmd.spawn().map_err(|e| DevError::Subprocess {
        program: program.into(),
        code: None,
        stderr: e.to_string(),
    })
}

fn clone_buffer(buf: &Arc<Mutex<Vec<u8>>>, label: &str) -> Result<Vec<u8>, DevError> {
    buf.lock()
        .map(|v| v.clone())
        .map_err(|_| DevError::Build(format!("{label} buffer poisoned")))
}

fn clone_hung(hung: &Arc<Mutex<Option<HungTest>>>) -> Result<Option<HungTest>, DevError> {
    hung.lock()
        .map(|h| h.clone())
        .map_err(|_| DevError::Build("hung-test state poisoned".into()))
}

/// State of partial-marker tracking under `--nocapture --test-threads=1`.
///
/// Libtest writes `test NAME ... ` (no newline, flushed) before running
/// each test, then the test's own stdout glues onto that partial line,
/// then libtest writes the bare status (`ok\n`/`FAILED\n`/`ignored\n`,
/// optionally with a `<X.Xs>` suffix when `--report-time` is set).
/// We strip the partial marker and track which name is still running
/// so the watchdog can age it; the trailing bare-status line, when
/// it arrives unambiguously (no preceding test output), is consumed
/// as the terminator.
///
/// The earlier implementation gated partial-marker detection on
/// "no pending test" and cleared pending unconditionally on the first
/// bare-status-shaped line. Two failure modes:
/// - `print!("hi")` glues with libtest's `ok\n` -> arrives as `hiok`,
///   not bare status; pending never cleared; the *next* test's partial
///   marker is then ignored (gated out), and the watchdog blames the
///   wrong test.
/// - `println!("ok")` arrives as a real bare-status line *before*
///   libtest's terminator; pending cleared early, real hang in same
///   test goes unnoticed.
///
/// The state machine here fixes Trigger A (the next-start-marker case)
/// and narrows Trigger B (`intermediate_output_seen` suppresses the
/// bare-status shortcut once any non-status output has flowed).
#[derive(Default)]
enum PartialState {
    #[default]
    Idle,
    AwaitingTerminator {
        name: String,
        /// True once a non-blank, non-status-shaped line has been
        /// forwarded for this pending test. Suppresses the
        /// bare-status terminator shortcut: at that point the next
        /// `ok`/`FAILED`/`ignored` line is more likely test output
        /// than libtest framing, so we wait for the *next* partial
        /// start marker (or `test result:` summary) to clear instead.
        intermediate_output_seen: bool,
    },
}

fn drain_stdout<F>(
    mut pipe: ChildStdout,
    buf: &Mutex<Vec<u8>>,
    tracker: &Mutex<TestTracker>,
    mut forward_line: F,
) where
    F: FnMut(&str),
{
    let mut read_buf = [0_u8; 4096];
    let mut line = Vec::<u8>::new();
    let mut state = PartialState::Idle;

    while let Ok(n) = pipe.read(&mut read_buf) {
        if n == 0 {
            break;
        }
        if let Ok(mut out) = buf.lock() {
            out.extend_from_slice(&read_buf[..n]);
        }
        for &byte in &read_buf[..n] {
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                handle_stdout_line(&line, tracker, &mut forward_line, true, &mut state);
                line.clear();
            } else {
                line.push(byte);
                // Detect the partial `test NAME ... ` start marker before
                // the newline arrives, so the watchdog can age the test
                // even if it never produces output. Note: no `Idle`-only
                // gate - a new start marker while a previous test is
                // pending is the strongest signal that the previous test
                // ended without an explicit terminator (Trigger A).
                if byte == b' '
                    && line.len() >= "test x ... ".len()
                    && let Ok(text) = std::str::from_utf8(&line)
                    && let Some(name) = parse_start_marker(text)
                {
                    if let PartialState::AwaitingTerminator { name: prev, .. } = &state
                        && let Ok(mut t) = tracker.lock()
                    {
                        t.observe_result(prev);
                    }
                    if let Ok(mut t) = tracker.lock() {
                        t.observe_start(name.clone());
                    }
                    state = PartialState::AwaitingTerminator {
                        name,
                        intermediate_output_seen: false,
                    };
                    line.clear();
                }
            }
        }
    }

    if !line.is_empty() {
        handle_stdout_line(&line, tracker, &mut forward_line, false, &mut state);
    }
}

fn drain_stderr<F, G>(
    mut pipe: ChildStderr,
    buf: &Mutex<Vec<u8>>,
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

    while let Ok(n) = pipe.read(&mut read_buf) {
        if n == 0 {
            break;
        }
        if let Ok(mut out) = buf.lock() {
            out.extend_from_slice(&read_buf[..n]);
        }
        for &byte in &read_buf[..n] {
            if byte == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let text = String::from_utf8_lossy(&line).into_owned();
                if on_build_finished.is_some() && is_cargo_finished_line(&text)
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

fn is_cargo_finished_line(line: &str) -> bool {
    line.trim_start().starts_with("Finished ")
}

fn handle_stdout_line<F>(
    line: &[u8],
    tracker: &Mutex<TestTracker>,
    forward_line: &mut F,
    terminated: bool,
    state: &mut PartialState,
) where
    F: FnMut(&str),
{
    let text = String::from_utf8_lossy(line).into_owned();

    // `running N tests` is libtest announcing that execution has begun. From
    // here the per-test cap bounds the whole run, so a lost `test NAME ... `
    // start marker cannot buy a test the five-minute idle ceiling instead of its
    // twenty seconds.
    if text.starts_with("running ") && text.contains(" test")
        && let Ok(mut t) = tracker.lock()
    {
        t.observe_suite_start();
    }

    // libtest's per-suite summary is the universal pending-clear:
    // `test result: ok. ...` or `test result: FAILED. ...`. Match
    // both verb forms specifically so a test that does
    // `println!("test result:")` doesn't accidentally clear pending
    // and leave the watchdog blind to a subsequent hang.
    if is_libtest_result_summary(&text) {
        if let PartialState::AwaitingTerminator { name, .. } = state
            && let Ok(mut t) = tracker.lock()
        {
            t.observe_result(name);
        }
        *state = PartialState::Idle;
    }

    if let PartialState::AwaitingTerminator { name: _, intermediate_output_seen } = state {
        if !*intermediate_output_seen && is_bare_status_line(&text) {
            // No test output has been seen yet, and the line looks like
            // libtest's terminator (`ok` / `FAILED` / `ignored`,
            // optionally `<X.Xs>`). Two real shapes match here:
            //
            // 1. The legitimate libtest terminator for a silent test.
            // 2. A test whose *first* println! was literally one of
            //    those words - the watchdog can't tell which.
            //
            // Drop the line from display either way (so we don't print
            // a stray `ok` next to libtest's real one), but DO NOT
            // call observe_result. If the test then hangs after
            // `println!("ok")`, the watchdog must still fire. Pending
            // is cleared by either the next `test NAME ... ` start
            // marker or by the `test result:` summary - both happen
            // well inside the watchdog timeout for a normal completion.
            return;
        }
        if !text.trim().is_empty() {
            *intermediate_output_seen = true;
        }
    }

    if let Some(name) = parse_result_marker(&text)
        && let Ok(mut t) = tracker.lock()
    {
        t.observe_result(&name);
    }

    if terminated || parse_start_marker(&text).is_none() {
        forward_line(&text);
    }
}

/// True for libtest's per-suite summary line.
///
/// Libtest emits exactly one of:
/// - `test result: ok. N passed; M failed; ...`
/// - `test result: FAILED. N passed; M failed; ...`
///
/// Match both verbs explicitly so a user `println!("test result:")`
/// in test output cannot accidentally clear the watchdog's pending
/// state - which would silently let a subsequent hang go undetected.
fn is_libtest_result_summary(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("test result: ok.") || t.starts_with("test result: FAILED.")
}

/// True for libtest's standalone status lines (`ok`, `FAILED`, `ignored`,
/// optionally followed by ` <X.Xs>` when `--report-time` is enabled).
/// Also used by `test_cmd`'s display condenser: under `--nocapture` the
/// verdict lands on its own line after the test's output, so the
/// full-line `test NAME ... FAILED` filter never sees it.
pub(crate) fn is_bare_status_line(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(head) = parts.next() else {
        return false;
    };
    if !matches!(head, "ok" | "FAILED" | "ignored") {
        return false;
    }
    // Allow at most one trailing token, and only if it looks like a
    // libtest timing suffix: `<0.001s>`.
    match parts.next() {
        None => true,
        Some(tail) => {
            parts.next().is_none()
                && tail.starts_with('<')
                && tail.ends_with("s>")
        }
    }
}

fn parse_start_marker(line: &str) -> Option<String> {
    let rest = line.strip_prefix("test ")?;
    let name = rest.strip_suffix(" ... ")?;
    (!name.is_empty()).then(|| name.to_owned())
}

fn parse_result_marker(line: &str) -> Option<String> {
    let rest = line.strip_prefix("test ")?;
    let (name, status) = rest.rsplit_once(" ... ")?;
    let is_result = ["ok", "FAILED", "ignored"]
        .iter()
        .any(|s| status == *s || status.strip_prefix(s).is_some_and(|tail| tail.starts_with(' ')));
    (is_result && !name.is_empty()).then(|| name.to_owned())
}

#[allow(clippy::needless_pass_by_value)] // The Arcs are moved into a spawned thread.
fn watchdog_loop(
    state_root: PathBuf,
    cargo_pid: u32,
    tracker: Arc<Mutex<TestTracker>>,
    done: Arc<AtomicBool>,
    hung: Arc<Mutex<Option<HungTest>>>,
    ceilings: Ceilings,
    started: Instant,
) {
    watchdog_loop_with_timing(
        state_root,
        cargo_pid,
        tracker,
        done,
        hung,
        ceilings,
        WATCHDOG_POLL,
        started,
    );
}

#[allow(clippy::needless_pass_by_value)] // The Arcs are moved into a spawned thread.
#[allow(clippy::too_many_arguments)]
fn watchdog_loop_with_timing(
    state_root: PathBuf,
    cargo_pid: u32,
    tracker: Arc<Mutex<TestTracker>>,
    done: Arc<AtomicBool>,
    hung: Arc<Mutex<Option<HungTest>>>,
    ceilings: Ceilings,
    poll: Duration,
    // `started` is the run's start, captured by the CALLER before it spawned the
    // child. Passed in rather than taken here: a fresh `Instant::now()` inside
    // this thread begins after the child and the reader threads are already
    // running, so the bound would silently exclude spawn and thread-start
    // latency while the comments claimed it ran "from spawn".
    started: Instant,
) {
    let timeout = ceilings.per_test;
    // Emitted at most once, so a long sweep does not repeat the same guess every
    // poll tick.
    let mut warned = false;
    loop {
        if done.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(poll);
        if done.load(Ordering::SeqCst) {
            return;
        }

        // THE PER-TEST CAP TERMINATES. Every test brokkr runs gets `per_test`
        // of wall time and no more; exceeding it fails the run and stops it. The
        // only exception is `brokkr test --timeout`, which raises this cap for one
        // resolved test and is itself hard-capped at 280s by the CLI parser.
        //
        // The cap is enforced even though the clock that measures it is fed by a
        // stream test output shares, and that is not an oversight. Either the
        // named test really has burned its budget, or its terminal event was lost
        // and brokkr has seen NO test complete for longer than the budget - in
        // which case the run has still exceeded what the contract allows, and
        // failing is right either way. What a lost event can corrupt is the NAME,
        // not the entitlement to kill, so the report says so rather than
        // pretending the attribution is proven.
        let per_test_expiry = tracker.lock().ok().and_then(|t| {
            t.timed_out(timeout).map(|(name, elapsed)| {
                let stale = !t.ever_observed;
                (name, elapsed, stale)
            })
        });

        // The idle ceiling covers the gap the per-test cap cannot see: no test in
        // flight at all, so there is no budget to charge. While
        // `ever_observed` is false nothing has been parsed, so `idle_since` still
        // holds the run's start and this is plain wall time since spawn. The case
        // it was built for is cargo parked on a build-directory lock with no test
        // ever announced.
        let idle_expiry = tracker
            .lock()
            .ok()
            .filter(|t| t.current.is_empty())
            .map(|t| t.idle_since.elapsed())
            .filter(|e| *e >= IDLE_TIMEOUT);

        // The caller's wall ceiling: a backstop for a wedge that neither clock
        // above can charge to anything.
        let wall_expiry = ceilings.wall.filter(|w| started.elapsed() >= *w);

        let verdict = if let Some((name, elapsed, stale)) = per_test_expiry {
            if name == IDLE_WEDGE {
                Some((TimeoutReason::Idle, elapsed, IDLE_TIMEOUT))
            } else {
                // A one-test invocation is named by the caller; a shared harness
                // can only offer the tracker's best-known suspect.
                let named = match &ceilings.shape {
                    WallShape::OneTest { name } => name.clone(),
                    WallShape::SharedHarness => name,
                };
                if stale && !warned {
                    warned = true;
                    crate::output::warn(
                        "no libtest lifecycle event was ever observed, so the test named below is \
                         brokkr's best guess at which one burned the budget - the budget itself \
                         was still exceeded.",
                    );
                }
                Some((TimeoutReason::PerTest { name: named }, elapsed, timeout))
            }
        } else if let Some(elapsed) = idle_expiry {
            Some((TimeoutReason::Idle, elapsed, IDLE_TIMEOUT))
        } else if let Some(wall) = wall_expiry {
            let blamed = tracker
                .lock()
                .ok()
                .map(|t| t.current.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            match &ceilings.shape {
                WallShape::OneTest { name } => Some((
                    TimeoutReason::PerTest { name: name.clone() },
                    started.elapsed(),
                    wall,
                )),
                WallShape::SharedHarness => {
                    Some((TimeoutReason::SweepWall { blamed }, started.elapsed(), wall))
                }
            }
        } else {
            None
        };

        let Some((reason, elapsed, ceiling)) = verdict else {
            continue;
        };
        if done.load(Ordering::SeqCst) {
            return;
        }

        let hung_test = capture_hung_test(&state_root, cargo_pid, reason, elapsed, ceiling);
        if let Ok(mut slot) = hung.lock() {
            *slot = Some(hung_test);
        }
        kill_process_group(cargo_pid).ok();
        return;
    }
}

fn capture_hung_test(
    state_root: &Path,
    cargo_pid: u32,
    reason: TimeoutReason,
    elapsed: Duration,
    ceiling: Duration,
) -> HungTest {
    let test = reason.label();
    let test = test.as_str();
    let test_pids = direct_child_pids(cargo_pid);
    let snapshot_pid = test_pids.first().copied().or(Some(cargo_pid));
    // Diagnostic snapshots are brokkr-owned state, so they live under the
    // config-dir `project_root` (`state_root`), not the code tree cargo runs
    // in - the two differ when brokkr.toml is one level above a foreign
    // checkout, and the foreign repo must stay clean.
    let snapshot_dir = state_root
        .join(".brokkr")
        .join("test-hung")
        .join(format!(
            "{}-{}-{}",
            unix_secs(),
            cargo_pid,
            sanitize_path_component(test)
        ));

    let mut snapshot_error = None;
    if let Err(err) = fs::create_dir_all(&snapshot_dir) {
        snapshot_error = Some(err.to_string());
    } else if let Some(pid) = snapshot_pid
        && let Err(err) = snapshot_proc(pid, &snapshot_dir)
    {
        snapshot_error = Some(err.to_string());
    }

    let wchan = snapshot_error
        .is_none()
        .then(|| first_line(snapshot_dir.join("proc-wchan.txt")))
        .flatten();
    let stack = snapshot_error
        .is_none()
        .then(|| first_line(snapshot_dir.join("proc-stack.txt")))
        .flatten();

    HungTest {
        reason,
        test: test.to_owned(),
        elapsed,
        ceiling,
        snapshot_dir,
        cargo_pid,
        test_pids,
        snapshot_pid,
        wchan,
        stack,
        snapshot_error,
    }
}

fn kill_process_group(pgid: u32) -> Result<(), DevError> {
    let pgid = i32::try_from(pgid)
        .map_err(|_| DevError::Build(format!("process group id {pgid} does not fit pid_t")))?;
    let target: libc::pid_t = -pgid;
    let ret = unsafe { libc::kill(target, libc::SIGKILL) };
    if ret == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(DevError::Io(err))
}

fn direct_child_pids(parent: u32) -> Vec<u32> {
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

fn first_line(path: PathBuf) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn sanitize_path_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(120));
    let mut prev_sep = false;
    for ch in name.chars() {
        let keep = ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_');
        if keep {
            out.push(ch);
            prev_sep = false;
        } else if !prev_sep {
            out.push('_');
            prev_sep = true;
        }
        if out.len() >= 120 {
            break;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "unknown".into()
    } else {
        trimmed.to_owned()
    }
}

pub(crate) fn format_hung_test(hung: &HungTest, cwd: &Path) -> String {
    let snapshot_path = hung
        .snapshot_dir
        .strip_prefix(cwd)
        .unwrap_or(&hung.snapshot_dir)
        .display()
        .to_string();
    let child_text = match hung.test_pids.as_slice() {
        [] => "no test child found".to_owned(),
        [pid] => format!("test child (pid {pid})"),
        pids => format!(
            "test children (pids {})",
            pids.iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    };
    let proc_pid = hung
        .snapshot_pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "?".into());
    let wchan = hung.wchan.as_deref().unwrap_or("unavailable");
    let stack = hung.stack.as_deref().unwrap_or("unavailable");
    let snapshot_line = match &hung.snapshot_error {
        Some(err) => format!("snapshot failed: {err}"),
        None => format!("full snapshot: {snapshot_path}"),
    };

    let headline = if let TimeoutReason::SweepWall { blamed } = &hung.reason {
        // No offender is named, because none is known: the deadline expiring
        // says nothing about which test caused it, and it can just as easily
        // expire during the hundredth healthy test. Whatever was in flight is
        // reported as context, explicitly unverified.
        let context = match blamed.as_slice() {
            [] => "no test was in flight".to_owned(),
            names => format!("in flight when it expired (unverified): {}", names.join(", ")),
        };
        format!(
            "the run exceeded its {}s wall deadline after {}s and was killed\n  \
             wall deadline: the authoritative bound - it is measured from a clock nothing the \
             tests print can reach, so unlike the per-test ceiling it cannot be misled by output \
             that swallows a libtest record\n  {context}\n  \
             note: \"in flight when the deadline expired\" is not \"caused the timeout\"",
            hung.ceiling.as_secs(),
            hung.elapsed.as_secs(),
        )
    } else if matches!(hung.reason, TimeoutReason::Idle) {
        format!(
            "no test in flight for {}s, exceeding the {}s idle ceiling - cargo wedged before the first test or after the last (a build-directory lock held by another cargo, e.g. rust-analyzer, looks exactly like this)\n  idle ceiling: covers the compile, link and teardown the per-test timeout cannot see",
            hung.elapsed.as_secs(),
            hung.ceiling.as_secs(),
        )
    } else {
        format!(
            "test {} ran {}s, exceeding the {}s per-test timeout, after libtest started it\n  per-test timeout: cargo build time excluded",
            hung.test,
            hung.elapsed.as_secs(),
            hung.ceiling.as_secs(),
        )
    };
    format!(
        "{headline}\n  killed cargo process group (pgid {}) and {}\n  /proc/{}/wchan: {}\n  /proc/{}/stack: {}\n  {}",
        hung.cargo_pid,
        child_text,
        proc_pid,
        wchan,
        proc_pid,
        stack,
        snapshot_line,
    )
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
            "libtest watchdog requires --test-threads=1, but no --test-threads flag was passed".into(),
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used
    )]

    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    /// Drive `drain_stdout` against a synthetic libtest stream and
    /// return (forwarded_lines, names_observed_finished).
    /// The stream is the concatenation of the byte slices the libtest
    /// stdout pipe would have produced, in chunk order. The tracker
    /// records `observe_start` / `observe_result` calls so we can
    /// inspect which tests the watchdog still believes are running.
    fn drive_drain(chunks: &[&[u8]]) -> (Vec<String>, Vec<String>) {
        use std::io::Write;
        // Build a real pipe: one end gets the chunks, the other is fed
        // to drain_stdout. This exercises the same byte-by-byte path as
        // production and avoids forking the loop under test.
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cat");
        let mut stdin = child.stdin.take().expect("cat stdin");
        let stdout = child.stdout.take().expect("cat stdout");

        let chunks_owned: Vec<Vec<u8>> = chunks.iter().map(|c| c.to_vec()).collect();
        let writer = std::thread::spawn(move || {
            for c in &chunks_owned {
                stdin.write_all(c).ok();
            }
            drop(stdin);
        });

        let buf = Mutex::new(Vec::<u8>::new());
        let tracker = Mutex::new(TestTracker::default());
        let forwarded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let forwarded_t = Arc::clone(&forwarded);

        drain_stdout(stdout, &buf, &tracker, move |line: &str| {
            forwarded_t.lock().unwrap().push(line.to_owned());
        });

        writer.join().ok();
        child.wait().ok();

        // Names still in the tracker are tests the watchdog believes
        // are running. Names absent from the tracker had observe_result
        // called - they're "finished" from the watchdog's view.
        let still_running: Vec<String> =
            tracker.lock().unwrap().current.keys().cloned().collect();
        let forwarded = forwarded.lock().unwrap().clone();
        (forwarded, still_running)
    }

    /// Like [`drive_drain`] but returns the names the tracker recorded as
    /// *completed* (in order) - the exact set that feeds `check --timings`.
    /// `drive_drain` only exposes `current` (the hang watchdog's view), so
    /// nothing else pins down the timing capture.
    fn drive_drain_completed(chunks: &[&[u8]]) -> Vec<String> {
        use std::io::Write;
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cat");
        let mut stdin = child.stdin.take().expect("cat stdin");
        let stdout = child.stdout.take().expect("cat stdout");

        let chunks_owned: Vec<Vec<u8>> = chunks.iter().map(|c| c.to_vec()).collect();
        let writer = std::thread::spawn(move || {
            for c in &chunks_owned {
                stdin.write_all(c).ok();
            }
            drop(stdin);
        });

        let buf = Mutex::new(Vec::<u8>::new());
        let tracker = Mutex::new(TestTracker::default());
        drain_stdout(stdout, &buf, &tracker, |_line: &str| {});

        writer.join().ok();
        child.wait().ok();

        tracker
            .lock()
            .unwrap()
            .completed
            .iter()
            .map(|(name, _)| name.clone())
            .collect()
    }

    #[test]
    fn completed_captures_all_tests_atomic_lines() {
        // Capture ON (no --nocapture): libtest writes each result as one
        // atomic `test NAME ... ok\n` line. Every test - including the last,
        // cleared by the summary - must land in `completed`, or
        // `check --timings` reports "no tests ran" for a green suite.
        let stream: Vec<&[u8]> = vec![
            b"\nrunning 3 tests\n",
            b"test alpha ... ok\n",
            b"test beta ... ok\n",
            b"test gamma ... ok\n",
            b"\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let mut completed = drive_drain_completed(&stream);
        completed.sort();
        assert_eq!(completed, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn completed_captures_all_tests_partial_markers() {
        // Capture OFF (--nocapture): the `test NAME ... ` marker is flushed
        // separately from the trailing `ok\n`. Same guarantee.
        let stream: Vec<&[u8]> = vec![
            b"\nrunning 2 tests\n",
            b"test alpha ... ",
            b"ok\n",
            b"test beta ... ",
            b"ok\n",
            b"\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let mut completed = drive_drain_completed(&stream);
        completed.sort();
        assert_eq!(completed, vec!["alpha", "beta"]);
    }

    #[test]
    fn watchdog_clears_when_test_prints_without_newline() {
        // Trigger A from the review: `print!("hello")` glues with the
        // libtest `ok\n` -> arrives as `hellook\n`. The next test's
        // partial marker must clear the previous pending; otherwise
        // the watchdog times the wrong test.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"hellook\n",
            b"test bar ... ",
            b"ok\n",
            b"\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (forwarded, still_running) = drive_drain(&stream);
        // "hellook" should have been forwarded as test output (intermediate).
        assert!(forwarded.iter().any(|l| l == "hellook"), "got: {forwarded:?}");
        // After the result summary, no test should still be pending.
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn is_libtest_result_summary_matches_real_shapes_only() {
        // Exact libtest summary forms (with leading whitespace tolerated).
        assert!(is_libtest_result_summary(
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
        ));
        assert!(is_libtest_result_summary(
            "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
        ));
        // A user `println!("test result:")` with no verb is NOT a
        // summary - reviewer flagged that the prefix-only check let
        // tests accidentally clear watchdog pending state.
        assert!(!is_libtest_result_summary("test result:"));
        assert!(!is_libtest_result_summary("test result: foo"));
        assert!(!is_libtest_result_summary("test result: ok bar"));
    }

    #[test]
    fn watchdog_keeps_pending_when_test_prints_test_result_prefix_then_hangs() {
        // Item 4 from the second review: a test that prints
        // `println!("test result:")` and then hangs must NOT have
        // pending cleared by the summary detector.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"test result: maybe later\n",
            // ... and then the test hangs.
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert_eq!(still_running, vec!["foo".to_string()]);
    }

    #[test]
    fn watchdog_keeps_pending_when_test_prints_ok_then_hangs() {
        // Reviewer-flagged regression: a test whose *first* output is
        // `println!("ok")` and which then hangs forever must NOT be
        // declared finished by the bare-status shortcut. Previously
        // the watchdog cleared pending on that line and the timeout
        // never fired. With the deferred-observe fix the test stays
        // pending; the real watchdog timer (20s in production, not
        // simulated here) will eventually trigger.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"ok\n",
            // ... and then nothing. EOF here stands in for "the test
            // is still running / hung when the pipe closes".
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert_eq!(still_running, vec!["foo".to_string()]);
    }

    #[test]
    fn watchdog_clears_when_test_prints_ok_literal() {
        // Trigger B fixture: the test calls `println!("ok")` then
        // completes normally. Both the test's `ok` and libtest's
        // own `ok\n` look like bare-status terminators with no
        // intermediate output - the deferred-observe path drops
        // both from display but does *not* clear pending. The
        // `test result:` summary line at the end is what actually
        // clears the watchdog tracker.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"ok\n",
            b"ok\n",
            b"\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_clears_when_test_prints_failed_literal() {
        // Symmetric to the `ok` case: `println!("FAILED")` followed
        // by libtest's own `FAILED\n`. Both bare-status lines are
        // dropped from display without observing a result; the
        // `test result:` summary at the end is the actual
        // pending-clear event.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"FAILED\n",
            b"FAILED\n",
            b"\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_clears_silent_test_via_bare_status() {
        // No test output: stream is `test foo ... ok\n`. The partial
        // start marker is stripped; the bare `ok` line is dropped from
        // display (deferred-observe: we can't tell it from a test's
        // own `println!("ok")`) without clearing pending. The
        // `test result:` summary is what clears the watchdog tracker.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"ok\n",
            b"\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_clears_after_panic_message() {
        // A failing test usually prints panic info on stdout/stderr
        // before libtest writes `FAILED\n`. The state machine must
        // eventually clear pending; the `test result:` summary is the
        // last-resort terminator after intermediate output suppressed
        // the bare-status shortcut.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"thread 'foo' panicked at tests/x.rs:1:1:\n",
            b"assertion failed\n",
            b"FAILED\n",
            b"\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (forwarded, still_running) = drive_drain(&stream);
        assert!(
            forwarded.iter().any(|l| l.contains("panicked")),
            "forwarded: {forwarded:?}"
        );
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_clears_with_report_time_suffix() {
        // libtest with `--report-time` emits `ok <0.001s>\n`. The
        // is_bare_status_line check accepts the `<X.Xs>` suffix, so
        // pending should clear normally even when timing is enabled.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ",
            b"ok <0.001s>\n",
            b"\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn watchdog_full_form_marker_clears_pending() {
        // When `--nocapture` is off, libtest writes the full
        // `test foo ... ok\n` line atomically. parse_result_marker
        // catches it; pending stays empty.
        let stream: Vec<&[u8]> = vec![
            b"test foo ... ok\n",
            b"\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        ];
        let (_forwarded, still_running) = drive_drain(&stream);
        assert!(still_running.is_empty(), "still running: {still_running:?}");
    }

    #[test]
    fn start_marker_allows_spaces_in_doctest_names() {
        let name = parse_start_marker("test src/lib.rs - module::Foo::bar (line 42) ... ")
            .expect("start marker");
        assert_eq!(name, "src/lib.rs - module::Foo::bar (line 42)");
    }

    #[test]
    fn result_marker_allows_report_time_suffix() {
        let name = parse_result_marker("test my_mod::slow ... ok (0.25s)")
            .expect("result marker");
        assert_eq!(name, "my_mod::slow");
    }

    #[test]
    fn result_marker_parses_failed() {
        let name = parse_result_marker("test my_mod::slow ... FAILED")
            .expect("result marker");
        assert_eq!(name, "my_mod::slow");
    }

    #[test]
    fn result_marker_ignores_non_test_lines() {
        assert!(parse_result_marker("hello test my_mod::slow ... ok").is_none());
    }

    #[test]
    fn effective_test_threads_uses_last_value() {
        let args = vec![
            "--test-threads=1".to_owned(),
            "--test-threads".to_owned(),
            "4".to_owned(),
        ];
        assert_eq!(effective_test_threads(&args).unwrap(), Some(4));
    }

    #[test]
    fn sanitize_test_name_for_path_component() {
        assert_eq!(
            sanitize_path_component("src/lib.rs - foo::bar (line 42)"),
            "src_lib.rs_-_foo_bar_line_42"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn idle_wedge_fires_only_with_nothing_in_flight() {
        let long_ago = Instant::now()
            .checked_sub(IDLE_TIMEOUT + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let mut tracker = TestTracker { idle_since: long_ago, ..TestTracker::default() };
        let (name, elapsed) = tracker.timed_out(TEST_TIMEOUT).expect("idle wedge");
        assert_eq!(name, IDLE_WEDGE);
        assert!(elapsed >= IDLE_TIMEOUT);

        // A start marker ends the idle window and switches to per-test aging.
        tracker.observe_start("a::fast".to_owned());
        assert!(tracker.timed_out(TEST_TIMEOUT).is_none());
        tracker.observe_result("a::fast");
        assert!(tracker.timed_out(TEST_TIMEOUT).is_none(), "idle window restarts at the result");
    }

    /// The wall deadline must fire with an entirely empty tracker: no announced
    /// start, no announced result, nothing for either event-driven ceiling to
    /// age. That is the case the whole struct exists for - a run whose lifecycle
    /// events were lost or never emitted must still be bounded, and the bound
    /// must come from a clock no input can reach.
    #[test]
    fn the_wall_deadline_fires_with_no_events_at_all() {
        use std::os::unix::process::CommandExt;

        let root = test_root("watchdog_wall_deadline");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 60 & wait")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn shell");
        let cargo_pid = child.id();

        // Deliberately pristine: `timed_out` would report the idle wedge only
        // after IDLE_TIMEOUT (five minutes), and per-test aging needs a start
        // that never comes. Neither can explain a kill here.
        let tracker = Arc::new(Mutex::new(TestTracker::default()));
        let done = Arc::new(AtomicBool::new(false));
        let hung = Arc::new(Mutex::new(None::<HungTest>));

        watchdog_loop_with_timing(
            root.clone(),
            cargo_pid,
            Arc::clone(&tracker),
            Arc::clone(&done),
            Arc::clone(&hung),
            Ceilings {
                per_test: Duration::from_secs(3600),
                wall: Some(Duration::from_millis(40)),
                shape: WallShape::SharedHarness,
            },
            Duration::from_millis(5),
            Instant::now(),
        );
        child.wait().ok();

        let hung = hung.lock().unwrap().clone().expect("wall deadline must produce a verdict");
        assert_eq!(hung.test, WALL_WEDGE, "the wall deadline names no offender");
        assert_eq!(hung.ceiling, Duration::from_millis(40));
        assert!(
            wait_for_process_group_exit(cargo_pid),
            "the process group must be killed"
        );
    }

    /// THE CONTRACT: every test gets `per_test` of wall time and no more.
    /// Exceeding it kills the run, whatever the wall ceiling still has left.
    ///
    /// The clock is fed by a stream test output shares, and the cap is enforced
    /// anyway. Either the named test really burned its budget, or its terminal
    /// event was lost and brokkr has seen no test complete for longer than the
    /// budget - in which case the run has exceeded what the contract allows
    /// either way. A lost event can corrupt the name, never the entitlement to
    /// kill.
    /// The hole a lost START record opened. With no start, `current` stays empty,
    /// so the per-test clock has nothing to age - and the only remaining bound was
    /// the five-minute idle ceiling, letting a test run for minutes under a
    /// twenty-second contract. Once a suite announces itself, brokkr must see a
    /// completion at least every `per_test`.
    #[test]
    fn a_lost_start_record_does_not_escape_the_per_test_cap() {
        let long_ago = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        let mut tracker = TestTracker::default();
        // A suite announced itself, then nothing: no start, no result. Exactly
        // what a swallowed start marker leaves behind.
        tracker.observe_suite_start();
        tracker.last_progress = long_ago;

        let (name, elapsed) = tracker
            .timed_out(TEST_TIMEOUT)
            .expect("execution under way with no completion must blow the budget");
        assert_eq!(name, NO_COMPLETION, "there is no test to name");
        assert!(elapsed >= TEST_TIMEOUT);

        // And before any suite announces itself there is no test to bill, so the
        // per-test cap must NOT fire - that window belongs to the idle ceiling.
        let fresh = TestTracker { last_progress: long_ago, ..TestTracker::default() };
        assert!(
            fresh.timed_out(TEST_TIMEOUT).is_none_or(|(n, _)| n == IDLE_WEDGE),
            "before execution begins only the idle ceiling applies"
        );
    }

    /// A suite that is slow to reach its first test has broken no budget. Arming
    /// the no-progress clock on the suite event alone killed it: suite at t=0,
    /// first test starting at t=2 and finishing at t=19 was killed at t=20 for
    /// consuming 19 seconds of its 20. An observed START has to count as progress.
    #[test]
    fn a_slow_suite_startup_is_not_a_blown_budget() {
        let mut tracker = TestTracker::default();
        tracker.observe_suite_start();
        // Pretend the suite announced itself 19 seconds ago and the first test
        // started 2 seconds later - so 17 seconds into a 20 second budget.
        tracker.last_progress = Instant::now()
            .checked_sub(Duration::from_secs(19))
            .unwrap_or_else(Instant::now);
        tracker.observe_start("a::first".to_owned());
        assert!(
            tracker.timed_out(TEST_TIMEOUT).is_none(),
            "a test that started 0s ago has spent none of its budget"
        );
    }

    /// The forgery bound. A test can write lifecycle-shaped records to the same
    /// stdout the protocol uses, so a naive clock that any completion refreshes
    /// lets a test re-emit one on a timer and run until the sweep backstop.
    /// Refreshes are once per name, so a forger can spend only as many as there
    /// are distinct names it invents.
    #[test]
    fn a_repeated_forged_completion_cannot_refresh_the_clock() {
        let mut tracker = TestTracker::default();
        tracker.observe_suite_start();
        tracker.observe_start("a::one".to_owned());
        tracker.observe_result("a::one");

        // Wind the clock back past the cap, then replay the same records - which
        // is exactly what a test looping `println!` of a captured event does.
        tracker.last_progress = Instant::now()
            .checked_sub(TEST_TIMEOUT + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        tracker.observe_start("a::one".to_owned());
        tracker.observe_result("a::one");

        let (name, _) = tracker
            .timed_out(TEST_TIMEOUT)
            .expect("a replayed event must not buy more time");
        assert_eq!(name, NO_COMPLETION);
    }

    #[test]
    fn an_expired_per_test_cap_kills_even_with_the_wall_far_away() {
        use std::os::unix::process::CommandExt;

        let root = test_root("watchdog_attribution_never_kills");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 60 & wait")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn shell");
        let cargo_pid = child.id();

        // A test that has apparently been running an hour: the per-test ceiling
        // is long expired, so an implementation that lets attribution terminate
        // kills the group here.
        let long_ago = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        let tracker = Arc::new(Mutex::new(TestTracker {
            current: HashMap::from([("a::apparently_overdue".to_owned(), long_ago)]),
            completed: Vec::new(),
            ever_observed: true,
            executing: true,
            last_progress: Instant::now(),
            seen: HashSet::new(),
            finished: HashSet::new(),
            idle_since: Instant::now(),
        }));
        let done = Arc::new(AtomicBool::new(false));
        let hung = Arc::new(Mutex::new(None::<HungTest>));

        // The wall is an hour away and must not be what saves or condemns this
        // run: the per-test cap alone has to fire.
        watchdog_loop_with_timing(
            root.clone(),
            cargo_pid,
            Arc::clone(&tracker),
            Arc::clone(&done),
            Arc::clone(&hung),
            Ceilings {
                per_test: Duration::from_millis(10),
                wall: Some(Duration::from_secs(3600)),
                shape: WallShape::SharedHarness,
            },
            Duration::from_millis(5),
            Instant::now(),
        );
        child.wait().ok();

        let hung = hung.lock().unwrap().clone().expect("the per-test cap must fire");
        assert_eq!(
            hung.reason,
            TimeoutReason::PerTest { name: "a::apparently_overdue".to_owned() },
            "the cap names the test that burned the budget"
        );
        assert_eq!(hung.ceiling, Duration::from_millis(10));
        assert!(
            wait_for_process_group_exit(cargo_pid),
            "exceeding the per-test cap must kill the run"
        );
    }

    #[test]
    fn watchdog_kills_process_group_and_writes_snapshot() {
        use std::os::unix::process::CommandExt;

        let root = test_root("watchdog_kill_snapshot");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 60 & wait")
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn shell");
        let cargo_pid = child.id();
        let child_pids = wait_for_direct_children(cargo_pid);
        assert!(!child_pids.is_empty(), "shell did not spawn sleep child");

        let started = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let tracker = Arc::new(Mutex::new(TestTracker {
            current: HashMap::from([("watchdog::hangs".to_owned(), started)]),
            completed: Vec::new(),
            ever_observed: true,
            executing: true,
            last_progress: Instant::now(),
            seen: HashSet::new(),
            finished: HashSet::new(),
            idle_since: started,
        }));
        let done = Arc::new(AtomicBool::new(false));
        let hung = Arc::new(Mutex::new(None::<HungTest>));

        watchdog_loop_with_timing(
            root.clone(),
            cargo_pid,
            Arc::clone(&tracker),
            Arc::clone(&done),
            Arc::clone(&hung),
            // One process, one test, named by the caller: the only shape whose
            // verdict names a test, since a shared-harness wall expiry cannot
            // know which test to blame. `wall` is also what kills now, so the
            // ceiling has to be there for this test to exercise a kill at all.
            Ceilings::one_test(Duration::from_millis(20), "watchdog::hangs"),
            Duration::from_millis(5),
            Instant::now(),
        );
        child.wait().ok();

        let hung = hung.lock().unwrap().clone().expect("hung result");
        assert_eq!(hung.test, "watchdog::hangs");
        assert!(hung.snapshot_dir.exists());
        assert!(hung.snapshot_dir.join("proc-status.txt").exists());
        assert!(hung.snapshot_pid.is_some());
        assert!(
            wait_for_process_group_exit(cargo_pid),
            "process group {cargo_pid} survived watchdog kill"
        );
    }

    /// Drive a JSON event stream through the reconstructor, returning the
    /// emitted human lines and the set of tests still in-flight in the tracker
    /// (i.e. what the watchdog would age).
    fn drive_recon(events: &[&str]) -> (Vec<String>, Vec<String>) {
        let tracker = Mutex::new(TestTracker::default());
        let mut recon = JsonReconstructor::default();
        let mut lines = Vec::new();
        for ev in events {
            lines.extend(recon.observe(ev, &tracker));
        }
        let mut in_flight: Vec<String> =
            tracker.lock().unwrap().current.keys().cloned().collect();
        in_flight.sort();
        (lines, in_flight)
    }

    /// The concatenation case. libtest writes a record and its newline as one
    /// write with no boundary against preceding output, so a test that prints
    /// without a trailing newline lands its text on the same line as the next
    /// event. Requiring the line to START with `{` silently dropped that event:
    /// the test stayed in-flight, its duration ran to the next transition, and
    /// the per-test clock aged the wrong thing.
    #[test]
    fn an_event_glued_after_unterminated_output_is_still_consumed() {
        let (lines, in_flight) = drive_recon(&[
            r#"{"type":"suite","event":"started","test_count":1}"#,
            r#"{"type":"test","event":"started","name":"a::one"}"#,
            // `print!("breadcrumb")` with no newline, then libtest's record.
            r#"breadcrumb{"type":"test","name":"a::one","event":"ok"}"#,
            r#"{"type":"suite","event":"ok","passed":1,"failed":0,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.01}"#,
        ]);
        assert!(in_flight.is_empty(), "the glued terminal event must clear the tracker");
        assert!(
            lines.iter().any(|l| l == "breadcrumb"),
            "the test's own output must survive verbatim: {lines:?}"
        );
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let parsed = crate::cargo_filter::parse_test_output(&refs);
        assert_eq!(parsed.passed, 1, "the event must be counted: {lines:?}");
    }

    /// Test output that itself ends in JSON, immediately before a real record.
    /// Only the later boundary parses cleanly to end-of-line, which is why the
    /// scan runs right to left.
    #[test]
    fn json_shaped_test_output_before_a_real_event_is_preserved() {
        let (lines, in_flight) = drive_recon(&[
            r#"{"type":"suite","event":"started","test_count":1}"#,
            r#"{"type":"test","event":"started","name":"a::one"}"#,
            r#"{"cfg":"mine"}{"type":"test","name":"a::one","event":"ok"}"#,
        ]);
        assert!(in_flight.is_empty(), "the real event must still be consumed");
        assert!(
            lines.iter().any(|l| l == r#"{"cfg":"mine"}"#),
            "the test's JSON-shaped output must survive verbatim: {lines:?}"
        );
    }

    /// Neither arbitrary JSON nor cargo's own artifact records are libtest
    /// events, however JSON-shaped they are.
    #[test]
    fn non_libtest_json_is_passed_through_not_interpreted() {
        let (lines, in_flight) = drive_recon(&[
            r#"{"type":"suite","event":"started","test_count":1}"#,
            r#"{"type":"test","event":"started","name":"a::one"}"#,
            // cargo's message-format record: `reason`, no `type`.
            r#"{"reason":"compiler-artifact","package_id":"p"}"#,
            // An object with a `type` that is not one of libtest's.
            r#"{"type":"telemetry","event":"ok","name":"a::one"}"#,
        ]);
        assert_eq!(
            in_flight,
            vec!["a::one"],
            "no impostor may clear a test from the tracker"
        );
        assert!(lines.iter().any(|l| l.contains("compiler-artifact")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("telemetry")), "{lines:?}");
    }

    #[test]
    fn recon_passing_suite_round_trips_to_parseable_text() {
        let (lines, in_flight) = drive_recon(&[
            r#"{"type":"suite","event":"started","test_count":2}"#,
            r#"{"type":"test","event":"started","name":"a::one"}"#,
            r#"{"type":"test","event":"started","name":"a::two"}"#,
            r#"{"type":"test","name":"a::one","event":"ok"}"#,
            r#"{"type":"test","name":"a::two","event":"ok"}"#,
            r#"{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"measured":0,"filtered_out":3,"exec_time":0.01}"#,
        ]);
        assert!(in_flight.is_empty(), "all tests should have finished");
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let parsed = crate::cargo_filter::parse_test_output(&refs);
        assert_eq!(parsed.passed, 2);
        assert_eq!(parsed.failed, 0);
        assert_eq!(parsed.filtered_out, 3);
        assert_eq!(parsed.suites, 1);
    }

    #[test]
    fn recon_failure_renders_failures_block_with_panic() {
        let (lines, _) = drive_recon(&[
            r#"{"type":"suite","event":"started","test_count":1}"#,
            r#"{"type":"test","event":"started","name":"a::boom"}"#,
            r#"{"type":"test","name":"a::boom","event":"failed","stdout":"thread 'a::boom' panicked at src/lib.rs:10:5:\nassertion failed\n"}"#,
            r#"{"type":"suite","event":"failed","passed":0,"failed":1,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.02}"#,
        ]);
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let parsed = crate::cargo_filter::parse_test_output(&refs);
        assert_eq!(parsed.failed, 1);
        assert_eq!(parsed.failures.len(), 1, "panic detail recovered: {refs:?}");
        assert_eq!(parsed.failures[0].name, "a::boom");
        assert!(
            parsed.failures[0]
                .location
                .as_deref()
                .is_some_and(|l| l.contains("src/lib.rs:10:5")),
            "location parsed: {:?}",
            parsed.failures[0].location
        );
    }

    #[test]
    fn recon_leaves_started_test_in_flight_for_watchdog() {
        // A test that started but never emitted a terminating event is what the
        // per-test watchdog must be able to age and blame.
        let (_, in_flight) = drive_recon(&[
            r#"{"type":"suite","event":"started","test_count":2}"#,
            r#"{"type":"test","event":"started","name":"a::done"}"#,
            r#"{"type":"test","event":"started","name":"a::hangs"}"#,
            r#"{"type":"test","name":"a::done","event":"ok"}"#,
        ]);
        assert_eq!(in_flight, vec!["a::hangs".to_owned()]);
    }

    #[test]
    fn recon_passes_cargo_json_and_stray_output_through() {
        // cargo's own message-format=json line (has `reason`, no `type`) and a
        // non-JSON stray print must survive verbatim - the former for the
        // `--json` path, the latter for `--nocapture` test output.
        let (lines, _) = drive_recon(&[
            r#"{"reason":"compiler-artifact","target":{"name":"brokkr"}}"#,
            "hello from a test under --nocapture",
        ]);
        assert_eq!(
            lines,
            vec![
                r#"{"reason":"compiler-artifact","target":{"name":"brokkr"}}"#.to_owned(),
                "hello from a test under --nocapture".to_owned(),
            ]
        );
    }

    /// A fresh scratch dir for one test. `name` must be unique within this
    /// module - see `crate::test_scratch`.
    fn test_root(name: &str) -> PathBuf {
        crate::test_scratch::scratch("test-runner", name)
    }

    fn wait_for_direct_children(parent: u32) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let pids = direct_child_pids(parent);
            if !pids.is_empty() || Instant::now() >= deadline {
                return pids;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_process_group_exit(pgid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !process_group_exists(pgid) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        !process_group_exists(pgid)
    }

    fn process_group_exists(pgid: u32) -> bool {
        let Ok(pgid) = i32::try_from(pgid) else {
            return false;
        };
        let ret = unsafe { libc::kill(-pgid, 0) };
        if ret == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
}
