//! Shared streaming runner for cargo libtest invocations.
//!
//! The runner keeps captured stdout/stderr for the cargo parsers and watches
//! libtest's JSON lifecycle records. Eligible serial check sweeps isolate each
//! harness and rustdoc stdout stream through exec shims (`harness_shim`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::DevError;
use crate::output::CapturedOutput;
use crate::ratatoskr::process::snapshot_proc;

#[path = "test_runner/harness_shim.rs"]
mod harness_shim;

/// Run as the harness shim when this process was launched as one; `None`
/// otherwise. Checked first thing in `main`, before any CLI parsing.
pub(crate) fn maybe_run_harness_shim() -> Option<i32> {
    harness_shim::maybe_run()
}

// THE OBSERVATION CONTRACT. The runner reports what it saw, typed, as it sees
// it - one `Observation` per libtest record, stream end and kill - to a sink
// the caller supplies. It never interprets them: the caller resolves a stream
// against whatever plan it holds (`check_cmd::accounting`), so nothing here
// knows a sweep, a policy or an expected test. Delivered live rather than
// returned, because the interesting runs are the ones that never return
// normally - an interrupt, a watchdog kill - and a report assembled at the end
// is exactly what those paths used to drop.

/// Where an observed stream came from, as far as the runner knows.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum StreamSource {
    /// The launched process's own stdout: a directly executed test binary's,
    /// or cargo's when no harness is isolated.
    Process,
    /// A harness the shim accepted, named by the executable cargo asked the
    /// runner to run - the shim's handshake carries it before the exec.
    Harness { executable: String },
    /// The rustdoc entry point the shim accepted.
    Rustdoc,
}

/// How a stream stopped. Three different facts: the writer closed it, a read
/// failed, or brokkr stopped reading (the drain grace ran out with something
/// still holding the pipe) - in which case records may have been lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StreamEnd {
    Eof,
    ReadError,
    Cancelled,
}

/// A test's terminal record. libtest states the first three; the engine lane
/// also reports a timed-out test and one its own cancellation killed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TestResult {
    Ok,
    Failed,
    Ignored,
    TimedOut,
    Interrupted,
}

/// Why the runner killed (or stopped waiting for) a process. Structured, so a
/// consumer never has to read a cause back out of a signal status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KillCause {
    /// A watched test crossed the per-test cap; `test` names it.
    PerTest,
    /// Execution under way and nothing completed within the cap.
    NoCompletion,
    /// Nothing in flight for the idle window.
    Idle,
    /// The run's own wall backstop.
    Wall,
    /// A sibling run's failure cancelled this one (the parallel lane's abort).
    Cancelled,
    /// A cooperative shutdown: an interrupt or the phase watchdog.
    Stopped,
}

/// One thing the runner saw on one stream.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum ObsEvent {
    SuiteStarted { test_count: u64 },
    SuiteFinished { passed: u64, failed: u64, ignored: u64 },
    Started { name: String },
    Finished { name: String, result: TestResult },
    /// libtest's `test/timeout`: a test past libtest's own 60s warning. Not a
    /// terminal record - the test is still running.
    SlowWarning { name: String },
    StreamEnded { end: StreamEnd },
    /// A directly launched process's exit status.
    Exited { code: Option<i32>, signal: Option<i32> },
    Killed { cause: KillCause, test: Option<String> },
    /// The shim could not authenticate or attribute a harness.
    AttributionError { detail: String },
}

#[derive(Clone, Debug)]
pub(crate) struct Observation {
    /// Unique within this brokkr process; one per stream the runner reads.
    pub(crate) stream: u64,
    pub(crate) source: StreamSource,
    pub(crate) event: ObsEvent,
}

pub(crate) type ObservationSink = Arc<dyn Fn(Observation) + Send + Sync>;

static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

/// A fresh stream id, for a caller that reports observations of its own (the
/// engine lane, whose events come from the engine rather than a pipe).
pub(crate) fn next_stream_id() -> u64 {
    NEXT_STREAM.fetch_add(1, Ordering::Relaxed)
}

/// One stream's handle on the sink.
#[derive(Clone)]
pub(crate) struct StreamTap {
    sink: ObservationSink,
    stream: u64,
    source: StreamSource,
}

impl StreamTap {
    pub(crate) fn new(sink: &ObservationSink, source: StreamSource) -> Self {
        Self { sink: Arc::clone(sink), stream: next_stream_id(), source }
    }

    pub(crate) fn emit(&self, event: ObsEvent) {
        (self.sink)(Observation { stream: self.stream, source: self.source.clone(), event });
    }
}

/// xxh3-64 of a file's contents, hex, streamed: the identity a plan records
/// for an executable, and checks again before it runs.
pub(crate) fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0_u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:016x}", hasher.digest()))
}

/// What a strict session admits.
#[derive(Clone, Default, Debug)]
pub(crate) struct StrictPlan {
    /// Executable path -> content hash: the harnesses this run may attribute.
    /// A harness is admitted only when its path is here AND the file at that
    /// path still hashes to the planned value at its handshake - the last
    /// moment before it runs.
    pub(crate) known: HashMap<String, String>,
    /// Whether this run is a planned doctest carrier: only then may a rustdoc
    /// stream connect.
    pub(crate) rustdoc: bool,
}

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

/// A watchdog verdict as a structured kill cause, with the test it names when,
/// and only when, it names one. A no-completion verdict names nothing: the
/// budget was blown, but by a test nobody saw.
pub(crate) fn kill_of(reason: &TimeoutReason) -> (KillCause, Option<String>) {
    match reason {
        TimeoutReason::PerTest { name } if name == NO_COMPLETION => (KillCause::NoCompletion, None),
        TimeoutReason::PerTest { name } => (KillCause::PerTest, Some(name.clone())),
        TimeoutReason::SweepWall { .. } => (KillCause::Wall, None),
        TimeoutReason::Idle => (KillCause::Idle, None),
    }
}

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
/// resetting. `brokkr check` and `brokkr test` both arm the 15-minute `test`
/// phase ceiling (`check_cmd/watchdog.rs`), which always fires first, so this
/// is the bound only for a caller that arms no phase watchdog.
pub(crate) const SWEEP_WALL_TIMEOUT: Duration = Duration::from_secs(1800);

/// What [`streaming_run_libtest`] spawns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Launch<'a> {
    /// `cargo` with the caller's args.
    Cargo,
    /// A prebuilt test binary, executed directly with the caller's args.
    Direct { program: &'a str },
}

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
    /// Whether a suite is open - started and not yet summarised. A suite-started
    /// record arriving while one is already open is not a boundary, and must not
    /// reset the once-per-name sets: see [`TestTracker::observe_suite_start`].
    in_suite: bool,
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
            in_suite: false,
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

    /// A test finished. `ran` is false for an `ignored` result: libtest emits
    /// `started` for an `#[ignore]`d test too, immediately followed by
    /// `ignored`, so the start alone does not mean the test body executed.
    /// Such a test leaves the in-flight set (it is no longer running) but is
    /// kept out of `completed`, which callers sum as the pass count and feed to
    /// timing history - counting it there reported ignored tests as passed and
    /// made an all-`#[ignore]` binary look like it had run something.
    fn observe_result(&mut self, name: &str, ran: bool) {
        let first_completion = self.finished.insert(name.to_owned());
        if let Some(started) = self.current.remove(name)
            && ran
        {
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
        self.idle_since = Instant::now();
        // Honoured only when no suite is open.
        //
        // Resetting on every suite-started record was an unbounded forgery
        // channel: a hung test could print `suite/started` then `test/started` on
        // a loop, and each cycle cleared the once-per-name guard and refreshed the
        // clock, so the per-test ceiling never fired. A real stream opens a suite,
        // runs it, closes it with `suite/ok` or `suite/failed`, and only then
        // opens the next; a second start with the previous suite still open is
        // not a suite boundary.
        if self.in_suite {
            return;
        }
        self.in_suite = true;
        self.last_progress = Instant::now();
        // Names are unique within a suite, not within an invocation: cargo runs
        // every test binary of a selection in ONE invocation, and the same path
        // legitimately exists in two of them (`tests::works` in a lib's unit
        // tests and in an integration target). Keeping the once-per-name sets
        // across the whole stream meant the second binary's occurrence refreshed
        // nothing, so a healthy test could be killed for a name an earlier binary
        // had already spent.
        self.seen.clear();
        self.finished.clear();
    }

    /// A suite reported its summary, so the next suite-started record is a real
    /// boundary. See [`Self::observe_suite_start`].
    fn observe_suite_end(&mut self) {
        self.in_suite = false;
    }

    /// How long until the soonest thing this tracker bounds comes due, so the
    /// watchdog can wake exactly then rather than one poll interval late.
    ///
    /// `None` when nothing is pending: no test in flight and execution not begun,
    /// where only the caller's ceilings apply.
    fn next_deadline(&self, timeout: Duration) -> Option<Duration> {
        let oldest_in_flight = self
            .current
            .values()
            .map(|started| timeout.saturating_sub(started.elapsed()))
            .min();
        let no_progress = self
            .executing
            .then(|| timeout.saturating_sub(self.last_progress.elapsed()));
        let idle = self
            .current
            .is_empty()
            .then(|| IDLE_TIMEOUT.saturating_sub(self.idle_since.elapsed()));
        [oldest_in_flight, no_progress, idle].into_iter().flatten().min()
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
    let primary = obs.sink.as_ref().map(|s| StreamTap::new(s, StreamSource::Process));
    let session = shim
        .then(|| harness_shim::Session::new(Arc::clone(&stdout_buf), obs.sink.clone(), strict.clone()))
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
            format!("CARGO_TARGET_{}_RUNNER", host.to_uppercase().replace('-', "_")),
            format!("{} {}", harness_shim::own_exe().display(), harness_shim::RUNNER_ARG),
        );
        socket_env = session.socket();
        rustdoc_env = session.rustdoc();
        cargo_env.push((&runner_env.0, &runner_env.1));
        cargo_env.push(("BROKKR_HARNESS_SOCKET", &socket_env));
        cargo_env.push(("RUSTDOC", &rustdoc_env));
    }
    let (mut child, leader) = match launch {
        Launch::Cargo => (spawn_cargo_process_group(args, cwd, &cargo_env)?, Leader::Cargo),
        Launch::Direct { program } => {
            (spawn_process_group(program, args, cwd, &cargo_env)?, Leader::TestBinary)
        }
    };
    let cargo_pid = child.id();
    if let Some(session) = &session {
        session.set_cargo_pid(cargo_pid);
    }
    let acceptor = session.as_ref().map(harness_shim::Session::run_acceptor).transpose()?;
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
        session.watchdog(state_root_t, cargo_pid, tracker_t, hung_t, &ceilings, start, primary.clone())
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
            tap.emit(ObsEvent::Killed { cause: KillCause::Stopped, test: None });
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
/// in-flight; it also honours a cooperative `brokkr kill` / Ctrl-C. `sink`
/// receives the stream's observations, the kill that ended it, and the exit.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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

    // The per-test hang watchdog - identical to the serial path, aging the
    // JSON-fed tracker. It names an in-flight test that crosses
    // `per_test_timeout`. Attribution only: this path already has an
    // independent whole-sweep clock in the `try_wait` loop below, which is the
    // authoritative bound, so the watchdog carries no `wall` of its own.
    let state_root_t = state_root.to_path_buf();
    let tracker_w = Arc::clone(&tracker);
    let done_w = Arc::clone(&done);
    let hung_w = Arc::clone(&hung);
    let primary_w = primary.clone();
    let watchdog_thread = thread::spawn(move || {
        watchdog_loop(
            state_root_t,
            cargo_pid,
            Leader::TestBinary,
            tracker_w,
            done_w,
            hung_w,
            Ceilings { per_test: per_test_timeout, wall: None, shape: WallShape::SharedHarness },
            start,
            primary_w,
        );
    });

    let waited = wait_parallel(&mut child, program, start, timeout, abort);
    // The kill is recorded as what it was, before the stream end it caused.
    if let Some(tap) = &primary {
        let cause = match &waited {
            Ok(ParallelWait::Exited { timed_out: true, .. }) => Some(KillCause::Wall),
            Ok(ParallelWait::Exited { cancelled: true, .. }) => Some(KillCause::Cancelled),
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
    let (status, timed_out) = match waited? {
        ParallelWait::Exited { status, timed_out, .. } => (status, timed_out),
        ParallelWait::Interrupted => return Err(DevError::Interrupted),
    };
    if let Some(tap) = &primary {
        use std::os::unix::process::ExitStatusExt;
        tap.emit(ObsEvent::Exited { code: status.code(), signal: status.signal() });
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
        timed_out,
        completed,
    })
}

/// How [`wait_parallel`] ended.
enum ParallelWait {
    /// The leader exited, on its own or killed by the backstop (`timed_out`)
    /// or a lane-wide abort (`cancelled`).
    Exited { status: std::process::ExitStatus, timed_out: bool, cancelled: bool },
    /// A cooperative shutdown (`brokkr kill`, Ctrl-C, the `check` watchdog):
    /// the group was killed and reaped, and the run is not a verdict.
    Interrupted,
}

/// The parallel runner's wait loop: poll the process-group leader until it
/// exits, or kill the group on the whole-sweep backstop, a lane-wide `abort`,
/// or a cooperative shutdown.
fn wait_parallel(
    child: &mut std::process::Child,
    program: &str,
    start: Instant,
    timeout: Duration,
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
                return Ok(ParallelWait::Exited { status, timed_out: false, cancelled: false });
            }
            Ok(None) => {
                let overtime = start.elapsed() >= timeout;
                let cancelled = abort.is_some_and(|a| a.load(Ordering::SeqCst));
                // `check` and `brokkr test` hold a `SigtermGuard` for their
                // whole run, whose handler already killed this group; the poll
                // covers a request raised without a signal (the watchdog).
                let interrupted = crate::shutdown::is_shutdown_requested();
                if overtime || cancelled || interrupted {
                    kill_process_group(cargo_pid).ok();
                    let status = child.wait().map_err(spawn_err)?;
                    if interrupted {
                        return Ok(ParallelWait::Interrupted);
                    }
                    return Ok(ParallelWait::Exited {
                        status,
                        timed_out: overtime,
                        cancelled: cancelled && !overtime,
                    });
                }
                thread::sleep(WATCHDOG_POLL);
            }
            Err(e) => return Err(spawn_err(e)),
        }
    }
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
fn drain_libtest_json<R, F>(
    mut pipe: R,
    buf: &Mutex<Vec<u8>>,
    tracker: &Mutex<TestTracker>,
    cancel: &AtomicBool,
    tap: Option<StreamTap>,
    mut forward_line: F,
) where
    R: Read + std::os::fd::AsRawFd,
    F: FnMut(&str),
{
    let mut recon = JsonReconstructor { tap, ..JsonReconstructor::default() };
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
    let end = loop {
        let n = match read_chunk(&mut pipe, &mut read_buf, cancel) {
            Chunk::Data(n) => n,
            Chunk::Eof => break StreamEnd::Eof,
            Chunk::Error => break StreamEnd::ReadError,
            Chunk::Cancelled => break StreamEnd::Cancelled,
        };
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
    };
    if !line.is_empty() {
        let out = recon.observe(&String::from_utf8_lossy(&line), tracker);
        emit(&out, buf, &mut forward_line);
    }
    recon.report(ObsEvent::StreamEnded { end });
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
    /// Where each recognised record is reported as a typed observation, when
    /// the caller asked for them. Only records parsed off the stream are
    /// reported - text this reconstructor synthesizes (a dead suite's closure)
    /// is display, never evidence.
    tap: Option<StreamTap>,
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
        let count = |key: &str| val.get(key).and_then(Value::as_u64).unwrap_or(0);
        match (kind, event) {
            ("suite", "started") => {
                self.failures.clear();
                // Execution has begun: the per-test cap now bounds the whole run,
                // so a lost start record cannot buy a test the idle ceiling.
                if let Ok(mut t) = tracker.lock() {
                    t.observe_suite_start();
                }
                let test_count = count("test_count");
                self.report(ObsEvent::SuiteStarted { test_count });
                vec![String::new(), format!("running {test_count} tests")]
            }
            ("suite", _) => {
                // A summary closes the suite, so the next suite-started record is
                // a real boundary rather than a replay.
                if let Ok(mut t) = tracker.lock() {
                    t.observe_suite_end();
                }
                self.report(ObsEvent::SuiteFinished {
                    passed: count("passed"),
                    failed: count("failed"),
                    ignored: count("ignored"),
                });
                self.render_suite_summary(val, event)
            }
            ("test", "started") => {
                if let Some(name) = val.get("name").and_then(Value::as_str) {
                    if let Ok(mut t) = tracker.lock() {
                        t.observe_start(name.to_owned());
                    }
                    self.report(ObsEvent::Started { name: name.to_owned() });
                }
                Vec::new()
            }
            ("test", "ok") => {
                let name = self.finish(val, tracker, TestResult::Ok);
                name.map(|n| vec![format!("test {n} ... ok")]).unwrap_or_default()
            }
            ("test", "ignored") => {
                // Not a run: see `TestTracker::observe_result`.
                let name = self.finish(val, tracker, TestResult::Ignored);
                name.map(|n| vec![format!("test {n} ... ignored")])
                    .unwrap_or_default()
            }
            ("test", "failed") => {
                let Some(name) = self.finish(val, tracker, TestResult::Failed) else {
                    return Vec::new();
                };
                let captured = val
                    .get("stdout")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.failures.push((name.clone(), captured));
                vec![format!("test {name} ... FAILED")]
            }
            // libtest's slow-test warning. The test is still running, so it
            // changes nothing in the tracker - but it is reported rather than
            // dropped: under brokkr's 20s cap a test can only get there by a
            // budget that was not enforced, which accounting must see.
            ("test", "timeout") => {
                if let Some(name) = val.get("name").and_then(Value::as_str) {
                    self.report(ObsEvent::SlowWarning { name: name.to_owned() });
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn report(&self, event: ObsEvent) {
        if let Some(tap) = &self.tap {
            tap.emit(event);
        }
    }

    /// Clear a completed test from the tracker, report its terminal record and
    /// return its name. An ignored result does not count as completed.
    fn finish(&self, val: &Value, tracker: &Mutex<TestTracker>, result: TestResult) -> Option<String> {
        let name = val.get("name").and_then(Value::as_str)?.to_owned();
        if let Ok(mut t) = tracker.lock() {
            t.observe_result(&name, result != TestResult::Ignored);
        }
        self.report(ObsEvent::Finished { name: name.clone(), result });
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

/// One listing run of a prebuilt test binary: its exit status, its output, and
/// the two ways it can have failed to finish.
pub(crate) struct ListingRun {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// Killed at the wall deadline. Whatever it printed is not a listing.
    pub(crate) timed_out: bool,
    /// The leader exited but its output did not close within [`DRAIN_GRACE`]
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
/// whole process, and once it exits its output gets [`DRAIN_GRACE`] to close,
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

    let spawn_err = |error: std::io::Error| DevError::Spawn { program: program.into(), error };
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
    Ok(ListingRun { status, stdout: take(&out_buf), stderr: take(&err_buf), timed_out, unsettled })
}

/// Read `pipe` into a shared buffer on a new thread until EOF or `cancel`.
fn drain_to_buffer<R>(
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

    // A shutdown already requested means the run is unwinding: a queued
    // parallel binary, or the next sweep, must not start a group the handler
    // will never see registered in time.
    if crate::shutdown::is_shutdown_requested() {
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

// The partial-marker state machine lived here, together with `drain_stdout`,
// `handle_stdout_line`, `is_libtest_result_summary`, `parse_start_marker` and
// `parse_result_marker`. All deleted: both lanes read libtest's JSON event stream
// now, so nothing infers which test is running from a `test NAME ... ` marker
// that the test's own output glues itself onto.
//
// Worth recording what it cost, because the replacement is not a matter of taste.
// libtest writes that marker without a newline, flushes, lets the test print,
// then writes a bare `ok`/`FAILED`/`ignored`. So `print!("hi")` arrived as
// `hiok` - not a bare status - the pending test was never cleared, the NEXT
// test's marker was then ignored, and the watchdog blamed the wrong test.
// `println!("ok")` produced the mirror image: a real bare-status line before
// libtest's own, clearing pending early and hiding a hang in the same test. The
// machine's own documentation admitted it fixed the first and merely NARROWED
// the second. Events state plainly what all of that had to guess.

fn drain_stderr<F, G>(
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
        let mut pfd = libc::pollfd { fd: pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 };
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
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Join a run's output drains after its child exited. They normally reach EOF
/// at once; a process that inherited the child's stdout or stderr (a daemon a
/// test leaked) keeps them open forever, and an unbounded join then hangs
/// brokkr with the lock held. So after [`DRAIN_GRACE`] the drains are told to
/// stop and joined - always joined, so no thread outlives the run to append to
/// its buffers or print into the next one. No signal is sent: the run's
/// process group may be gone and its id reused. A leaked process that keeps
/// writing gets `EPIPE`; one carrying the run token is reaped at the next hold
/// (`crate::test_orphans`). Returns whether capture was cut short.
fn join_drains(drains: Vec<thread::JoinHandle<()>>, cancel: &AtomicBool) -> bool {
    let until = Instant::now() + DRAIN_GRACE;
    while Instant::now() < until && !drains.iter().all(thread::JoinHandle::is_finished) {
        thread::sleep(Duration::from_millis(10));
    }
    let cut = !drains.iter().all(thread::JoinHandle::is_finished);
    if cut {
        cancel.store(true, Ordering::Release);
        crate::output::warn(&format!(
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

#[allow(clippy::needless_pass_by_value)] // The Arcs are moved into a spawned thread.
#[allow(clippy::too_many_arguments)]
fn watchdog_loop(
    state_root: PathBuf,
    leader_pid: u32,
    leader: Leader,
    tracker: Arc<Mutex<TestTracker>>,
    done: Arc<AtomicBool>,
    hung: Arc<Mutex<Option<HungTest>>>,
    ceilings: Ceilings,
    started: Instant,
    tap: Option<StreamTap>,
) {
    watchdog_loop_with_timing(
        state_root,
        leader_pid,
        leader,
        tracker,
        done,
        hung,
        ceilings,
        WATCHDOG_POLL,
        started,
        tap.as_ref(),
    );
}

#[allow(clippy::needless_pass_by_value)] // The Arcs are moved into a spawned thread.
#[allow(clippy::too_many_arguments)]
fn watchdog_loop_with_timing(
    state_root: PathBuf,
    cargo_pid: u32,
    leader: Leader,
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
    // Where the verdict is reported, before the kill it causes.
    tap: Option<&StreamTap>,
) {
    let timeout = ceilings.per_test;
    // Emitted at most once, so a long sweep does not repeat the same guess every
    // poll tick.
    let mut warned = false;
    loop {
        if done.load(Ordering::SeqCst) {
            return;
        }
        // Sleep to the nearest known deadline rather than a flat interval, so the
        // wake-up lands ON the ceiling instead of up to one poll past it. A flat
        // 250ms poll meant every cap was really "the ceiling plus up to 250ms",
        // which is not what a hard cap means. Capped by `poll` so the loop still
        // notices `done` and the abort flag promptly.
        let until_deadline = tracker
            .lock()
            .ok()
            .and_then(|t| t.next_deadline(timeout))
            .into_iter()
            .chain(
                ceilings
                    .wall
                    .map(|w| w.saturating_sub(started.elapsed())),
            )
            .min();
        thread::sleep(until_deadline.map_or(poll, |d| d.min(poll)));
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

        // FREEZE first, diagnose second, kill third.
        //
        // Snapshotting before stopping anything let the offending test keep
        // running for as long as `/proc` collection took, so the cap was not
        // literal: a test was permitted past its ceiling by the snapshot's
        // duration on top of the poll interval. Killing first would fix the cap
        // and destroy the diagnostic - a dead process has no `wchan` and no
        // stack, which is exactly what a hang investigation needs.
        //
        // SIGSTOP resolves it. The group stops executing immediately, so no
        // further test time is consumed, while `/proc` stays readable for the
        // snapshot. SIGKILL then lands on already-stopped processes.
        stop_process_group(cargo_pid).ok();
        if let Some(tap) = tap {
            let (cause, test) = kill_of(&reason);
            tap.emit(ObsEvent::Killed { cause, test });
        }
        let hung_test = capture_hung_test(&state_root, cargo_pid, leader, reason, elapsed, ceiling);
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
    leader: Leader,
    reason: TimeoutReason,
    elapsed: Duration,
    ceiling: Duration,
) -> HungTest {
    let test = reason.label();
    let test = test.as_str();
    let test_pids = direct_child_pids(cargo_pid);
    let snapshot_pid = match leader {
        Leader::Cargo => test_pids.first().copied().or(Some(cargo_pid)),
        Leader::TestBinary => Some(cargo_pid),
    };
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

/// SIGSTOP a process group: stop it executing without destroying it.
///
/// Used to freeze a run the instant its budget expires, so the diagnostic
/// snapshot that follows costs the offending test no further wall time and still
/// has a live `/proc` to read. SIGKILL follows.
fn stop_process_group(pgid: u32) -> Result<(), DevError> {
    signal_process_group(pgid, libc::SIGSTOP)
}

fn kill_process_group(pgid: u32) -> Result<(), DevError> {
    signal_process_group(pgid, libc::SIGKILL)
}

fn signal_process_group(pgid: u32, signal: libc::c_int) -> Result<(), DevError> {
    let pgid = i32::try_from(pgid)
        .map_err(|_| DevError::Build(format!("process group id {pgid} does not fit pid_t")))?;
    let target: libc::pid_t = -pgid;
    let ret = unsafe { libc::kill(target, signal) };
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

    /// A child that exits while a background process it started still holds
    /// its stdout: the drain is cut after the grace period instead of waiting
    /// for that process, and what arrived before the exit is kept.
    #[test]
    fn a_leaked_pipe_holder_does_not_hang_the_drain_join() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 15 & echo $!; echo captured")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let pipe = child.stdout.take().expect("piped stdout");
        let buf = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(Mutex::new(TestTracker::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (b, t, c) = (Arc::clone(&buf), Arc::clone(&tracker), Arc::clone(&cancel));
        let drain = thread::spawn(move || drain_libtest_json(pipe, &b, &t, &c, None, |_| {}));
        child.wait().expect("sh exits");
        let started = Instant::now();
        assert!(join_drains(vec![drain], &cancel), "the leaked holder kept the pipe open");
        assert!(started.elapsed() < DRAIN_GRACE + Duration::from_secs(2));
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(text.contains("captured"), "{text}");
        if let Some(pid) = text.lines().next().and_then(|l| l.trim().parse::<i32>().ok()) {
            // SAFETY: the sleep this test started; ESRCH is fine.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    fn collecting_sink() -> (ObservationSink, Arc<Mutex<Vec<Observation>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_t = Arc::clone(&seen);
        let sink: ObservationSink = Arc::new(move |o| seen_t.lock().unwrap().push(o));
        (sink, seen)
    }

    fn drained_end(script: &str) -> (Option<StreamEnd>, String) {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let pipe = child.stdout.take().expect("piped stdout");
        let (sink, seen) = collecting_sink();
        let tap = StreamTap::new(&sink, StreamSource::Process);
        let buf = Arc::new(Mutex::new(Vec::new()));
        let tracker = Arc::new(Mutex::new(TestTracker::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (b, t, c) = (Arc::clone(&buf), Arc::clone(&tracker), Arc::clone(&cancel));
        let drain = thread::spawn(move || drain_libtest_json(pipe, &b, &t, &c, Some(tap), |_| {}));
        child.wait().expect("sh exits");
        join_drains(vec![drain], &cancel);
        let end = seen.lock().unwrap().iter().find_map(|o| match o.event {
            ObsEvent::StreamEnded { end } => Some(end),
            _ => None,
        });
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (end, text)
    }

    /// The three ends of a stream are three facts. A drain the grace period
    /// cancelled - a leaked process still holding the pipe - says so, because
    /// whatever that stream had not delivered yet is lost; a writer that
    /// closed says EOF.
    #[test]
    fn a_cancelled_drain_is_told_apart_from_an_eof() {
        let (end, text) = drained_end("sleep 15 & echo $!; echo captured");
        assert_eq!(end, Some(StreamEnd::Cancelled), "{text}");
        if let Some(pid) = text.lines().next().and_then(|l| l.trim().parse::<i32>().ok()) {
            // SAFETY: the sleep this test started; ESRCH is fine.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let (end, _) = drained_end("echo done");
        assert_eq!(end, Some(StreamEnd::Eof));
    }

    /// Every recognised record reaches the sink typed, `test/timeout`
    /// included, and nothing the reconstructor renders on its own does.
    #[test]
    fn recognised_records_reach_the_sink_typed() {
        let (sink, seen) = collecting_sink();
        let tracker = Mutex::new(TestTracker::default());
        let mut recon = JsonReconstructor {
            tap: Some(StreamTap::new(&sink, StreamSource::Process)),
            ..JsonReconstructor::default()
        };
        for ev in [
            r#"{"type":"suite","event":"started","test_count":2}"#,
            r#"{"type":"test","event":"started","name":"a::one"}"#,
            r#"{"type":"test","event":"timeout","name":"a::one"}"#,
            r#"{"type":"test","name":"a::one","event":"ok"}"#,
            r#"{"type":"test","event":"started","name":"a::two"}"#,
            r#"{"type":"test","name":"a::two","event":"failed","stdout":"boom"}"#,
            r#"{"type":"suite","event":"failed","passed":1,"failed":1,"ignored":0,"measured":0,"filtered_out":0,"exec_time":0.0}"#,
        ] {
            recon.observe(ev, &tracker);
        }
        let events: Vec<ObsEvent> = seen.lock().unwrap().iter().map(|o| o.event.clone()).collect();
        assert_eq!(
            events,
            vec![
                ObsEvent::SuiteStarted { test_count: 2 },
                ObsEvent::Started { name: "a::one".into() },
                ObsEvent::SlowWarning { name: "a::one".into() },
                ObsEvent::Finished { name: "a::one".into(), result: TestResult::Ok },
                ObsEvent::Started { name: "a::two".into() },
                ObsEvent::Finished { name: "a::two".into(), result: TestResult::Failed },
                ObsEvent::SuiteFinished { passed: 1, failed: 1, ignored: 0 },
            ]
        );
    }

    #[test]
    fn a_forged_suite_start_cannot_reset_the_anti_forgery_guard() {
        let mut tracker = TestTracker::default();
        tracker.observe_suite_start();
        tracker.observe_start("a::hung".to_owned());

        // The loop a hung test could otherwise run: re-announce a suite, then
        // re-announce itself, forever. With the previous suite still open, the
        // suite record is not a boundary, so neither the once-per-name guard nor
        // the clock is reset.
        for _ in 0..5 {
            tracker.last_progress = Instant::now()
                .checked_sub(TEST_TIMEOUT + Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
            tracker.observe_suite_start();
            tracker.observe_start("a::hung".to_owned());
            assert!(
                tracker.timed_out(TEST_TIMEOUT).is_some(),
                "a replayed suite start must not buy the test more time"
            );
        }

        // A real boundary - the suite summarised first - still resets.
        tracker.observe_suite_end();
        tracker.observe_suite_start();
        tracker.observe_start("a::hung".to_owned());
        assert!(
            tracker.timed_out(TEST_TIMEOUT).is_none(),
            "a suite that closed and reopened is a genuine boundary"
        );
    }

    #[test]
    fn the_same_name_in_a_second_binary_still_counts_as_progress() {
        let mut tracker = TestTracker::default();
        tracker.observe_suite_start();
        tracker.observe_start("tests::works".to_owned());
        tracker.observe_result("tests::works", true);

        // Second binary of the same invocation, same test path, clock wound past
        // the cap in between. The first suite summarises first, as a real stream
        // does - that summary is what makes the next start a genuine boundary.
        tracker.observe_suite_end();
        tracker.observe_suite_start();
        tracker.last_progress = Instant::now()
            .checked_sub(TEST_TIMEOUT + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        tracker.observe_start("tests::works".to_owned());
        assert!(
            tracker.timed_out(TEST_TIMEOUT).is_none(),
            "a fresh suite's occurrence of the same name is real progress"
        );
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
        tracker.observe_result("a::fast", true);
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
            Leader::Cargo,
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
            None,
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

    /// A summary alone must not disarm the no-progress clock: a live test can
    /// print `suite/ok`. Only a process boundary (an isolated harness's exit)
    /// retires a tracker.
    #[test]
    fn suite_summary_alone_cannot_disarm_a_live_process_clock() {
        let mut tracker = TestTracker::default();
        tracker.observe_suite_start();
        tracker.observe_suite_end();
        tracker.last_progress = Instant::now() - TEST_TIMEOUT;
        let (name, _) = tracker.timed_out(TEST_TIMEOUT).expect("live process remains bounded");
        assert_eq!(name, NO_COMPLETION);
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
        tracker.observe_result("a::one", true);

        // Wind the clock back past the cap, then replay the same records - which
        // is exactly what a test looping `println!` of a captured event does.
        tracker.last_progress = Instant::now()
            .checked_sub(TEST_TIMEOUT + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        tracker.observe_start("a::one".to_owned());
        tracker.observe_result("a::one", true);

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
            in_suite: false,
            idle_since: Instant::now(),
        }));
        let done = Arc::new(AtomicBool::new(false));
        let hung = Arc::new(Mutex::new(None::<HungTest>));

        // The wall is an hour away and must not be what saves or condemns this
        // run: the per-test cap alone has to fire.
        watchdog_loop_with_timing(
            root.clone(),
            cargo_pid,
            Leader::Cargo,
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
            None,
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
            in_suite: false,
            idle_since: started,
        }));
        let done = Arc::new(AtomicBool::new(false));
        let hung = Arc::new(Mutex::new(None::<HungTest>));

        watchdog_loop_with_timing(
            root.clone(),
            cargo_pid,
            Leader::Cargo,
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
            None,
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

    // A directly executed test binary is the hung harness itself; its children
    // are whatever the tests spawned. The snapshot must examine the leader,
    // where under cargo it examines cargo's first child.
    #[test]
    fn a_direct_launch_snapshots_the_leader_not_its_child() {
        use std::os::unix::process::CommandExt;

        let root = test_root("direct_leader_snapshot");
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
        let leader = child.id();
        let children = wait_for_direct_children(leader);
        assert!(!children.is_empty(), "shell did not spawn sleep child");

        let reason = TimeoutReason::PerTest { name: "direct::hangs".to_owned() };
        let tick = Duration::from_millis(1);
        let direct = capture_hung_test(&root, leader, Leader::TestBinary, reason.clone(), tick, tick);
        let cargo = capture_hung_test(&root, leader, Leader::Cargo, reason, tick, tick);
        kill_process_group(leader).ok();
        child.wait().ok();

        assert_eq!(direct.snapshot_pid, Some(leader));
        assert_eq!(cargo.snapshot_pid, children.first().copied());
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

    /// libtest emits `started` for an `#[ignore]`d test, then `ignored`. The
    /// test leaves the in-flight set but must not land in `completed`, which
    /// the parallel lane sums as its pass count: counting it reported ignored
    /// tests as passed and hid an all-ignored binary from the empty-unit note.
    #[test]
    fn an_ignored_test_is_not_counted_as_completed() {
        let tracker = Mutex::new(TestTracker::default());
        let mut recon = JsonReconstructor::default();
        for ev in [
            r#"{"type":"suite","event":"started","test_count":2}"#,
            r#"{"type":"test","event":"started","name":"a::skipped"}"#,
            r#"{"type":"test","name":"a::skipped","event":"ignored"}"#,
            r#"{"type":"test","event":"started","name":"a::ran"}"#,
            r#"{"type":"test","name":"a::ran","event":"ok"}"#,
        ] {
            recon.observe(ev, &tracker);
        }
        let t = tracker.lock().unwrap();
        assert!(t.current.is_empty(), "an ignored test is not in flight");
        let names: Vec<&str> = t.completed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a::ran"]);
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
