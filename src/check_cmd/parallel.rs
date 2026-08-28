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
// Slices are handed out largest-binary-first. A binary claims
// `min(test count, budget)` - a single-test binary claims one slot and
// overlaps with everything, which is precisely the waste named above,
// collected as the default rather than as something to tune.
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
use std::time::Duration;

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
    slots: u32,
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
        slots,
    })
}

/// Run one sweep with its test binaries executing concurrently under the
/// entry's in-flight budget. Returns `Ok(false)` when any binary failed,
/// having already reported it.
#[allow(clippy::too_many_arguments)]
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
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);

    // Doctests live in the `--doc` pseudo-target, which has no `--list`-able
    // executable and so is not one of the binaries this lane fans out over.
    // Announced rather than swallowed, on the same reasoning as the
    // process-isolated lane: a project that set `[test] doctests = true` is
    // owed the news that this sweep did not honour it.
    if doctests {
        output::warn(&format!(
            "test {}: doctests are not run on a `parallel` sweep (no test \
             binary to fan out over); run them from a sweep without `parallel`",
            sweep.label
        ));
    }

    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // One build for the whole sweep, before any fan-out. The per-binary runs
    // below re-enter cargo, but the artifacts are already current, so they
    // resolve to a no-op rebuild rather than N concurrent compiles of the same
    // crate graph fighting over the target dir lock.
    let selection = sweep_selection_args(sweep, packages);
    let Some(all) = test_binaries(project_root, &selection, &env_refs, commands)? else {
        return Ok(false);
    };
    let binaries = filter_binaries(&all, &sweep.cargo_test_filters);
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

    let mut planned: Vec<(&TestBinary, u32)> = Vec::new();
    for b in binaries {
        let Some(listed) = binary_list(b, project_root, &filter_args, &env_refs, &libdir)? else {
            return Ok(false);
        };
        let count = u32::try_from(listed.len()).unwrap_or(u32::MAX);
        if count == 0 {
            continue;
        }
        // Clamped to the whole budget so the claim is always satisfiable -
        // an unclamped claim larger than the pool would wait forever.
        // `budget >= 1` is a load-time guarantee, so the clamp bounds are
        // always ordered.
        planned.push((b, count.clamp(1, budget)));
    }
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

    output::run_msg(&format!(
        "test {}: {} test binaries in parallel, budget {} test(s) in flight",
        sweep.label,
        planned.len(),
        budget
    ));

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

    report_runs(project_root, sweep, runs, raw, timings)
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
    mut timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let mut ok = true;
    for run in runs {
        let run = run?;
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
        if raw {
            if !stderr.is_empty() {
                print!("{stderr}");
            }
            if !stdout.is_empty() {
                print!("{stdout}");
            }
        } else {
            output::run_msg(&format!(
                "test {}: {} ok ({} slot(s))",
                sweep.label, run.label, run.slots
            ));
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
    // could never be admitted. The plan clamps, so the pool never sees one.
    #[test]
    fn clamped_claims_are_always_satisfiable() {
        let budget: u32 = 4;
        for test_count in [1_u32, 3, 4, 9, 100] {
            let claim = test_count.clamp(1, budget);
            assert!((1..=budget).contains(&claim), "count {test_count}");
        }
    }

    // A single-test binary claims one slot and overlaps with everything -
    // the specific waste (a binary contributing its full duration because it
    // has nobody to overlap with) that the lane exists to collect.
    #[test]
    fn a_single_test_binary_claims_one_slot() {
        assert_eq!(1_u32.clamp(1, 8), 1);
    }
}
