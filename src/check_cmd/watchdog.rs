// The time ceilings on `brokkr check`, `brokkr test` and `brokkr clippy`: one on
// the whole run (check only), one per phase.
//
// Every child `check` spawns already has a per-invocation deadline (the
// 20s hung-test watchdog, the captured runner's deadline), but nothing
// bounded a phase or the run as a whole: a clippy invocation that never
// returns (observed: 1h15m on `clippy threaded/default`), a linker that
// hangs, or a test binary that keeps forking could hold the global lock for
// hours. These are the outer bounds - clocks armed after the lock is taken,
// and a forceful `brokkr kill --hard` equivalent for the run's processes when
// one fires.
//
// Forceful towards the processes, cooperative towards brokkr. The cooperative
// kill (`SigtermGuard` + `Interrupted`) depends on whichever runner is
// currently polling the flag, and a run that has overrun by this much is
// exactly the one whose runner may not be polling - so the watchdog SIGKILLs
// every descendant of brokkr found in `/proc` itself (children of `check` are
// spawned into their own process groups, so a group signal at brokkr's PG
// would miss them, and not all of them are published to the lockfile), each
// identity-checked against its starttime.
//
// It does NOT `exit` from the watchdog thread, which is what it used to do:
// that skipped every destructor on the main thread - `history.db` never
// recorded the run (so exactly the runaway runs were missing from `brokkr
// history`), the run log and status line were never closed, and the lock's
// drop never ran, leaving a `disable_toolchain` pin moved aside. Instead it
// raises the shutdown flag and keeps the subtree dead while the main thread
// unwinds through its normal exit path, which maps the outcome to exit 124.
// Only if the main thread has not unwound after [`UNWIND_GRACE`] (blocked
// somewhere no kill releases) does the watchdog exit the process itself - and
// that backstop restores the toolchain pin and records the history row
// first.

/// How long a `brokkr check` run may hold the lock before the watchdog
/// kills it. Measured from lock acquisition, so a wait behind another brokkr
/// command does not count.
pub(crate) use crate::test_runner::ceilings::CHECK_CEILING;

/// Exit status of a run the watchdog killed. `timeout(1)`'s convention.
pub(crate) const WATCHDOG_EXIT_CODE: i32 = 124;

/// How long the main thread gets to unwind after a ceiling fired before the
/// watchdog exits the process itself. Everything the run started is dead by
/// then (and anything it starts meanwhile is killed within a poll), so a
/// healthy unwind takes well under a second; this only matters for a main
/// thread blocked somewhere no kill releases.
use crate::test_runner::ceilings::UNWIND_GRACE;

// The per-phase ceilings are `Phase::ceiling` (`phase_type.rs`), the one
// source: the source-reading phases finish in seconds, so two minutes is
// already generous; the build phases get what a cold store plausibly needs and
// no more. Clippy's five minutes is set against the observed hang, not against
// its normal cost (about 20s warm on the largest consuming workspace).
// `prepare` gets the test phase's fifteen: it does the test phase's builds -
// every lane's `cargo test --no-run`, cold on a fresh store - plus every
// listing.

/// Why a ceiling fired, once one has. Read by the command's exit path
/// ([`watchdog_fired`]) to turn the unwound outcome into [`WATCHDOG_EXIT_CODE`].
static WATCHDOG_FIRED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static WATCHDOG_FIRED_POISON_REPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn watchdog_fired_lock() -> std::sync::MutexGuard<'static, Option<String>> {
    match WATCHDOG_FIRED.lock() {
        Ok(fired) => fired,
        Err(poisoned) => {
            let fired = poisoned.into_inner();
            WATCHDOG_FIRED.clear_poison();
            if !WATCHDOG_FIRED_POISON_REPORTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                crate::output::warn_stderr(
                    "watchdog result mutex was poisoned; recovered its recorded result",
                );
            }
            fired
        }
    }
}

/// Mark `phase` as the phase in flight: points `failing_phase` at it (the
/// summary's `failed_phase` on an error) and restarts the phase clock. Every
/// phase entry in `run_convention_phases` / `run_build_phases` goes through
/// here, so the two bookkeepings cannot drift.
pub(crate) fn begin_phase(failing_phase: &mut Option<Phase>, phase: Phase) {
    begin_phase_with(failing_phase, phase, enter_phase);
    output::detail(&format!("phase {}: started", phase.as_str()));
    output::status(phase.as_str());
}

/// The two bookkeeping updates with an injected clock entry for unit tests.
fn begin_phase_with(failing_phase: &mut Option<Phase>, phase: Phase, enter: impl FnOnce(Phase)) {
    *failing_phase = Some(phase);
    enter(phase);
}

/// Restart the phase clock for `phase` alone - for the commands that borrow
/// check's ceilings without its summary (`brokkr test`, `brokkr clippy`).
pub(crate) fn enter_phase(phase: Phase) {
    crate::cargo_env::enter_phase(phase);
}

/// The phase in flight, if any: the phase a cargo launch inside it names on
/// its spawn profile, so a `[cargo_env]` failure is attributed to it.
pub(crate) fn current_phase() -> Option<Phase> {
    crate::cargo_env::current_phase()
}

/// How long the phase in flight has been running - the wall time its grouped
/// green line reports. Zero outside a phase.
pub(crate) fn phase_elapsed() -> std::time::Duration {
    crate::cargo_env::phase_clock()
        .map(|(_, since)| since.elapsed())
        .unwrap_or_default()
}

/// How much of the phase in flight's ceiling is left, or `None` outside a
/// phase. What a child with its own deadline inside the phase (a script check)
/// should stay under, so its overrun fails that child rather than firing the
/// watchdog on the whole run.
pub(crate) fn phase_time_left() -> Option<std::time::Duration> {
    phase_time_left_at(crate::cargo_env::phase_clock())
}

/// [`phase_time_left`] over a given clock reading.
fn phase_time_left_at(
    clock: Option<(Phase, std::time::Instant)>,
) -> Option<std::time::Duration> {
    clock.map(|(phase, since)| crate::test_runner::ceilings::phase_ceiling(phase).saturating_sub(since.elapsed()))
}

/// Why the watchdog fired, if it has. A command that armed one checks this
/// on its way out and exits [`WATCHDOG_EXIT_CODE`] when it is set, whatever
/// error the kill surfaced as.
pub(crate) fn watchdog_fired() -> Option<String> {
    watchdog_fired_lock().clone()
}

/// Arms the ceilings; dropping it disarms. Hold it for the whole command.
pub(crate) struct CheckWatchdog {
    disarm: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CheckWatchdog {
    /// `check`'s ceilings: `limit` on the whole run, plus the phase clock.
    pub(crate) fn arm(limit: std::time::Duration) -> Result<Self, crate::error::DevError> {
        Self::arm_for("check", Some(limit))
    }

    /// Ceilings for `command`: an optional whole-run `limit`, plus the phase
    /// clock (which does nothing until a phase is entered). `brokkr test` arms
    /// no whole-run limit - a `-N` repeat run is bounded per iteration, not in
    /// total. Returns an error before command work begins if the watchdog
    /// thread cannot be started; these commands never run without their
    /// configured phase ceilings.
    pub(crate) fn arm_for(
        command: &'static str,
        limit: Option<std::time::Duration>,
    ) -> Result<Self, crate::error::DevError> {
        let disarm = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&disarm);
        let started = std::time::Instant::now();
        let thread = std::thread::Builder::new()
            .name("check-watchdog".into())
            .spawn(move || {
                // Parked rather than slept, so a disarm (which unparks) releases
                // the thread at once and the drop's join costs nothing.
                loop {
                    if flag.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    if let Some(limit) = limit
                        && started.elapsed() >= limit
                    {
                        fire(
                            &format!(
                                "{command} exceeded its {} whole-run ceiling",
                                crate::lockfile::format_duration(limit.as_secs()),
                            ),
                            &flag,
                        );
                        return;
                    }
                    let overrun = crate::cargo_env::phase_clock().and_then(|(phase, since)| {
                        let ceiling = crate::test_runner::ceilings::phase_ceiling(phase);
                        (since.elapsed() >= ceiling).then_some((phase, ceiling))
                    });
                    if let Some((phase, ceiling)) = overrun {
                        fire(
                            &format!(
                                "{command}'s {} phase exceeded its {} ceiling",
                                phase.as_str(),
                                crate::lockfile::format_duration(ceiling.as_secs()),
                            ),
                            &flag,
                        );
                        return;
                    }
                    std::thread::park_timeout(std::time::Duration::from_secs(1));
                }
            })
            .map_err(|error| crate::error::DevError::Spawn {
                program: format!("{command} time-limit watchdog thread"),
                error,
            })?;
        Ok(Self {
            disarm,
            thread: Some(thread),
        })
    }
}

impl Drop for CheckWatchdog {
    fn drop(&mut self) {
        self.disarm.store(true, std::sync::atomic::Ordering::SeqCst);
        crate::cargo_env::clear_phase();
        // Wait the thread out, so a fired watchdog's kill loop has stopped
        // before anything after the command (history's `git`, say) spawns a
        // process it would otherwise SIGKILL.
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            t.join().ok();
        }
    }
}

/// A ceiling was hit: report, SIGKILL every descendant, raise the shutdown
/// flag, and keep the subtree dead while the main thread unwinds. Returns once
/// the command disarms the watchdog (it has unwound); exits the process only
/// if that has not happened within [`UNWIND_GRACE`].
fn fire(why: &str, disarm: &std::sync::atomic::AtomicBool) {
    fire_with(why, disarm, &FireEffects::REAL);
}

/// The process-wide effects of a firing, apart so the firing path itself can
/// run under test without killing the test binary's children, raising its
/// shutdown flag or marking it fired for every other test.
struct FireEffects {
    mark: fn(&str),
    shutdown: fn(),
    kill: fn() -> usize,
    backstop: fn(),
}

impl FireEffects {
    const REAL: Self = Self {
        mark: |why| {
            *watchdog_fired_lock() = Some(why.to_owned());
        },
        shutdown: crate::shutdown::request_shutdown,
        kill: || crate::shutdown::kill_descendants(std::process::id()),
        backstop: || backstop_exit(),
    };
}

fn fire_with(why: &str, disarm: &std::sync::atomic::AtomicBool, fx: &FireEffects) {
    (fx.mark)(why);
    // The cause, durably, before anything else: if the main thread never
    // unwinds, `backstop_exit` leaves this process without the journal's
    // termination the unwind would have written, and the record would hold
    // observations with nothing saying why they stop. Lock-free - the thread
    // that would write it may be the wedged one, holding the journal's lock.
    journal_watchdog_fired();
    // `error_forced`, not `error`: the output locks may be held by a thread
    // that is blocked for good, and nothing may stand between a fired ceiling
    // and the kill. It also lands the kill in the run log, the one record of
    // how far an incomplete run got.
    output::error_forced(&format!(
        "{why} - killing every process it started (as `brokkr kill --hard` would)"
    ));
    // Runners polling the flag return `Interrupted`, and no new test group is
    // spawned; the exit path reads `watchdog_fired()` first, so this unwinds
    // as 124, not as an interrupt.
    (fx.shutdown)();
    // Two sweeps: a descendant killed mid-fork can leave a child that the
    // first walk did not see. Anything spawned after that is caught by the
    // grace loop below.
    let killed = (fx.kill)() + (fx.kill)();
    output::error_forced(&format!(
        "SIGKILL sent to {}; brokkr exits {WATCHDOG_EXIT_CODE}",
        output::count(killed, "process"),
    ));
    let grace = std::time::Instant::now();
    while grace.elapsed() < UNWIND_GRACE {
        if disarm.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Whatever the unwinding main thread still starts (the next sweep of
        // a phase that does not poll the flag) dies within one tick.
        (fx.kill)();
        std::thread::park_timeout(std::time::Duration::from_millis(100));
    }
    if disarm.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    (fx.backstop)();
}

/// The main thread did not unwind: do the two things its exit path owed and
/// that nothing later can do for it, then exit. The run log, status line and
/// lock metadata are left as a hard kill leaves them (the kernel releases the
/// flock; lockfile readers fail closed on the stale record).
fn backstop_exit() -> ! {
    output::error_forced(&format!(
        "the run did not unwind within {}s of the kill; exiting {WATCHDOG_EXIT_CODE} directly",
        UNWIND_GRACE.as_secs(),
    ));
    // The lock's drop is what normally puts a moved-aside pin back.
    crate::toolchain::restore_active_hold();
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    crate::history_cmd::record_history(&raw_args, process_age_ms(), WATCHDOG_EXIT_CODE, None);
    std::process::exit(WATCHDOG_EXIT_CODE)
}

/// How long this process has been running, from `/proc/self/stat`'s starttime
/// against `/proc/uptime` - the backstop's stand-in for `main`'s own clock,
/// which lives on the thread that did not unwind. Zero when unreadable.
// Float seconds to whole milliseconds: the value is clamped non-negative and a
// process age is nowhere near u64's range, so neither cast can bite.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn process_age_ms() -> u64 {
    let ticks = crate::process::proc_starttime(std::process::id())
        .map(|ticks| ticks.get() as f64);
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|u| u.parse::<f64>().ok()));
    // SAFETY: sysconf takes no pointers.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    match (ticks, uptime) {
        (Some(ticks), Some(uptime)) if hz > 0 => {
            let started = ticks / hz as f64;
            ((uptime - started).max(0.0) * 1000.0) as u64
        }
        _ => 0,
    }
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;

    #[test]
    fn every_phase_uses_the_shared_ceilings_table() {
        for phase in Phase::ALL {
            assert_eq!(phase.ceiling(), crate::test_runner::ceilings::phase_ceiling(phase));
        }
    }

    #[test]
    fn clippy_ceiling_is_five_minutes() {
        assert_eq!(Phase::Clippy.ceiling(), std::time::Duration::from_secs(300));
        assert!(Phase::Test.ceiling() > Phase::Clippy.ceiling());
        assert_eq!(Phase::Prepare.ceiling(), Phase::Test.ceiling());
        assert!(CHECK_CEILING > Phase::Test.ceiling());
    }

    #[test]
    fn begin_phase_sets_both_bookkeepings() {
        let mut failing = None;
        let mut entered = None;
        begin_phase_with(&mut failing, Phase::Gremlins, |phase| entered = Some(phase));
        assert_eq!(failing, Some(Phase::Gremlins));
        assert_eq!(entered, failing);
    }

    /// The real firing path journals the deadline itself, before anything is
    /// killed and without the journal's lock - here held by a "wedged"
    /// thread for the whole firing. If it waited on that lock this test
    /// would hang into its cap; if it left the record to the unwind, the
    /// journal read back would say nothing about why its execution stopped.
    #[test]
    fn a_firing_journals_its_deadline_without_the_journal_lock() {
        let _journal = JOURNAL_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = crate::test_scratch::scratch("watchdog", "firing_journal");
        let unit = BinaryUnit::of(&test_binary_for_tests("core", "lib", "core"));
        let pair = PairId { shape: "s".into(), resolution: Resolution::Combined, unit, test: "queued".into() };
        let plan = AccountingPlan {
            run_id: new_run_id(),
            complete: true,
            // A lane index no other test's tap uses: the journal is process-wide,
            // and a concurrently running test's records may land in it too.
            lanes: vec![LaneRecord {
                inventory: Inventory::Prepared {
                    executions: vec![pair],
                    ignored_selected: Vec::new(),
                    outside_claim: Vec::new(),
                    artifacts: Vec::new(),
                    unlisted: Vec::new(),
                },
                ..LaneRecord::empty(LaneIdx::new(7_777), "lane".into(), LaneKind::Parallel, "s".into())
            }],
            ..AccountingPlan::default()
        };
        let paths = accounting_open(&root, &plan).expect("open the journal");

        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let wedged = std::thread::spawn(move || {
            with_journal_locked(|| {
                locked_tx.send(()).expect("signal");
                release_rx.recv().ok();
            });
        });
        locked_rx.recv().expect("the wedged thread holds the lock");
        let disarmed = std::sync::atomic::AtomicBool::new(true);
        fire_with(
            "test's prepare phase exceeded its ceiling",
            &disarmed,
            &FireEffects { mark: |_| {}, shutdown: || {}, kill: || 0, backstop: || {} },
        );
        release_tx.send(()).expect("release");
        wedged.join().expect("wedged thread");
        journal_close();

        let (records, closed, errors) = read_journal(&paths.journal);
        let r = reconcile(&plan, &records, closed, errors);
        assert!(
            records.iter().any(|rec| matches!(
                rec,
                JournalRecord::Terminated(t)
                    if t.scope == TerminationScope::Run && t.cause == TerminationCause::PhaseDeadline
            )),
            "the firing is on the record: {records:?}"
        );
        let queued = r.accounted.iter().find(|a| a.id.pair.test == "queued").expect("planned");
        assert_eq!((queued.outcome, queued.detail), (Outcome::Unobserved, Some(Detail::PhaseDeadline)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_age_is_plausible() {
        // This test binary has been running for less than a day; a wrong
        // field or unit (ticks read as seconds, say) overshoots that by
        // orders of magnitude.
        let age = process_age_ms();
        assert!(age < 24 * 3600 * 1000, "age {age}ms");
    }

    #[test]
    fn phase_time_left_counts_down_from_the_ceiling() {
        let ceiling = Phase::ScriptCheck.ceiling();
        assert_eq!(phase_time_left_at(None), None);
        let fresh = phase_time_left_at(Some((Phase::ScriptCheck, std::time::Instant::now())))
            .expect("in flight");
        assert!(fresh <= ceiling && fresh > ceiling - std::time::Duration::from_secs(5));
        // A phase already past its ceiling has nothing left, not a wrapped value.
        if let Some(overdue) =
            std::time::Instant::now().checked_sub(ceiling + std::time::Duration::from_secs(1))
        {
            let spent = phase_time_left_at(Some((Phase::ScriptCheck, overdue))).expect("in flight");
            assert_eq!(spent, std::time::Duration::ZERO);
        }
    }
}
