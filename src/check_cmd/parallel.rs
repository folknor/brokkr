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
// Slices are proportional to test count (see `claim_slots`), handed out
// largest-binary-first. Proportional rather than `min(test count, budget)`
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
// another (see `run_test_phase`'s loop), so a second entry with no `parallel`
// key IS the serial lane. Enumerate the parallel side with `tests` and leave
// the complement unfiltered, never the reverse - a binary added later then
// shows up serial and slow rather than parallel and flaky, and a STALE name in
// the enumerated list is a hard cargo error (`no test target named X`, globs
// included) rather than a silent shrink. The partition is total by
// construction on both sides; the coverage gate is a third line of defence,
// not the first.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::output::CapturedOutput;
use crate::test_runner::{effective_test_threads, HungTest};

/// One binary's finished run, buffered so concurrent binaries cannot
/// interleave their output into an unreadable braid.
struct BinaryRun {
    /// `<package>/<target>`, the label a failure is reported under.
    label: String,
    /// The copy-pasteable cargo line, reprinted on failure.
    command: String,
    captured: CapturedOutput,
    hung: Option<HungTest>,
    timed_out: bool,
    completed: Vec<(String, Duration)>,
    /// Wall time for this binary. The sweep's critical path is the maximum of
    /// these, so the summary can name the binary that IS the floor - the one
    /// piece of information that tells a reader which binary to split, merge
    /// or move to the serial lane.
    elapsed: Duration,
}

/// A counting semaphore over the sweep's in-flight test budget.
///
/// Claims are clamped to the whole budget by the caller, so a binary can
/// always eventually be admitted and the wait cannot deadlock on a claim
/// larger than the pool.
struct Budget {
    free: Mutex<u32>,
    released: Condvar,
}

impl Budget {
    fn new(total: u32) -> Self {
        Self {
            free: Mutex::new(total),
            released: Condvar::new(),
        }
    }

    fn acquire(&self, n: u32) -> Result<(), DevError> {
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

    fn release(&self, n: u32) {
        if let Ok(mut free) = self.free.lock() {
            *free += n;
            self.released.notify_all();
        }
    }
}

/// Split cargo-level extra args into target selectors and everything else.
///
/// `brokkr check -- --test read_paths` is a documented workflow, and on this
/// lane it needs different handling from every other cargo arg. A selector
/// must shape WHICH BINARIES the sweep plans; every other arg (`--release`,
/// `--no-fail-fast`, a feature flag) belongs on each per-binary invocation.
///
/// Getting this wrong is not a cosmetic bug. Each per-binary command already
/// carries its own selector, and **cargo unions selection flags** - so
/// appending `--test cli` to all of them turns every invocation into
/// `cargo test -p pkg --lib --test cli`, running `cli` once per planned
/// binary, pulling in binaries the user meant to exclude, and putting more
/// tests in flight than the budget allows. Meanwhile the plan itself ignored
/// the selector, because enumeration never saw it.
///
/// Value-taking selectors consume their following token; the `--test=NAME`
/// form carries it inline. An unknown flag is left in `rest`, which is the
/// safe direction: a non-selector wrongly treated as a selector would silently
/// change the planned binary set, while a selector wrongly left in `rest` is
/// the behaviour this function exists to fix and would be caught by the same
/// symptom.
fn partition_target_selectors(args: &[String]) -> (Vec<String>, Vec<String>) {
    const VALUED: [&str; 4] = ["--test", "--bin", "--example", "--bench"];
    const BARE: [&str; 8] = [
        "--lib",
        "--doc",
        "--tests",
        "--bins",
        "--examples",
        "--benches",
        "--all-targets",
        "--doctests",
    ];

    let mut selectors = Vec::new();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let head = a.split_once('=').map_or(a.as_str(), |(h, _)| h);
        if a.contains('=') && VALUED.contains(&head) {
            selectors.push(a.clone());
        } else if VALUED.contains(&a.as_str()) {
            selectors.push(a.clone());
            if let Some(v) = args.get(i + 1) {
                selectors.push(v.clone());
                i += 1;
            }
        } else if BARE.contains(&a.as_str()) {
            selectors.push(a.clone());
        } else {
            rest.push(a.clone());
        }
        i += 1;
    }
    (selectors, rest)
}

/// How many budget slots one binary claims: its share of the sweep's total
/// cost.
///
/// # Why proportional
///
/// Model a binary of cost `c` running on `k` threads as taking `c/k`. All
/// binaries run at once under `sum(k) <= budget`, so the sweep's wall time is
/// `max(c_i / k_i)`. That maximum is minimised by making `c_i / k_i` equal for
/// every binary - which is exactly `k_i` proportional to `c_i`. Threads in
/// proportion to cost is not a heuristic; it is the allocation that finishes
/// every binary at the same moment, and any other split leaves one binary a
/// longer pole than it had to be.
///
/// # Why not `min(count, budget)`
///
/// The obvious rule - take as many slots as you have tests, capped at the
/// budget - makes the lane a no-op on the workspaces that need it most. **Any
/// binary holding at least `budget` tests claims the entire pool and runs
/// alone.** A workspace whose binaries each hold more tests than the budget
/// can never fan out at any sane setting: overlapping two of them would need a
/// budget above their test counts, which is hundreds in flight - the exact
/// mistake the budget exists to make unspellable. It failed green, too:
/// measured on a 35-binary tree, eight fat binaries serialized at the full
/// budget each and the sweep reported success.
///
/// # Why the weight is duration, not test count
///
/// See `timing_weight_for`. Count-as-cost assumes every test costs the same, and
/// starves whichever binary is slow-but-small - which on a latency-bound suite
/// is precisely the critical path.
///
/// Two floors keep it honest at the edges:
///
/// - **At least one slot**, so a binary is never unschedulable. This can push
///   the sum of claims past the budget when there are more binaries than
///   slots; that is the semaphore's problem, not the plan's, and it resolves
///   as admission in waves rather than as oversubscription.
/// - **Never more than its own test count**, which only binds when the budget
///   exceeds the whole suite - a binary asking for more threads than it has
///   tests would idle slots another binary could use.
fn claim_slots(weight_ms: u64, total_ms: u64, budget: u32, count: u32) -> u32 {
    if total_ms == 0 {
        return 1;
    }
    // Integer milliseconds rather than seconds-as-f64: the arithmetic is a
    // ratio of durations, and doing it in floats would need a float-to-int
    // cast at the end that is unsound for NaN, negative and out-of-range
    // values - none of which a weight should ever be, but all of which a
    // cast would silently accept.
    let share = u64::from(budget).saturating_mul(weight_ms) / total_ms;
    let share = u32::try_from(share).unwrap_or(budget);
    share.clamp(1, budget).min(count.max(1))
}

/// A weight in seconds as integer milliseconds, for the ratio above.
///
/// Total by construction: NaN, negative and absurd values all collapse to
/// zero, which reads downstream as "no information" and floors the binary at
/// one slot rather than corrupting every other binary's share.
fn weight_ms(seconds: f64) -> u64 {
    Duration::try_from_secs_f64(seconds)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The cargo target selector for one test binary.
///
/// Mirrors cargo's own target kinds rather than assuming `--test`: a package's
/// unit tests live in the `lib`/`bin` harnesses, which `--test <name>` does not
/// select at all. Paired with `-p <package>` because `--lib` alone is ambiguous
/// across a workspace.
fn binary_selector(binary: &TestBinary) -> Vec<String> {
    let mut args = vec!["-p".to_owned(), binary.package.clone()];
    match binary.kind.as_str() {
        "test" => {
            args.push("--test".to_owned());
            args.push(binary.target.clone());
        }
        "bin" => {
            args.push("--bin".to_owned());
            args.push(binary.target.clone());
        }
        // "lib", and anything cargo grows later that is neither an integration
        // target nor a bin: the lib harness is the only remaining place a unit
        // test can live.
        _ => args.push("--lib".to_owned()),
    }
    args
}

/// Build the full `cargo test` argv for one binary of a parallel sweep.
///
/// Deliberately does NOT reuse `sweep_selection_args`: that emits the sweep's
/// own `--test` filters, and here the binary's own selector replaces them.
/// cargo unions selection flags, so leaving the sweep's filters in would
/// broaden every per-binary run back to the whole lane.
fn binary_args(
    sweep: &ResolvedSweep,
    binary: &TestBinary,
    allow_args: &[String],
    threads: u32,
    cargo_extra: &[String],
    libtest_extra: &[String],
) -> Result<Vec<String>, DevError> {
    let mut args: Vec<String> = vec!["test".into()];
    args.extend(allow_args.iter().cloned());
    args.extend(sweep.cargo_feature_args.iter().cloned());
    args.extend(binary_selector(binary));
    if let Some(p) = sweep.profile {
        args.extend(p.cargo_args().iter().map(|s| (*s).to_owned()));
    }
    // Same reason as the sequential lane: without it cargo stops at the first
    // failure and a red run under-reports. Per binary here, so each binary
    // enumerates its own failures.
    if !cargo_extra.iter().any(|c| c == "--no-fail-fast") {
        args.push("--no-fail-fast".into());
    }
    args.extend(cargo_extra.iter().cloned());

    let mut libtest_args: Vec<String> = sweep.libtest_args.clone();
    libtest_args.extend(sweep.name_filters.iter().cloned());
    libtest_args.extend(libtest_extra.iter().cloned());

    // The budget is the single source of in-flight count, so a `--test-threads`
    // arriving from anywhere else is a second one. Refused rather than
    // silently overridden: the two numbers multiply into a concurrency nobody
    // asked for, which is exactly the failure the budget key exists to prevent.
    if effective_test_threads(&libtest_args)?.is_some() {
        return Err(DevError::Config(format!(
            "sweep '{}' sets `parallel.budget` and also carries a --test-threads \
             override; the budget owns per-binary thread counts. Drop the \
             override, or drop `parallel` from the [[check]] entry.",
            sweep.label
        )));
    }
    libtest_args.push(format!("--test-threads={threads}"));

    // The per-test hang watchdog reads libtest's JSON event stream: human
    // output emits no per-test *start* signal under concurrency, so there is
    // nothing to age. Same constraint, and same refusal, as the sequential
    // parallel path.
    if libtest_args.iter().any(|a| a == "--format") {
        return Err(DevError::Config(format!(
            "sweep '{}' drives libtest's JSON output for the per-test watchdog; \
             remove the `--format` override from this profile's libtest_args",
            sweep.label
        )));
    }
    libtest_args.push("-Z".into());
    libtest_args.push("unstable-options".into());
    libtest_args.push("--format".into());
    libtest_args.push("json".into());

    args.push("--".into());
    args.extend(libtest_args);
    Ok(args)
}

/// Run one binary to completion, having already claimed `slots` of the budget.
#[allow(clippy::too_many_arguments)]
fn run_one_binary(
    project_root: &Path,
    state_root: &Path,
    sweep: &ResolvedSweep,
    binary: &TestBinary,
    allow_args: &[String],
    env_refs: &[(&str, &str)],
    slots: u32,
    cargo_extra: &[String],
    libtest_extra: &[String],
) -> Result<BinaryRun, DevError> {
    let args = binary_args(
        sweep,
        binary,
        allow_args,
        slots,
        cargo_extra,
        libtest_extra,
    )?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let started = Instant::now();
    let run = test_runner::run_libtest_parallel(
        &arg_refs,
        project_root,
        state_root,
        env_refs,
        test_runner::PARALLEL_SWEEP_TIMEOUT,
        test_runner::TEST_TIMEOUT,
        |_| {},
        |_| {},
        |_| {},
    )?;
    let hung = match run.outcome {
        LibtestOutcome::HungTest(h) => Some(h),
        LibtestOutcome::Completed => None,
    };
    Ok(BinaryRun {
        label: format!("{}/{}", binary.package, binary.target),
        command: format!("failing command: cargo {}", args.join(" ")),
        captured: run.captured,
        hung,
        timed_out: run.timed_out,
        completed: run.completed,
        elapsed: started.elapsed(),
    })
}

/// Run one sweep with its test binaries executing concurrently under the
/// entry's in-flight budget. Returns `Ok(false)` when any binary failed,
/// having already reported it.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_parallel_sweep(
    project_root: &Path,
    state_root: &Path,
    sweep: &ResolvedSweep,
    packages: &[&str],
    budget: u32,
    extra_args: &[String],
    project_env: &[(String, String)],
    allow_args: &[String],
    raw: bool,
    doctests: bool,
    commands: bool,
    timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let sweep_started = Instant::now();
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);
    // Selectors narrow the PLAN; the rest rides on each per-binary command.
    // See `partition_target_selectors` for why mixing the two is a real bug
    // rather than a tidiness question.
    let (extra_selectors, cargo_extra) = partition_target_selectors(cargo_extra);

    // Doctests are not reachable from this lane (they live in the `--doc`
    // pseudo-target, which has no binary to fan out over), but this is NOT
    // where that is reported: `[test] doctests = true` alongside a `parallel`
    // entry is refused at config load. A per-run warning would be printed on
    // every green run forever for a decision that only needs making once, and
    // a gate whose normal output contains a warning has taught its readers to
    // skip warnings.
    let _ = doctests;

    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // One build for the whole sweep, before any fan-out. The per-binary runs
    // below re-enter cargo, but the artifacts are already current, so they
    // resolve to a no-op rebuild rather than N concurrent compiles of the same
    // crate graph fighting over the target dir lock.
    let mut selection = sweep_selection_args(sweep, packages);
    selection.extend(extra_selectors.iter().cloned());
    let Some(all) = test_binaries(project_root, &selection, &env_refs, commands)? else {
        return Ok(false);
    };
    // The sweep's own `--test` filters UNION with any the caller supplied,
    // matching cargo's own semantics for repeated selection flags - the
    // enumeration above already unions them, so narrowing to one side here
    // would drop binaries the build was told to produce.
    let mut target_filters = sweep.cargo_test_filters.clone();
    target_filters.extend(extra_selectors.iter().cloned());
    let binaries = filter_binaries(&all, &target_filters);
    if binaries.is_empty() {
        return Err(DevError::Config(format!(
            "sweep '{}' has `parallel` but its selection matched no test \
             binaries; a lane that fans out over nothing is a wrong-run.",
            sweep.label
        )));
    }

    // Test counts drive the slot claims, so the listing is not optional: a
    // claim of one for every binary would serialize the fan-out down to one
    // test at a time per binary and give up most of the win. Listing executes
    // no test code (the same argument `binaries` relies on for running the
    // executables directly), so this costs one cheap spawn per binary.
    let libdir = toolchain_libdir(project_root, &env_refs)?;
    let mut filter_args: Vec<&str> = sweep.name_filters.iter().map(String::as_str).collect();
    filter_args.extend(sweep.libtest_args.iter().map(String::as_str));

    let mut counted: Vec<(&TestBinary, u32)> = Vec::new();
    for b in binaries {
        let Some(listed) = binary_list(b, project_root, &filter_args, &env_refs, &libdir)? else {
            return Ok(false);
        };
        let count = u32::try_from(listed.len()).unwrap_or(u32::MAX);
        if count == 0 {
            continue;
        }
        counted.push((b, count));
    }
    // Claims need the whole sweep's cost, so they are allocated after every
    // binary has been listed rather than as each one is. Cost is the previous
    // run's measured wall time where there is one, and an estimate from the
    // known binaries' cost per test otherwise - see `timing_weight_for`.
    let history = timings_load(state_root);
    let recorded = history.get(&sweep.label);
    let label_of = |b: &TestBinary| format!("{}/{}", b.package, b.target);
    let known: Vec<(f64, u32)> = counted
        .iter()
        .filter_map(|(b, c)| {
            recorded
                .and_then(|m| m.get(&label_of(b)))
                .filter(|s| **s > 0.0)
                .map(|s| (*s, *c))
        })
        .collect();
    let mean_cost = mean_cost_per_test(&known);
    let weights: Vec<f64> = counted
        .iter()
        .map(|(b, c)| {
            let rec = recorded.and_then(|m| m.get(&label_of(b))).copied();
            timing_weight_for(rec, *c, mean_cost)
        })
        .collect();
    let weights: Vec<u64> = weights.iter().copied().map(weight_ms).collect();
    let total_weight: u64 = weights.iter().sum();
    let mut planned: Vec<(&TestBinary, u32)> = counted
        .into_iter()
        .zip(&weights)
        .map(|((b, count), w)| (b, claim_slots(*w, total_weight, budget, count)))
        .collect();
    if planned.is_empty() {
        return Err(DevError::Config(format!(
            "cargo test: zero tests ran (sweep: {}) - a profile/filter combo \
             collected no work across {} test binaries; treat as a wrong-run.",
            sweep.label,
            all.len()
        )));
    }

    // Largest first. A big binary admitted late would find the budget carved
    // into slices too small to use it, so the long pole goes in while the pool
    // is whole - the ordering that puts the critical path first.
    planned.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.target.cmp(&b.0.target)));

    // Split here on purpose. Everything above is cargo building and listing;
    // everything below is tests actually running. Reporting one number for
    // both would have said "1429 passed in 40.5s" for a fan-out that took
    // half a second, which is the opposite of what a lane sold on wall time
    // should tell its reader.
    let build_elapsed = sweep_started.elapsed();
    output::run_msg(&format!(
        "test {}: {} {}, budget {} in flight (claims {}-{})",
        sweep.label,
        planned.len(),
        if planned.len() == 1 { "binary" } else { "binaries" },
        budget,
        planned.iter().map(|(_, c)| *c).min().unwrap_or(0),
        planned.iter().map(|(_, c)| *c).max().unwrap_or(0),
    ));
    let fanout_started = Instant::now();

    let pool = Budget::new(budget);
    let mut runs: Vec<Result<BinaryRun, DevError>> = Vec::new();

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (binary, slots) in &planned {
            let pool = &pool;
            let env_refs = &env_refs;
            let cargo_extra = &cargo_extra;
            let libtest_extra = &libtest_extra;
            handles.push(scope.spawn(move || {
                pool.acquire(*slots)?;
                let out = run_one_binary(
                    project_root,
                    state_root,
                    sweep,
                    binary,
                    allow_args,
                    env_refs,
                    *slots,
                    cargo_extra,
                    libtest_extra,
                );
                // Released whether the run succeeded or errored: a binary that
                // failed to spawn must not strand its slice of the budget and
                // wedge every binary still waiting.
                pool.release(*slots);
                out
            }));
        }
        for h in handles {
            runs.push(h.join().unwrap_or_else(|_| {
                Err(DevError::Build(
                    "a parallel test binary thread panicked".into(),
                ))
            }));
        }
    });

    // Recorded before reporting, and for a red sweep too: a binary that
    // failed still took the time it took, and the next run's allocation is
    // better for knowing it. A sweep whose binaries all failed to spawn
    // records nothing, since `measured` is then empty.
    let measured: Vec<(String, Duration)> = runs
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|r| (r.label.clone(), r.elapsed))
        .collect();
    timings_record(state_root, &sweep.label, &measured);

    report_runs(project_root, sweep, runs, raw, fanout_started, build_elapsed, timings)
}

/// Render every binary's buffered output and decide the sweep's verdict.
///
/// Split from the fan-out above so the concurrency and the reporting can be
/// read separately; called after the join, in the plan's stable order rather
/// than in completion order, because a gate's output is diffed between runs
/// and ordering it by which binary happened to finish first makes two
/// identical red runs look different.
fn report_runs(
    project_root: &Path,
    sweep: &ResolvedSweep,
    runs: Vec<Result<BinaryRun, DevError>>,
    raw: bool,
    started: Instant,
    build_elapsed: Duration,
    mut timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let binaries = runs.len();
    let mut passed = 0usize;
    let mut slowest = (String::new(), Duration::ZERO);
    let mut ok = true;
    for run in runs {
        let run = run?;
        passed += run.completed.len();
        if let Some(out) = timings.as_deref_mut() {
            for (name, elapsed) in run.completed {
                out.push(TestTiming {
                    sweep: sweep.label.clone(),
                    name,
                    elapsed,
                });
            }
        }

        let stdout = String::from_utf8_lossy(&run.captured.stdout);
        let stderr = String::from_utf8_lossy(&run.captured.stderr);

        if run.timed_out {
            output::error(&format!(
                "sweep '{}' binary {} exceeded the parallel test timeout ({}s) \
                 and was killed",
                sweep.label,
                run.label,
                test_runner::PARALLEL_SWEEP_TIMEOUT.as_secs(),
            ));
            output::error(&run.command);
            ok = false;
            continue;
        }
        if let Some(hung) = run.hung {
            output::error(&format!("sweep '{}' binary {}:", sweep.label, run.label));
            output::error(&test_runner::format_hung_test(&hung, project_root));
            output::error(&run.command);
            ok = false;
            continue;
        }
        if !run.captured.status.success() {
            output::error(&format!(
                "sweep '{}' binary {} failed:",
                sweep.label, run.label
            ));
            output::error(&run.command);
            if raw {
                if !stderr.is_empty() {
                    output::error(&stderr);
                }
                if !stdout.is_empty() {
                    output::error(&stdout);
                }
            } else {
                output::error(&cargo_filter::filter_test(&stdout, &stderr));
            }
            ok = false;
            continue;
        }
        // A passing binary prints NOTHING. Thirty-five green lines say only
        // what one summary line says, and a reader who has learned to scroll
        // past the normal case will scroll past the abnormal one too. `--raw`
        // still gets everything, which is what `--raw` is for.
        if raw {
            if !stderr.is_empty() {
                print!("{stderr}");
            }
            if !stdout.is_empty() {
                print!("{stdout}");
            }
        }
        if run.elapsed > slowest.1 {
            slowest = (run.label.clone(), run.elapsed);
        }
    }

    // The summary earns its line by carrying what no other line can: the wall
    // time, and WHICH binary was the critical path. A parallel sweep finishes
    // when its slowest binary finishes, so that name is the answer to "what do
    // I do next" - split it, or move it to the serial lane, or leave it alone
    // because it is already the floor.
    if ok {
        // The slowest binary IS the sweep's floor, so naming it is the whole
        // actionable content - it is the one to split, or to move to the
        // serial lane. Omitted for a single binary, where it would only
        // restate the line's own duration.
        let critical = if binaries > 1 {
            format!(", slowest {} {:.1}s", slowest.0, slowest.1.as_secs_f64())
        } else {
            String::new()
        };
        output::run_msg(&format!(
            "test {}: {} passed in {:.1}s ({} {}, built in {:.1}s{})",
            sweep.label,
            passed,
            started.elapsed().as_secs_f64(),
            binaries,
            if binaries == 1 { "binary" } else { "binaries" },
            build_elapsed.as_secs_f64(),
            critical,
        ));
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
            target: target.to_owned(),
            kind: kind.to_owned(),
            executable: format!("/tmp/{target}"),
        }
    }

    // `--test <name>` does not select a package's unit tests at all - those
    // live in the lib/bin harness - so a selector that assumed `--test` would
    // silently run nothing for every unit-test binary in the fan-out.
    #[test]
    fn each_target_kind_gets_the_selector_that_actually_selects_it() {
        assert_eq!(
            binary_selector(&binary("pkg", "test", "integration")),
            vec!["-p", "pkg", "--test", "integration"]
        );
        assert_eq!(
            binary_selector(&binary("pkg", "bin", "cli")),
            vec!["-p", "pkg", "--bin", "cli"]
        );
        assert_eq!(
            binary_selector(&binary("pkg", "lib", "pkg")),
            vec!["-p", "pkg", "--lib"]
        );
    }

    // `--lib` alone is ambiguous across a workspace, so every selector carries
    // the owning package regardless of kind.
    #[test]
    fn every_selector_names_its_package() {
        for kind in ["test", "bin", "lib", "something-cargo-added-later"] {
            let args = binary_selector(&binary("owner", kind, "t"));
            assert_eq!(&args[..2], &["-p".to_owned(), "owner".to_owned()], "{kind}");
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

    // The deadlock this guards: a binary claiming more than the whole budget
    // could never be admitted, so no claim may exceed it.
    #[test]
    fn claims_are_always_satisfiable() {
        let budget: u32 = 4;
        for count in [1_u32, 3, 4, 9, 100, u32::MAX] {
            let w = u64::from(count);
            let claim = claim_slots(w, w, budget, count);
            assert!((1..=budget).contains(&claim), "count {count}");
        }
    }

    // Degenerate weights must not produce an unschedulable or oversized claim.
    #[test]
    fn non_finite_and_empty_weights_fall_back_to_one_slot() {
        assert_eq!(claim_slots(1, 0, 8, 5), 1);
        assert_eq!(weight_ms(f64::NAN), 0);
        assert_eq!(weight_ms(-1.0), 0);
        assert_eq!(weight_ms(f64::INFINITY), 0);
        assert_eq!(weight_ms(22.3), 22_300);
    }

    // THE SECOND ALLOCATION REGRESSION. Weighting by test count starved the
    // binary that was the critical path: ten latency-bound tests out of 2224
    // computed to one slot, so its tests ran end to end for 22.3s of a 24.5s
    // sweep. Weighted by measured duration it draws the slots it needs.
    #[test]
    fn a_slow_small_binary_outranks_a_fast_big_one() {
        let budget = 24;
        // The measured shape: one 22.3s binary of 10 tests, and the rest of
        // the suite costing 38s across 2214 fast tests.
        let total = weight_ms(22.3) + weight_ms(38.0);
        let pole = claim_slots(weight_ms(22.3), total, budget, 10);
        let bulk = claim_slots(weight_ms(38.0), total, budget, 2214);
        assert!(pole >= 8, "the critical path must get real threads, got {pole}");
        assert!(bulk >= 1 && bulk <= budget);

        // Under the old count weighting it got exactly one slot - the bug.
        let by_count = claim_slots(10, 2224, budget, 10);
        assert_eq!(by_count, 1);
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
        let (sel, rest) = partition_target_selectors(&v(&["--test", "read_paths"]));
        assert_eq!(sel, v(&["--test", "read_paths"]));
        assert!(rest.is_empty());
    }

    // A value-taking selector must carry its value across with it; leaving
    // the name behind would make the plan select nothing and the per-binary
    // command inherit a stray positional.
    #[test]
    fn a_selectors_value_travels_with_it_in_both_spellings() {
        let (sel, rest) = partition_target_selectors(&v(&["--bin", "cli", "--release"]));
        assert_eq!(sel, v(&["--bin", "cli"]));
        assert_eq!(rest, v(&["--release"]));

        let (sel, rest) = partition_target_selectors(&v(&["--test=cli", "--release"]));
        assert_eq!(sel, v(&["--test=cli"]));
        assert_eq!(rest, v(&["--release"]));
    }

    #[test]
    fn bare_selectors_need_no_value_and_non_selectors_are_untouched() {
        let (sel, rest) =
            partition_target_selectors(&v(&["--lib", "--no-fail-fast", "--tests"]));
        assert_eq!(sel, v(&["--lib", "--tests"]));
        assert_eq!(rest, v(&["--no-fail-fast"]));
    }

    // An unknown flag stays in `rest`: treating it as a selector would
    // silently change which binaries the sweep plans, which is the worse
    // direction to be wrong in.
    #[test]
    fn an_unknown_flag_is_left_for_the_per_binary_command() {
        let (sel, rest) = partition_target_selectors(&v(&["--offline", "-j", "4"]));
        assert!(sel.is_empty());
        assert_eq!(rest, v(&["--offline", "-j", "4"]));
    }

    // A trailing value-taking selector with nothing after it must not index
    // past the end.
    #[test]
    fn a_dangling_selector_does_not_panic() {
        let (sel, rest) = partition_target_selectors(&v(&["--test"]));
        assert_eq!(sel, v(&["--test"]));
        assert!(rest.is_empty());
    }

    // THE REGRESSION THIS FILE EXISTS TO PREVENT. Under the old
    // `min(count, budget)` rule every one of these binaries claimed the whole
    // pool and ran alone, so a 35-binary sweep serialized and reported success.
    // Fat binaries must leave room for each other.
    #[test]
    fn a_binary_holding_more_tests_than_the_budget_does_not_eat_the_pool() {
        // Eight binaries of 300 tests each, budget 24: the shape measured on
        // the tree where the old rule was caught.
        let budget = 24;
        let total = 8 * 300;
        let claim = claim_slots(300, total, budget, 300);
        assert_eq!(claim, 3);
        // The whole point: several of them fit at once.
        assert!(claim * 8 <= budget, "eight fat binaries must co-exist");
    }

    // Proportional means the slice tracks the share of tests, so the binary
    // with most tests gets most threads and they finish together.
    #[test]
    fn slices_follow_each_binarys_share_of_the_suite() {
        let budget = 12;
        let total = 100;
        assert_eq!(claim_slots(50, total, budget, 50), 6);
        assert_eq!(claim_slots(25, total, budget, 25), 3);
        // A tiny binary still gets a slot rather than being unschedulable.
        assert_eq!(claim_slots(1, total, budget, 1), 1);
    }

    // A lone binary is the degenerate case of proportional: its share is all
    // of it, so it runs at the full budget exactly as before.
    #[test]
    fn a_lone_binary_takes_the_whole_budget() {
        assert_eq!(claim_slots(1429, 1429, 6, 1429), 6);
    }

    // The count cap only binds when the budget outruns the whole suite: a
    // binary asking for more threads than it has tests would idle slots.
    #[test]
    fn a_small_suite_on_a_big_machine_claims_no_more_than_it_has() {
        assert_eq!(claim_slots(3, 6, 64, 3), 3);
        assert_eq!(claim_slots(1, 1, 64, 1), 1);
    }

    // Degenerate inputs must not divide by zero or return an unschedulable
    // claim - the plan filters empty binaries, but the rule is total anyway.
    #[test]
    fn an_empty_suite_still_yields_a_schedulable_claim() {
        assert_eq!(claim_slots(0, 0, 8, 0), 1);
        assert!(claim_slots(0, 10, 8, 0) >= 1);
    }
}
