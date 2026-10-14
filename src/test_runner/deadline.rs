//! The deadline model shared by direct, cargo, parallel, and shim lanes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::{KillCause, Leader, ObsEvent, RunContext, StreamTap, capture_hung_test};

/// Why the shared deadline machinery declared a run over budget. This carries
/// the clock that fired; test naming and confidence live in `Attribution`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TimeoutReason {
    Watched,
    NoCompletion,
    Idle,
    /// The shared run wall expired; in-flight names are unverified context.
    Wall {
        blamed: Vec<String>,
    },
    /// The caller resolved one test and its authoritative wall expired.
    OneTestWall,
}

/// How confidently a deadline verdict can name a test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Attribution {
    /// A caller resolved the only test this process could execute.
    Exact { name: String },
    /// A stream reported this name, but its records can be missing or forged.
    Suspect { name: String, stale: bool },
    /// No test can be named from the available evidence.
    None,
}

impl Attribution {
    pub(crate) fn test_name(&self) -> Option<&str> {
        match self {
            Self::Exact { name } | Self::Suspect { name, .. } => Some(name),
            Self::None => None,
        }
    }
}

/// What the tracker says expired. The timer identity never travels through a
/// test-name string, so a test may use any name without colliding with a clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Expiry {
    Watched { test: String, elapsed: Duration },
    NoCompletion { elapsed: Duration },
    Idle { elapsed: Duration },
}

pub(crate) fn kill_of(reason: &TimeoutReason) -> KillCause {
    match reason {
        TimeoutReason::Watched | TimeoutReason::OneTestWall => KillCause::PerTest,
        TimeoutReason::NoCompletion => KillCause::NoCompletion,
        TimeoutReason::Idle => KillCause::Idle,
        TimeoutReason::Wall { .. } => KillCause::Wall,
    }
}

/// **The hard cap.** Every test brokkr runs gets this much wall time and no more;
/// exceeding it fails the run and stops it. The single exception is
/// `brokkr test --timeout`, which raises the cap for one resolved test and is
/// itself limited to 280s by the CLI parser.
///
/// The clock is fed by libtest's announcements, which share a stream with test
/// output, so a lost event can blur *which* test is named. It cannot excuse the
/// overrun: if no test has been seen completing for longer than the budget, the
/// budget is blown whoever spent it.
pub(crate) const TEST_TIMEOUT: Duration = Duration::from_secs(20);
pub(super) const WATCHDOG_POLL: Duration = Duration::from_millis(250);

/// How long a libtest run may go with no test in flight before the watchdog
/// kills it as a wedge. The per-test ceiling ages a test only after libtest
/// announces it, so it is blind to everything around the tests: the compile
/// and link inside `cargo test`, and the binary's teardown after the last
/// result. Five minutes covers cold builds while still bounding a wedge.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Wall-clock backstop for a whole run, enforced from the watchdog's own clock
/// and unreachable by test output.
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
        Self {
            per_test,
            wall: Some(SWEEP_WALL_TIMEOUT),
            shape: WallShape::SharedHarness,
        }
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

// Tracker readings are collected under their own locks; the decision below is
// shared by direct, shared-stream and shim watchdogs. Idle is supplied by the
// caller because a shim session owns the gap between successive harnesses.
pub(super) struct DeadlineReading {
    pub(super) expiry: Option<Expiry>,
    pub(super) stale: bool,
    pub(super) in_flight: Vec<String>,
}

impl DeadlineReading {
    pub(super) fn from_tracker(t: &TestTracker, timeout: Duration) -> Self {
        Self {
            expiry: t.timed_out(timeout),
            stale: !t.ever_observed,
            in_flight: t.current.keys().cloned().collect(),
        }
    }
}

pub(super) struct DeadlineVerdict {
    pub(super) reason: TimeoutReason,
    pub(super) attribution: Attribution,
    pub(super) elapsed: Duration,
    pub(super) ceiling: Duration,
    /// Index of the tracker whose stream owns a per-test expiry.
    pub(super) charged: Option<usize>,
}

impl Ceilings {
    pub(super) fn verdict(
        &self,
        readings: &[DeadlineReading],
        idle: Option<Duration>,
        elapsed: Duration,
    ) -> Option<DeadlineVerdict> {
        for (index, reading) in readings.iter().enumerate() {
            let Some(expiry) = reading.expiry.as_ref() else {
                continue;
            };
            match expiry {
                Expiry::Watched { test, elapsed } => {
                    let attribution = match &self.shape {
                        WallShape::OneTest { name } => Attribution::Exact { name: name.clone() },
                        WallShape::SharedHarness => Attribution::Suspect {
                            name: test.clone(),
                            stale: reading.stale,
                        },
                    };
                    return Some(DeadlineVerdict {
                        reason: TimeoutReason::Watched,
                        attribution,
                        elapsed: *elapsed,
                        ceiling: self.per_test,
                        charged: Some(index),
                    });
                }
                Expiry::NoCompletion { elapsed } => {
                    let attribution = match &self.shape {
                        WallShape::OneTest { name } => Attribution::Exact { name: name.clone() },
                        WallShape::SharedHarness => Attribution::None,
                    };
                    return Some(DeadlineVerdict {
                        reason: TimeoutReason::NoCompletion,
                        attribution,
                        elapsed: *elapsed,
                        ceiling: self.per_test,
                        charged: Some(index),
                    });
                }
                // The run-level idle clock below owns this expiry, because an
                // isolated session must measure the gaps between harnesses too.
                Expiry::Idle { .. } => {}
            }
        }
        if let Some(idle) = idle.filter(|idle| *idle >= IDLE_TIMEOUT) {
            return Some(DeadlineVerdict {
                reason: TimeoutReason::Idle,
                attribution: Attribution::None,
                elapsed: idle,
                ceiling: IDLE_TIMEOUT,
                charged: None,
            });
        }
        let wall = self.wall.filter(|wall| elapsed >= *wall)?;
        let (reason, attribution) = match &self.shape {
            WallShape::OneTest { name } => (
                TimeoutReason::OneTestWall,
                Attribution::Exact { name: name.clone() },
            ),
            WallShape::SharedHarness => {
                let mut blamed: Vec<_> = readings
                    .iter()
                    .flat_map(|reading| reading.in_flight.iter().cloned())
                    .collect();
                blamed.sort();
                blamed.dedup();
                (TimeoutReason::Wall { blamed }, Attribution::None)
            }
        };
        Some(DeadlineVerdict {
            reason,
            attribution,
            elapsed,
            ceiling: wall,
            charged: None,
        })
    }
}

pub(super) fn enforce_deadline(
    state_root: &Path,
    pid: u32,
    leader: Leader,
    verdict: DeadlineVerdict,
    hung: &Mutex<Option<HungTest>>,
    tap: Option<&StreamTap>,
    context: &RunContext,
) {
    // Freeze before diagnosis: collection must not extend the execution budget.
    crate::process::signal_process_group(pid, libc::SIGSTOP).ok();
    if matches!(
        &verdict.attribution,
        Attribution::Suspect { stale: true, .. }
    ) {
        context.warn(
            "no libtest lifecycle event was ever observed, so the test named below is \
             brokkr's best guess at which one burned the budget - the budget itself \
             was still exceeded.",
        );
    }
    if let Some(tap) = tap {
        tap.emit(ObsEvent::Killed {
            cause: kill_of(&verdict.reason),
            test: verdict.attribution.test_name().map(str::to_owned),
        });
    }
    let report = capture_hung_test(
        state_root,
        pid,
        leader,
        verdict.reason,
        verdict.attribution,
        verdict.elapsed,
        verdict.ceiling,
    );
    if let Ok(mut slot) = hung.lock() {
        *slot = Some(report);
    }
    crate::process::signal_process_group(pid, libc::SIGKILL).ok();
}

#[derive(Clone, Debug)]
pub(crate) struct HungTest {
    pub(crate) reason: TimeoutReason,
    pub(crate) attribution: Attribution,
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

pub(super) struct TestTracker {
    pub(super) current: HashMap<String, Instant>,
    pub(super) completed: Vec<(String, Duration)>,
    /// Whether any lifecycle event has ever been observed.
    ///
    /// While this is false nothing has been parsed, so `idle_since` still holds
    /// the run's start and the idle measurement is plain wall time - trustworthy
    /// because no input has touched it. Once an event arrives, output can reset
    /// the window and the idle clock becomes advisory like the per-test one.
    pub(super) ever_observed: bool,
    /// Whether a suite has announced itself, i.e. test execution has begun.
    /// From that point the per-test cap applies to the whole run, not only to
    /// tests brokkr happens to be watching.
    pub(super) executing: bool,
    /// When brokkr last saw *new* lifecycle progress: a suite announcing itself,
    /// or the first sighting of a test starting or completing. Bounded by the
    /// per-test cap - see [`TestTracker::timed_out`] for why a no-progress clock
    /// is needed on top of the per-test one, and `observe_start` for why only
    /// first sightings count.
    pub(super) last_progress: Instant,
    /// Names whose start has been counted, so a repeated start cannot refresh the
    /// clock. A test can write lifecycle-shaped records to the same stdout the
    /// protocol uses; this bounds how many refreshes such a forgery can buy to the
    /// number of distinct names it invents.
    pub(super) seen: HashSet<String>,
    /// Names whose completion has been counted, for the same reason.
    pub(super) finished: HashSet<String>,
    /// Whether a suite is open - started and not yet summarised. A suite-started
    /// record arriving while one is already open is not a boundary, and must not
    /// reset the once-per-name sets: see [`TestTracker::observe_suite_start`].
    pub(super) in_suite: bool,
    /// When the in-flight set last changed (or the run began): the start of
    /// the current no-test-in-flight window, which [`IDLE_TIMEOUT`] bounds.
    pub(super) idle_since: Instant,
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
    pub(super) fn observe_start(&mut self, name: String) {
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
    pub(super) fn observe_result(&mut self, name: &str, ran: bool) {
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
    pub(super) fn observe_suite_start(&mut self) {
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
    pub(super) fn observe_suite_end(&mut self) {
        self.in_suite = false;
    }

    /// How long until the soonest thing this tracker bounds comes due, so the
    /// watchdog can wake exactly then rather than one poll interval late.
    ///
    /// `None` when nothing is pending: no test in flight and execution not begun,
    /// where only the caller's ceilings apply.
    pub(super) fn next_deadline(&self, timeout: Duration) -> Option<Duration> {
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
        [oldest_in_flight, no_progress, idle]
            .into_iter()
            .flatten()
            .min()
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
    pub(super) fn timed_out(&self, timeout: Duration) -> Option<Expiry> {
        // 1. A test we can see, over budget. Prefer this: it can be named.
        let watched = self
            .current
            .iter()
            .filter_map(|(name, started)| {
                let elapsed = started.elapsed();
                (elapsed >= timeout).then(|| (name.clone(), elapsed))
            })
            .max_by_key(|(_, elapsed)| *elapsed);
        if let Some((test, elapsed)) = watched {
            return Some(Expiry::Watched { test, elapsed });
        }

        // 2. Execution under way and nothing completing.
        if self.executing {
            let elapsed = self.last_progress.elapsed();
            if elapsed >= timeout {
                return Some(Expiry::NoCompletion { elapsed });
            }
        }

        // 3. Nothing running yet: the idle window.
        if self.current.is_empty() {
            let elapsed = self.idle_since.elapsed();
            return (elapsed >= IDLE_TIMEOUT).then_some(Expiry::Idle { elapsed });
        }
        None
    }
}

#[allow(clippy::needless_pass_by_value)] // The Arcs are moved into a spawned thread.
#[allow(clippy::too_many_arguments)]
pub(super) fn watchdog_loop(
    state_root: PathBuf,
    leader_pid: u32,
    leader: Leader,
    tracker: Arc<Mutex<TestTracker>>,
    done: Arc<AtomicBool>,
    hung: Arc<Mutex<Option<HungTest>>>,
    ceilings: Ceilings,
    started: Instant,
    tap: Option<StreamTap>,
    context: RunContext,
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
        &context,
    );
}

#[allow(clippy::needless_pass_by_value)] // The Arcs are moved into a spawned thread.
#[allow(clippy::too_many_arguments)]
pub(super) fn watchdog_loop_with_timing(
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
    context: &RunContext,
) {
    let timeout = ceilings.per_test;
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
            .chain(ceilings.wall.map(|w| w.saturating_sub(started.elapsed())))
            .min();
        thread::sleep(until_deadline.map_or(poll, |d| d.min(poll)));
        if done.load(Ordering::SeqCst) {
            return;
        }

        let (readings, idle) = tracker.lock().ok().map_or_else(
            || (Vec::new(), None),
            |t| {
                (
                    vec![DeadlineReading::from_tracker(&t, timeout)],
                    t.current.is_empty().then(|| t.idle_since.elapsed()),
                )
            },
        );
        let Some(verdict) = ceilings.verdict(&readings, idle, started.elapsed()) else {
            continue;
        };
        if done.load(Ordering::SeqCst) {
            return;
        }
        enforce_deadline(&state_root, cargo_pid, leader, verdict, &hung, tap, context);
        return;
    }
}
