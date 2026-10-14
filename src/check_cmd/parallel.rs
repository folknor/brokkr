// `parallel = { budget = N }` execution: the sweep's test binaries run at the
// same time instead of one after another.
//
// THE FLOOR THIS DISSOLVES. cargo runs each test binary sequentially, and
// `--test-threads` parallelizes only *within* a binary. So a sweep's wall time
// has a hard floor at the sum, over binaries, of each binary's slowest test -
// and no amount of `test_threads` moves it, which is why a project that has
// already tuned threads sees 8 and 16 measure identically. The clearest waste
// is a single-test binary contributing its whole duration because it has
// nobody to overlap with. Running the binaries concurrently collapses that sum
// to a maximum.
//
// THE BUDGET COUNTS TESTS, NOT BINARIES, and that is the whole design of the
// key. Binaries times threads is the real in-flight count: seven binaries at
// `test_threads = 8` is fifty-six concurrent tests, which on any ordinary box
// is slower than the merged-binary figure the lane is chasing, not faster. A
// key naming concurrent *binaries* would let a config ask for that while
// looking like it asked for seven. The number a project has already tuned is
// the total, so the total is what the key takes: each binary claims a slice of
// the budget and runs under a matching `--test-threads`.
//
// Slices are proportional to serial duration (see `binary_timings::allocate`),
// handed out largest-binary-first. Proportional rather than
// `min(test count, budget)`
// because that obvious rule silently defeats the whole lane: a binary holding
// at least `budget` tests claims the entire pool and runs alone, so a
// workspace whose binaries each hold more tests than the budget never fans
// out at any sane setting. Measured on a 35-binary tree before it was fixed.
//
// WHAT THIS DOES NOT DO IS ISOLATE. Tests within one binary still share a
// process, deliberately: a shared-process parallel lane is the only place the
// process-global-state class (two tests contending over a global logger and a
// shared capture buffer) is visible at all, and `isolation = "process"`
// dissolves that contention along with the bug's visibility. This lane keeps
// the detector.
//
// What it ADDS is exposure to MACHINE-global state. Several binaries at once
// is several processes at once, so a per-machine singleton - a daemon holding
// an instance lock, a fixed socket path, a shared state dir - becomes
// contended where a sequential lane never showed it. That class arrives as
// FLAKES rather than as clean failures, which is the thing most likely to get
// this lane blamed for a defect it merely exposed.
//
// There is deliberately NO per-entry serial-group key for that, because the
// sweep list already composes: `[[check]]` entries run strictly one after
// another (see `run_test_phase`'s loop, `test_phase.rs`), so a second entry with no `parallel`
// key IS the serial lane. Enumerate the parallel side with `tests` and leave
// the complement unfiltered, never the reverse - a binary added later then
// shows up serial and slow rather than parallel and flaky, and a STALE name in
// the enumerated list is a hard cargo error (`no test target named X`, globs
// included) rather than a silent shrink. The partition is total by
// construction on both sides; the coverage gate is a third line of defence,
// not the first.

// THE FAN-OUT EXECUTES THE PREBUILT BINARIES DIRECTLY. The lane's
// preparation (prepare.rs) prebuilds once with the sweep's whole selection
// and lists every binary; the lane then runs each enumerated test executable
// itself, with the libtest argv - it never re-enters cargo. The
// previous shape (`cargo test -p <pkg> --test <t>` per binary) was
// structurally unsound: cargo's feature resolution follows the ROOT UNIT SET,
// not just the package selection, so a `-p`-scoped re-entry could resolve
// shared dependencies with different features than the sweep-wide prebuild
// (measured: hyper losing its `server` feature under `-p <daemon>`, one lone
// rustc serialized on the build lock for five minutes where the warm sweep
// takes fourteen seconds). Direct execution makes graph identity a
// non-question: the executable the prebuild emitted IS what runs, for every
// selection shape - which is what makes `parallel` legal on sweeps carrying
// `test_exclude_packages`, the selection the old re-entry could not fan out.
//
// Cargo does more than exec a test binary, though: it launches it with a cwd
// and environment contract (package-root cwd, loader path, `[env]` config,
// OUT_DIR, CARGO_PKG_*, runtime CARGO_BIN_EXE_*). `DirectRuntime`
// (direct_runtime.rs) reconstructs that contract from cargo metadata and the
// prebuild's own artifact stream; a configured target runner - a wrapper
// direct execution would silently bypass - refuses the lane at resolution
// time.
//
// The whole-workspace feature-unification promotion (`auto` ->
// `-Zfeature-unification workspace` on an eligible sweep) is UNCHANGED, and
// now shapes only the prebuild and enumeration: it decides which graph the
// binaries are built from, and existing configs keep compiling the graph
// they compiled before. What direct execution retires is the *need* for the
// pin on any runner - so an explicit `feature_unification = "selected"` on a
// parallel whole-workspace sweep, formerly refused as undeliverable, is now
// simply honoured.
//
// Nightly-only, like the lane itself (libtest's JSON format needs
// `-Z unstable-options`). A cargo too old for a pinned `-Zfeature-unification`
// still fails the prebuild loudly with a named remedy.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::output::CapturedOutput;
use crate::test_runner::{effective_test_threads, HungTest, LaneVerdict};

/// One binary's finished run, buffered so concurrent binaries cannot
/// interleave their output into an unreadable braid.
struct BinaryRun {
    /// `<package>/<target>`, the label a failure is reported under.
    label: String,
    /// `<package>/<kind>:<target>`, the identity used by the timing cache.
    /// The report label intentionally stays shorter, but cache identity must
    /// distinguish same-named targets of different kinds.
    timing_key: String,
    /// The copy-pasteable cargo line, reprinted on failure.
    command: String,
    captured: CapturedOutput,
    hung: Option<HungTest>,
    verdict: LaneVerdict,
    /// What the run's observed events reduce to; the counts come from here.
    outcome: test_runner::ExecutionOutcome,
    completed: Vec<(String, Duration)>,
    /// Wall time for this binary. Used for the summary's critical-path line -
    /// and NOT for the next run's allocation, because wall time is a function
    /// of the slots this run granted, so feeding it back oscillates. See
    /// `binary_timings`' header.
    elapsed: Duration,
    /// Never started, because an earlier binary blew its budget and the lane is
    /// stopping. Reported as such rather than counted either way.
    skipped: bool,
}

impl BinaryRun {
    /// A binary that reached the front of the queue after the lane was aborted.
    fn skipped_after_abort(binary: &TestBinary) -> Self {
        Self {
            label: format!("{}/{}", binary.package, binary.target),
            timing_key: binary_timing_key(binary),
            command: String::new(),
            captured: CapturedOutput {
                // Never spawned: an exit of 0 would read as a pass; the
                // `skipped` flag and the incomplete verdict carry the truth.
                ending: crate::output::Ending::Interrupted,
                stdout: Vec::new(),
                stderr: Vec::new(),
                elapsed: Duration::ZERO,
            },
            hung: None,
            verdict: LaneVerdict::Incomplete,
            outcome: test_runner::ExecutionOutcome::default(),
            completed: Vec::new(),
            elapsed: Duration::ZERO,
            skipped: true,
        }
    }
}

/// A counting semaphore over the sweep's in-flight test budget.
///
/// Claims are clamped to the whole budget by the caller, so a binary can
/// always eventually be admitted and the wait cannot deadlock on a claim
/// larger than the pool.
pub(crate) struct Budget {
    free: Mutex<u32>,
    released: Condvar,
}

impl Budget {
    pub(crate) fn new(total: u32) -> Self {
        Self {
            free: Mutex::new(total),
            released: Condvar::new(),
        }
    }

    pub(crate) fn acquire(&self, n: u32) -> Result<(), DevError> {
        let mut free = self
            .free
            .lock()
            .map_err(|_| DevError::Build("test budget mutex poisoned".into()))?;
        while *free < n {
            free = self
                .released
                .wait(free)
                .map_err(|_| DevError::Build("test budget mutex poisoned".into()))?;
        }
        *free -= n;
        Ok(())
    }

    pub(crate) fn release(&self, n: u32) {
        if let Ok(mut free) = self.free.lock() {
            *free += n;
            self.released.notify_all();
        }
    }
}

// The pin itself now lives on the sweep as `effective_unification`, resolved
// once per invocation by `profile::resolve_unification` and read here through
// `sweep.unification_args()`. The prebuild and every per-binary invocation take
// it from that one field, which is what keeps them naming one graph - the
// property this lane's local helper used to enforce by being the only caller.

/// Stable timing-cache identity for one planning unit. Keep it separate from
/// the shorter failure label: a package can have a bin target and an
/// integration-test target with the same name.
fn binary_timing_key(binary: &TestBinary) -> String {
    format!("{}/{}", binary.package, binary.label())
}

/// The libtest argv one binary of a parallel sweep is executed with,
/// directly - no cargo half, since the fan-out runs the prebuilt executable
/// itself.
fn direct_libtest_args(
    sweep: &PlannedSweep,
    threads: u32,
    libtest_extra: &[String],
) -> Result<Vec<String>, DevError> {
    test_runner::LibtestArgs::validate_passthrough(libtest_extra)?;
    let filters = sweep.lane_filters();
    let mut libtest_args = filters.libtest_args();
    libtest_args.extend(filters.name_filters());
    libtest_args.extend(libtest_extra.iter().cloned());

    // This runner is only valid for the typed parallel lane. Its budget
    // allocates the one in-flight count passed to each prebuilt binary; the
    // only remaining source of a competing --test-threads flag is forwarded
    // libtest argv.
    if !matches!(sweep.lane_exec, crate::profile::LaneExec::Parallel { .. }) {
        return Err(DevError::Config(format!(
            "sweep '{}' reached the parallel runner without a parallel execution policy.",
            sweep.label
        )));
    }
    if effective_test_threads(&libtest_args)?.is_some() {
        return Err(DevError::Config(format!(
            "sweep '{}' sets `parallel.budget` and also carries a --test-threads \
             override; the budget owns per-binary thread counts. Drop the override, \
             or drop `parallel` from the [[check]] entry.",
            sweep.label
        )));
    }
    libtest_args.push(format!("--test-threads={threads}"));

    Ok(libtest_args)
}

/// One planned binary of a parallel lane, with the resolution it belongs to.
struct PlannedRun<'a> {
    resolution: &'a Option<String>,
    planned: &'a PreparedBinary,
}

/// The forwarded args every binary of a lane shares, and the lane's planned
/// requests (a binary's repro line carries its resolution's features).
struct DirectExtras<'a> {
    attempt: &'a Attempt,
    libtest_extra: &'a [String],
    allow_args: &'a [String],
    /// The forwarded cargo args the prebuild carried (target selectors
    /// already applied to the plan), for each binary's reproduction line.
    forwarded: ForwardedArgs,
}

/// Run one binary to completion, having already claimed `slots` of the
/// budget. Executes the prebuilt binary directly under the cargo launch
/// envelope. Everything the binary's stream says
/// is journaled through `tap` as it arrives, attributed to this binary.
#[allow(clippy::too_many_arguments)]
fn run_one_binary(
    state_root: &Path,
    sweep: &PlannedSweep,
    run: &PlannedRun<'_>,
    extras: &DirectExtras<'_>,
    runtime: &DirectRuntime,
    env_refs: &[(&str, &str)],
    slots: u32,
    tap: &LaneTap,
    abort: &std::sync::atomic::AtomicBool,
) -> Result<BinaryRun, DevError> {
    let binary = &run.planned.binary;
    let origin = StreamOrigin::Binary {
        resolution: run.resolution.clone(),
        unit: run.planned.unit.clone(),
        one_test: None,
    };
    let args = direct_libtest_args(sweep, slots, extras.libtest_extra)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (cwd, env) = runtime.envelope(binary, env_refs)?;
    let env_pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let sink_origin = origin.clone();
    let sink = tap.sink(move |_| sink_origin.clone(), false);
    let started = Instant::now();
    let observe = test_runner::Observe {
        sink: Some(sink),
        shim: test_runner::ShimMode::Off,
    };
    let run_result = test_runner::run_libtest(
        test_runner::Launch::Direct {
            program: &binary.executable,
        },
        test_runner::LibtestInvocation {
            args: test_runner::LibtestArgs::from_argv(test_runner::Launch::Direct { program: &binary.executable }, &arg_refs)?,
            cwd: &cwd,
            state_root,
            env: &env_pairs,
            context: test_runner::RunContext::process(),
            // Shared with every concurrent binary: the first to blow its
            // budget cancels the rest mid-flight.
            abort: Some(abort),
        },
        test_runner::Ceilings::shared_harness_with(test_runner::TEST_TIMEOUT),
        &observe,
        |_| {},
        |_| {},
        |_| {},
    );
    if let Err(DevError::Spawn { error, .. }) = &run_result {
        tap.record(JournalRecord::SpawnFailed { lane: tap.lane(), origin, detail: error.to_string() });
    }
    let finished = run_result?;
    let hung = finished.hung;
    let verdict = test_runner::lane_verdict(&finished.outcome);
    Ok(BinaryRun {
        label: format!("{}/{}", binary.package, binary.target),
        timing_key: binary_timing_key(binary),
        command: ReproLine::parallel_binary(
            sweep,
            binary,
            extras.attempt.run_for(run.resolution.as_deref()),
            extras.allow_args,
            &extras.forwarded,
        )
        .as_str()
        .to_owned(),
        captured: finished.captured,
        hung,
        verdict,
        outcome: finished.outcome,
        completed: finished.completed,
        elapsed: started.elapsed(),
        skipped: false,
    })
}

/// Run one prepared sweep with its test binaries executing concurrently under
/// the entry's in-flight budget. Returns `Ok(false)` when any binary failed,
/// having already reported it.
///
/// The prebuild, the launch envelope and the listing all happened in
/// preparation (`prepare_direct`): this lane runs the binaries it was handed,
/// and the counts that drive its slot claims are the plan's.
///
/// Doctests are not reachable from this lane (they live in the `--doc`
/// pseudo-target, which has no binary to fan out over), but this is NOT where
/// that is reported: admission checks require a serial/doc-only carrier when
/// `[test] doctests = true`. A per-run warning would be printed on every green
/// run forever for a decision that only needs making once.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_parallel_sweep(
    project_root: &Path,
    state_root: &Path,
    sweep: &PlannedSweep,
    attempt: &Attempt,
    budget: u32,
    prepared: &PreparedLane,
    tap: &LaneTap,
    commands: bool,
    timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let sweep_started = Instant::now();
    announce_sweep(
        &format!(
            "test {}: {}",
            sweep.label,
            describe_sweep(sweep, true, attempt.selection(), attempt.described_features())
        ),
        None,
        commands,
    );
    let Some(runtime) = prepared.runtime.as_ref() else {
        return Err(DevError::Build(format!("sweep '{}' reached the parallel lane unprepared", sweep.label)));
    };
    let env_refs = prepared.env.refs();
    let binaries: Vec<PlannedRun<'_>> = prepared
        .resolutions
        .iter()
        .flat_map(|r| r.binaries.iter().map(move |planned| PlannedRun { resolution: &r.resolution, planned }))
        .collect();
    if binaries.is_empty() {
        return Err(DevError::Config(format!(
            "sweep '{}' has `parallel` but its selection matched no test \
             binaries; a lane that fans out over nothing is a wrong-run.",
            sweep.label
        )));
    }

    // Recorded serial costs (or test counts when there is no history) drive
    // the slot claims: a claim of one for every binary would serialize the
    // fan-out down to one test at a time per binary and give up most of the
    // win. A binary whose filters select nothing never enters the plan.
    let mut counted: Vec<(PlannedRun<'_>, u32)> = Vec::new();
    for b in binaries {
        let count = u32::try_from(b.planned.selected.len()).unwrap_or(u32::MAX);
        if count == 0 {
            continue;
        }
        counted.push((b, count));
    }
    // The pure allocator sees the complete listing and prior serial costs for
    // this entry. The cache key is the entry name, not the lane-qualified
    // label, so `default` and `tier1/default` share the same measurements.
    let history = timings_load(state_root);
    let recorded = history.get(&sweep.entry_name);
    let allocation_inputs: Vec<(String, u32)> = counted
        .iter()
        .map(|(run, count)| (binary_timing_key(&run.planned.binary), *count))
        .collect();
    let mut by_key: BTreeMap<String, PlannedRun<'_>> = counted
        .into_iter()
        .map(|(run, _)| {
            (
                binary_timing_key(&run.planned.binary),
                PlannedRun {
                    resolution: run.resolution,
                    planned: run.planned,
                },
            )
        })
        .collect();
    let planned: Vec<(PlannedRun<'_>, u32)> = allocate(&allocation_inputs, recorded, budget)
        .into_iter()
        .filter_map(|(key, slots)| by_key.remove(&key).map(|run| (run, slots)))
        .collect();
    if planned.is_empty() {
        return Err(DevError::Config(format!(
            "cargo test: zero tests ran (sweep: {}) - a profile/filter combo \
             collected no work across {} test binaries; treat as a wrong-run.",
            sweep.label, prepared.enumerated
        )));
    }

    // Split here on purpose. Everything above is cargo building and listing;
    // everything below is tests actually running. Reporting one number for
    // both would have said "1429 passed in 40.5s" for a fan-out that took
    // half a second, which is the opposite of what a lane sold on wall time
    // should tell its reader.
    let build_elapsed = sweep_started.elapsed();
    let plan_line = format!(
        "test {}: {} {}, budget {} in flight (claims {}-{})",
        sweep.label,
        planned.len(),
        if planned.len() == 1 { "binary" } else { "binaries" },
        budget,
        planned.iter().map(|(_, c)| *c).min().unwrap_or(0),
        planned.iter().map(|(_, c)| *c).max().unwrap_or(0),
    );
    output::detail(&plan_line);
    output::status(&plan_line);
    let claims: Vec<(&TestBinary, u32)> = planned.iter().map(|(b, c)| (&b.planned.binary, *c)).collect();
    warn_serialized_claims(sweep, budget, &claims);
    let fanout_started = Instant::now();

    let pool = Budget::new(budget);
    let mut runs: Vec<Result<BinaryRun, DevError>> = Vec::new();
    // A timeout cancels both running siblings and work not yet dequeued.
    let aborted = std::sync::atomic::AtomicBool::new(false);
    let extras = DirectExtras {
        attempt,
        libtest_extra: &prepared.libtest_extra,
        allow_args: &prepared.env.allow_args,
        forwarded: ForwardedArgs::parse(&prepared.cargo_extra),
    };

    let queue = std::sync::Mutex::new(planned.iter());
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..planned.len().min(usize::try_from(budget).unwrap_or(usize::MAX)) {
            let pool = &pool;
            let env_refs = &env_refs;
            let aborted = &aborted;
            let extras = &extras;
            let queue = &queue;
            handles.push(scope.spawn(move || {
                let mut completed = Vec::new();
                loop {
                    let next = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).next();
                    let Some((run, slots)) = next else { break; };
                    let out = match pool.acquire(*slots) {
                        Err(error) => Err(error),
                        Ok(()) => {
                            let out = if aborted.load(std::sync::atomic::Ordering::SeqCst) {
                                Ok(BinaryRun::skipped_after_abort(&run.planned.binary))
                            } else {
                                run_one_binary(state_root, sweep, run, extras, runtime,
                                    env_refs, *slots, tap, aborted)
                            };
                            if out.as_ref().is_ok_and(|r| r.hung.is_some()) {
                                aborted.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                            pool.release(*slots);
                            out
                        }
                    };
                    completed.push(out);
                }
                completed
            }));
        }
        for h in handles {
            runs.extend(h.join().unwrap_or_else(|_| {
                vec![Err(DevError::Build("a parallel test binary thread panicked".into()))]
            }));
        }
    });

    // An interrupt (or the watchdog) killed the in-flight groups and refused
    // the queued ones: the runs are not a verdict and their timings are
    // truncated, so neither is reported nor recorded. What they observed is
    // already journaled; the stop is what explains the rest.
    if crate::shutdown::is_shutdown_requested() {
        tap.terminate(TerminationScope::Lane, stop_cause(), None);
        return Err(DevError::Interrupted);
    }
    // A blown budget stopped the queued binaries: theirs is a sibling's
    // timeout, recorded as such rather than left to look like a silence.
    if aborted.load(std::sync::atomic::Ordering::SeqCst) {
        tap.terminate(TerminationScope::Lane, TerminationCause::SiblingTimeout, None);
    }

    // Recorded before reporting, and for a red sweep too: a binary that
    // failed still took the time it took, and the next run's allocation is
    // better for knowing it. A sweep whose binaries all failed to spawn
    // records nothing, since `measured` is then empty.
    let measured: Vec<(String, BinaryCost)> = runs
        .iter()
        .filter_map(|r| r.as_ref().ok())
        // A queued binary skipped after a sibling timeout has no observation;
        // do not replace its previous measurement with a fabricated zero.
        .filter(|r| !r.skipped)
        .map(|r| {
            // Serial cost, not `r.elapsed`: the sum of a binary's own tests
            // does not move when its allocation moves, and wall time does.
            let serial = r
                .completed
                .iter()
                .map(|(_, duration)| *duration)
                .fold(Duration::ZERO, |serial, duration| {
                    serial.saturating_add(duration)
                });
            let slowest = r
                .completed
                .iter()
                .map(|(_, duration)| *duration)
                .fold(Duration::ZERO, Duration::max);
            (r.timing_key.clone(), BinaryCost { serial, slowest })
        })
        .collect();
    timings_record(state_root, &sweep.entry_name, &measured);

    report_runs(project_root, sweep, runs, fanout_started, build_elapsed, timings)
}

/// Warn when the plan put binaries beyond each other's reach: two binaries
/// that each claimed the whole budget cannot overlap, however the rest of the
/// lane schedules. That is the failure the claims spread exists to expose, and
/// it otherwise reports success - so it is the one part of the plan that
/// prints on a green run. A budget of one serializes by design, so it never
/// warns.
///
/// Worded as what the claims prove and no more: smaller binaries may still
/// overlap around the full-budget ones, so this does not say the lane ran
/// serially.
fn warn_serialized_claims(sweep: &PlannedSweep, budget: u32, planned: &[(&TestBinary, u32)]) {
    if budget <= 1 {
        return;
    }
    let full: Vec<String> = planned
        .iter()
        .filter(|(_, c)| *c >= budget)
        .map(|(b, _)| format!("{}/{}", b.package, b.target))
        .collect();
    if full.len() >= 2 {
        output::warn(&format!(
            "test {}: {} binaries each claimed the full budget ({budget}) and could not overlap: {}",
            sweep.label,
            full.len(),
            full.join(", ")
        ));
    }
}

/// An error's message without its variant prefix, for a line that names the
/// sweep itself. `Display` leads with `build: ` / `verify: ` for those
/// variants, which inside `sweep 'x': ...` read as a second, unrelated
/// scope (`sweep 'x': build: ...`).
fn error_detail(e: &DevError) -> String {
    match e {
        DevError::Build(m) | DevError::Verify(m) | DevError::Config(m) | DevError::Reported(m) => {
            m.clone()
        }
        other => other.to_string(),
    }
}

/// Render every binary's buffered output and decide the sweep's verdict.
///
/// Split from the fan-out above so the concurrency and the reporting can be
/// read separately; called after the join, in the plan's stable order rather
/// than in completion order, because a gate's output is diffed between runs
/// and ordering it by which binary happened to finish first makes two
/// identical red runs look different.
#[allow(clippy::too_many_lines)]
fn report_runs(
    project_root: &Path,
    sweep: &PlannedSweep,
    runs: Vec<Result<BinaryRun, DevError>>,
    started: Instant,
    build_elapsed: Duration,
    mut timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let binaries = runs.len();
    let mut passed = 0usize;
    let mut ignored = 0usize;
    let mut slowest = (String::new(), Duration::ZERO);
    let mut ok = true;
    // Set when any binary blew its budget: the lane stops rather than reporting
    // an ordinary failed sweep, and the remaining reports are still rendered
    // first so the operator sees what did run.
    let mut budget_blown: Option<String> = None;
    let mut skipped = 0usize;
    // Binaries whose run errored rather than reported (a spawn failure, a
    // panicked thread, a poisoned budget). Collected, not returned at once:
    // `run?` here used to abandon every later binary's report - including the
    // one that blew its budget - on the first such error, so the operator saw
    // one spawn error and none of the failures that had already run.
    let mut errors: Vec<DevError> = Vec::new();
    for run in runs {
        let run = match run {
            Ok(run) => run,
            Err(e) => {
                ok = false;
                errors.push(e);
                continue;
            }
        };
        if run.skipped {
            skipped += 1;
            continue;
        }
        let (observed_passed, _, observed_ignored) = run.outcome.counts();
        let completed_count = run.outcome.terminal_count();
        passed += usize::try_from(observed_passed)
            .map_err(|_| DevError::Build("observed pass count exceeds usize".into()))?;
        ignored += usize::try_from(observed_ignored)
            .map_err(|_| DevError::Build("observed ignored count exceeds usize".into()))?;
        if let Some(out) = timings.as_deref_mut() {
            for (name, elapsed) in &run.completed {
                out.push(TestTiming {
                    sweep: sweep.label.clone(),
                    name: name.clone(),
                    elapsed: *elapsed,
                });
            }
        }

        let stdout = String::from_utf8_lossy(&run.captured.stdout);
        let stderr = String::from_utf8_lossy(&run.captured.stderr);

        match run.verdict {
            LaneVerdict::Pass => {}
            LaneVerdict::BudgetBlown => {
                output::error(&format!("sweep '{}' binary {}:", sweep.label, run.label));
                if let Some(hung) = &run.hung {
                    output::error(&test_runner::format_hung_test(hung, project_root));
                } else {
                    output::error("the test runner observed a timed-out execution");
                }
                output::error(&run.command);
                ok = false;
                budget_blown = Some(run.label.clone());
                continue;
            }
            LaneVerdict::Stopped => return Err(DevError::Interrupted),
            LaneVerdict::Fail => {
                output::error(&format!(
                    "sweep '{}' binary {} failed:",
                    sweep.label, run.label
                ));
                output::error(&run.command);
                output::error(&test_runner::filter_test(&stdout, &stderr));
                ok = false;
                continue;
            }
            LaneVerdict::Incomplete => {
                output::error(&format!(
                    "sweep '{}' binary {} exited without a complete test observation stream; \
                     observed {completed_count} completed test(s). A binary that stops talking \
                     has not shown its tests passed - treat as a wrong-run.",
                    sweep.label, run.label
                ));
                output::error(&run.command);
                ok = false;
                continue;
            }
        }
        // A passing binary prints NOTHING. Thirty-five green lines say only
        // what one summary line says, and a reader who has learned to scroll
        // past the normal case will scroll past the abnormal one too. To watch
        // one run, run it: `brokkr test <NAME>` streams its output live.
        if run.elapsed > slowest.1 {
            slowest = (run.label.clone(), run.elapsed);
        }
    }

    // Every binary that did run has spoken; now the errors. The first is
    // returned (the caller voices an `Err`'s message), so any others are
    // printed here or they would be lost. A blown budget still outranks them
    // below, since it is the verdict that stops the run.
    let mut errors = errors.into_iter();
    let first_error = errors.next();
    for extra in errors {
        output::error(&format!("sweep '{}': {}", sweep.label, error_detail(&extra)));
    }

    // A blown budget ends the run rather than failing a sweep. Reported after the
    // loop so every binary that did run has already spoken.
    if let Some(label) = budget_blown {
        if let Some(e) = &first_error {
            output::error(&format!("sweep '{}': {}", sweep.label, error_detail(e)));
        }
        if skipped > 0 {
            output::error(&format!(
                "{skipped} binary/binaries in sweep '{}' were not started, because the lane \
                 stopped when {label} exceeded its budget",
                sweep.label
            ));
        }
        // `Reported`: the budget diagnosis and the failing command printed above.
        return Err(DevError::Reported(format!(
            "binary {label} exceeded its time budget in sweep '{}' - stopping",
            sweep.label
        )));
    }
    if let Some(e) = first_error {
        return Err(e);
    }

    // The summary carries what no other line can: the wall time, the build
    // time kept apart from it, and WHICH binary was the critical path. A
    // parallel sweep finishes when its slowest binary finishes, so that name
    // is the answer to "what do I do next" - split it, or move it to the
    // serial lane, or leave it alone because it is already the floor. The
    // per-sweep line goes to the run log; the grouped test line names the
    // slowest parallel binary across every sweep.
    if ok {
        // Omitted for a single binary, where it would only restate the line's
        // own duration.
        let critical = if binaries > 1 {
            format!(", slowest {} {:.1}s", slowest.0, slowest.1.as_secs_f64())
        } else {
            String::new()
        };
        output::detail(&format!(
            "test {}: {} passed in {:.1}s ({} {}, built in {:.1}s{})",
            sweep.label,
            passed,
            started.elapsed().as_secs_f64(),
            binaries,
            if binaries == 1 { "binary" } else { "binaries" },
            build_elapsed.as_secs_f64(),
            critical,
        ));
        note_tests(passed, ignored, 0);
        if binaries > 1 {
            note_parallel_slowest(&sweep.label, &slowest.0, slowest.1);
        }
        // The execution unit here is the sweep, as a cargo resolution is in
        // the serial lane: a binary listing nothing never enters the plan, and
        // an empty plan is refused as a wrong-run. What remains is a plan
        // whose binaries completed nothing between them - every listed test
        // `#[ignore]`d - which passes green and must be named, not summed.
        if passed == 0 {
            note_empty_unit(sweep.label.clone());
        }
    }

    Ok(ok)
}

#[cfg(test)]
mod parallel_lane_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn binary(package: &str, kind: &str, target: &str) -> TestBinary {
        TestBinary {
            package: package.to_owned(),
            package_id: format!("path+file:///x/{package}#{package}@0.1.0"),
            target: target.to_owned(),
            kind: kind.to_owned(),
            executable: format!("/t/deps/{target}"),
            manifest_dir: std::path::PathBuf::from(format!("/x/{package}")),
        }
    }

    #[test]
    fn error_detail_drops_the_variant_prefix() {
        assert_eq!(error_detail(&DevError::Build("spawn failed".into())), "spawn failed");
        assert_eq!(error_detail(&DevError::Reported("stopping".into())), "stopping");
        assert!(error_detail(&DevError::Lock("held".into())).contains("held"));
    }

    // `--test <name>` does not select a package's unit tests at all - those
    // live in the lib/bin harness - so a selector that assumed `--test` would
    // silently run nothing for every unit-test binary in the fan-out.
    #[test]
    fn each_target_kind_gets_the_selector_that_actually_selects_it() {
        use selection::cargo_target_selector;
        assert_eq!(cargo_target_selector("test", "integration"), vec!["--test", "integration"]);
        assert_eq!(cargo_target_selector("bin", "cli"), vec!["--bin", "cli"]);
        assert_eq!(cargo_target_selector("lib", "pkg"), vec!["--lib"]);
        assert_eq!(cargo_target_selector("something-cargo-added-later", "t"), vec!["--lib"]);
    }

    // `--lib` alone is ambiguous across a workspace, so a reproduction line
    // with no planned request names the owning package regardless of kind.
    #[test]
    fn every_unplanned_repro_names_its_package() {
        for kind in ["test", "bin", "lib", "something-cargo-added-later"] {
            let line = ReproLine::parallel_binary(
                &bare_sweep(),
                &binary("owner", kind, "t"),
                None,
                &[],
                &ForwardedArgs::parse(&[]),
            );
            assert!(line.as_str().contains("-p owner"), "{kind}: {}", line.as_str());
        }
    }

    // The budget is a semaphore, not a counter of binaries: a claim is only
    // admitted when the whole slice is free, and releasing wakes the waiter.
    #[test]
    fn a_claim_waits_for_its_whole_slice_and_release_admits_it() {
        let pool = Budget::new(8);
        pool.acquire(5).unwrap();
        pool.acquire(3).unwrap();
        // Pool is now empty; prove the next claim is blocked by showing it
        // succeeds only after a release.
        pool.release(3);
        pool.acquire(3).unwrap();
        assert_eq!(*pool.free.lock().unwrap(), 0);
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }

    // THE OTHER REGRESSION. Each per-binary command already carries its own
    // selector and cargo UNIONS selection flags, so a `--test cli` copied
    // onto all of them ran `cli` once per planned binary and dragged in
    // binaries the caller meant to exclude. Selectors shape the plan instead.
    #[test]
    fn target_selectors_are_split_out_of_the_per_binary_args() {
        let (sel, rest) = ForwardedArgs::parse(&v(&["--test", "read_paths"])).partition_targets();
        assert_eq!(sel, v(&["--test", "read_paths"]));
        assert!(rest.is_empty());
    }

    // A value-taking selector must carry its value across with it; leaving
    // the name behind would make the plan select nothing and the per-binary
    // command inherit a stray positional.
    #[test]
    fn a_selectors_value_travels_with_it_in_both_spellings() {
        let (sel, rest) = ForwardedArgs::parse(&v(&["--bin", "cli", "--release"])).partition_targets();
        assert_eq!(sel, v(&["--bin", "cli"]));
        assert_eq!(rest, v(&["--release"]));

        let (sel, rest) = ForwardedArgs::parse(&v(&["--test=cli", "--release"])).partition_targets();
        assert_eq!(sel, v(&["--test=cli"]));
        assert_eq!(rest, v(&["--release"]));
    }

    #[test]
    fn bare_selectors_need_no_value_and_non_selectors_are_untouched() {
        let (sel, rest) =
            ForwardedArgs::parse(&v(&["--lib", "--no-fail-fast", "--tests"])).partition_targets();
        assert_eq!(sel, v(&["--lib", "--tests"]));
        assert_eq!(rest, v(&["--no-fail-fast"]));
    }

    // An unknown flag stays in `rest`: treating it as a selector would
    // silently change which binaries the sweep plans, which is the worse
    // direction to be wrong in.
    #[test]
    fn an_unknown_flag_is_left_for_the_cargo_build() {
        let (sel, rest) = ForwardedArgs::parse(&v(&["--offline", "-j", "4"])).partition_targets();
        assert!(sel.is_empty());
        assert_eq!(rest, v(&["--offline", "-j", "4"]));
    }

    // A trailing value-taking selector with nothing after it must not index
    // past the end.
    #[test]
    fn a_dangling_selector_does_not_panic() {
        let (sel, rest) = ForwardedArgs::parse(&v(&["--test"])).partition_targets();
        assert_eq!(sel, v(&["--test"]));
        assert!(rest.is_empty());
    }

    fn bare_decl() -> crate::profile::SweepDecl {
        crate::profile::sweep_from_check_entry(&crate::config::CheckEntry {
            name: "default".into(),
            ..Default::default()
        })
    }

    fn bare_sweep() -> PlannedSweep {
        crate::profile::PlannedSweep::from(bare_decl())
    }

    // A selector forwarded after `--` narrows the selection the same way
    // brokkr's own `-p` does, so it rules workspace promotion out
    // (`resolve_invocation_sweeps`). Every spelling cargo accepts has to
    // count: the attached short form is the one a hand-written scan misses,
    // and missing it means a one-package run resolving against the whole
    // workspace's features. (The sweep-side conditions - its own `packages`
    // and `test_exclude_packages`, a CLI `-p`, a default-members subset - are
    // the same resolver's, read before it asks cargo.)
    #[test]
    fn a_forwarded_package_selector_rules_promotion_out() {
        for extra in [
            vec!["-p".to_owned(), "one".to_owned()],
            vec!["-p=one".to_owned()],
            vec!["-pone".to_owned()],
            vec!["--package".to_owned(), "one".to_owned()],
            vec!["--package=one".to_owned()],
            vec!["--exclude".to_owned(), "bad".to_owned()],
            vec!["--exclude=bad".to_owned()],
            // Incomplete, and cargo will reject it - but brokkr does not
            // get to guess what it meant, so it reads as narrowing.
            vec!["-p".to_owned()],
        ] {
            assert!(
                ForwardedArgs::parse(&extra).narrows_package_selection(),
                "{extra:?} must rule promotion out"
            );
        }

        // Non-selection args leave the whole-workspace reading intact.
        for extra in [
            vec!["--no-fail-fast".to_owned()],
            vec!["--release".to_owned()],
            vec!["--profile=release".to_owned()],
            vec!["--workspace".to_owned()],
        ] {
            assert!(
                !ForwardedArgs::parse(&extra).narrows_package_selection(),
                "{extra:?} must not rule promotion out"
            );
        }
    }

    // Promotion turns an `auto` parallel sweep's unification into the
    // workspace pin; without eligibility it stays ambient.
    #[test]
    fn promotion_pins_auto_to_the_workspace() {
        let mut parallel = bare_decl();
        parallel.lane_exec = crate::profile::LaneExec::Parallel { budget: 4 };
        let promoted = resolve_planned_sweep(parallel.clone(), true).unwrap();
        assert!(promoted.unification_args().iter().any(|a| a.contains("feature-unification")));
        let ambient = resolve_planned_sweep(parallel, false).unwrap();
        assert!(ambient.unification_args().is_empty());
    }

    // The direct-exec argv is libtest-only: filters, the budget's thread
    // count, and the JSON format the watchdog reads - never a cargo flag.
    #[test]
    fn direct_argv_is_libtest_only_with_threads_and_json() {
        let mut sweep = bare_decl();
        sweep.lane_exec = crate::profile::LaneExec::Parallel { budget: 4 };
        sweep.filters = crate::profile::LaneFilters {
            filters: vec![
                crate::profile::Filter {
                    kind: crate::profile::FilterKind::Skip,
                    pattern: "slow_io".into(),
                    package: None,
                    origin: crate::profile::FilterOrigin::Entry("default".into()),
                },
                crate::profile::Filter {
                    kind: crate::profile::FilterKind::Only,
                    pattern: "fast_".into(),
                    package: None,
                    origin: crate::profile::FilterOrigin::Entry("default".into()),
                },
            ],
            ..Default::default()
        };
        let args = direct_libtest_args(&crate::profile::PlannedSweep::from(sweep), 3, &[]).unwrap();
        assert_eq!(
            args,
            v(&[
                "--skip",
                "slow_io",
                "fast_",
                "--test-threads=3"
            ])
        );
    }

    // A `--test-threads` from anywhere else is a second in-flight count; the
    // budget owns it, so it is refused, exactly as before the rewrite.
    #[test]
    fn a_foreign_test_threads_override_is_still_refused() {
        // A sweep that is not on the parallel lane never reaches this runner.
        assert!(direct_libtest_args(&bare_sweep(), 3, &[]).is_err());
        let mut parallel = bare_decl();
        parallel.lane_exec = crate::profile::LaneExec::Parallel { budget: 4 };
        let parallel = crate::profile::PlannedSweep::from(parallel);
        assert!(direct_libtest_args(&parallel, 3, &v(&["--test-threads", "2"])).is_err());
    }

    // The failure reprint stays cargo-shaped (rebuilds and supplies env when
    // pasted), and carries the sweep's pin so it resolves the same graph the
    // prebuild did.
    #[test]
    fn the_repro_line_is_cargo_shaped_and_carries_the_pin() {
        let b = binary("pkg", "test", "t");
        let promoted = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            feature_unification: crate::config::FeatureUnification::Workspace,
            ..bare_sweep().declaration().clone()
        });
        let none = ForwardedArgs::parse(&[]);
        let line = ReproLine::parallel_binary(&promoted, &b, None, &[], &none);
        let line = line.as_str();
        assert!(line.starts_with("failing command: cargo test"), "{line}");
        assert!(line.contains("-Zfeature-unification"), "{line}");
        assert!(line.contains("-p pkg --test t"), "{line}");
        // Unpinned `auto` reproduces without a pin - the pre-existing shape.
        let plain = ReproLine::parallel_binary(&bare_sweep(), &b, None, &[], &none);
        assert!(!plain.as_str().contains("-Zfeature-unification"), "{}", plain.as_str());
    }

    // The repro keeps the building request whole: a combined resolution
    // over `a` and `b` reproduces as that selection with its features, the
    // binary narrowing only the target - `-p a` alone would re-unify the
    // graph.
    #[test]
    fn the_repro_line_keeps_the_planned_requests_selection() {
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            packages: vec!["a".into(), "b".into()],
            ..bare_sweep().declaration().clone()
        });
        let selection = PhaseSelection::for_check(
            SelectionPhase::Test,
            std::slice::from_ref(&sweep),
            &[],
            &FeatureOracle::unavailable(),
        )
        .unwrap();
        let request = selection.attempt(LaneIdx::new(0)).unwrap().runs()[0].clone();
        let none = ForwardedArgs::parse(&[]);
        let line = ReproLine::parallel_binary(&sweep, &binary("a", "test", "t"), Some(&request), &[], &none);
        assert!(line.as_str().contains("-p a -p b"), "{}", line.as_str());
        assert!(line.as_str().contains("--test t"), "{}", line.as_str());
        let lib = ReproLine::parallel_binary(&sweep, &binary("a", "lib", "a"), Some(&request), &[], &none);
        assert!(lib.as_str().contains("-p a -p b"), "{}", lib.as_str());
        assert!(lib.as_str().ends_with("--lib"), "{}", lib.as_str());
    }

    // Forwarded cargo args only cargo-mediated execution can honour are
    // refused: `--no-run` contradicts running, and `--target` / `--config` /
    // `--manifest-path` / `--target-dir` move the platform, env, or artifact
    // paths out from under the direct launch envelope.
    #[test]
    fn unsupported_forwarded_cargo_args_are_refused() {
        let sweep = bare_sweep();
        for extra in [
            v(&["--no-run"]),
            v(&["--target", "aarch64-unknown-linux-gnu"]),
            v(&["--target=wasm32-wasip1"]),
            v(&["--config", "target.x.runner=\"qemu\""]),
            v(&["--manifest-path", "../other/Cargo.toml"]),
            v(&["--target-dir=/elsewhere"]),
        ] {
            assert!(reject_direct_execution_args(&sweep, &ForwardedArgs::parse(&extra)).is_err(), "{extra:?}");
        }
        for extra in [v(&["--release"]), v(&["--offline"]), v(&["-j", "4"])] {
            assert!(reject_direct_execution_args(&sweep, &ForwardedArgs::parse(&extra)).is_ok(), "{extra:?}");
        }
    }

    #[test]
    fn package_mode_refuses_forwarded_package_selectors() {
        let pkg = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            feature_unification: crate::config::FeatureUnification::Package,
            ..bare_sweep().declaration().clone()
        });
        // Every spelling cargo accepts, including the attached short form:
        // one that slipped through would union a second package into a
        // resolution promised to hold exactly one.
        for extra in [
            vec!["-p".to_owned(), "other".to_owned()],
            vec!["-pother".to_owned()],
            vec!["--package=other".to_owned()],
            vec!["--workspace".to_owned()],
            vec!["--exclude=x".to_owned()],
        ] {
            assert!(
                reject_forwarded_package_selectors(&pkg, &ForwardedArgs::parse(&extra)).is_err(),
                "{extra:?} must be refused"
            );
        }
        // Non-selection args are none of this rule's business.
        assert!(reject_forwarded_package_selectors(&pkg, &ForwardedArgs::parse(&["--release".to_owned()])).is_ok());
        // And the rule applies to package mode alone.
        assert!(
            reject_forwarded_package_selectors(
                &bare_sweep(),
                &ForwardedArgs::parse(&["-p".to_owned(), "o".to_owned()]),
            )
            .is_ok()
        );
    }
}
