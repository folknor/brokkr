// `isolation = "process"` execution.
//
// `--test-threads=1` serializes tests inside one process per test binary;
// it does not isolate them. Tests that touch process-global state (a
// global logger) pass under CI's nextest - which runs process-per-test -
// and fail in any shared-process libtest lane, because the first test's
// init is still resident for the ninth. This path provides the guarantee
// the tests actually need: the prepared plan holds each binary of the
// sweep's prebuilt selection and the names its filters select
// (`prepare_direct`), and the lane runs that exact binary once per selected
// test. Re-entering cargo with -p changes the feature graph; re-entering
// with the whole selection runs same-named tests in other binaries under one
// wall cap. DirectRuntime supplies cargo's launch environment for the
// prebuilt binary.

// Not used here any more, but every check_cmd/*.rs shares one module
// (include!'d into check_cmd.rs), and binary_timings.rs reads this import.
use std::collections::BTreeMap;

/// Run one prepared process-isolated sweep. Runs every test even after
/// failures (the per-test failure list is the point of the mode), returns
/// Ok(false) when any failed.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_isolated_sweep(
    project_root: &Path,
    state_root: &Path,
    sweep: &ResolvedSweep,
    attempt: &Attempt,
    prepared: &PreparedLane,
    tap: &LaneTap,
    doctests: bool,
    commands: bool,
    mut timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    // Doctests cannot be enumerated per test binary (they live in the
    // `--doc` pseudo-target, which has no `--list`-able executable), so a
    // process-isolated lane cannot run them. Announce the omission rather
    // than swallow a project's `[test] doctests = true` in silence.
    if doctests {
        output::warn(&format!(
            "{}: `doctests = true` has no effect on a process-isolated sweep - \
             doctests cannot be enumerated per binary, so this lane runs none",
            sweep.label
        ));
    }

    let env_refs = prepared.env.refs();

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
        return Err(DevError::Build(format!("sweep '{}' reached the isolated lane unprepared", sweep.label)));
    };
    let plan = IsolatedPlan {
        cases: prepared
            .resolutions
            .iter()
            .flat_map(|r| {
                r.binaries.iter().flat_map(move |b| {
                    b.selected.iter().map(move |name| IsolatedCase {
                        binary: b.binary.clone(),
                        unit: b.unit.clone(),
                        resolution: r.resolution.clone(),
                        ignored: b.ignored.contains(name),
                        name: name.clone(),
                    })
                })
            })
            .collect(),
        pkg_skipped: prepared.pkg_skipped,
        include_ignored: prepared.include_ignored,
    };

    let Some((runnable, pkg_skipped)) = plan_runnable(&plan, &sweep.label) else {
        return Ok(false);
    };

    let runnable_count = runnable.len();
    let mut failed = 0usize;
    let mut ignored = 0usize;
    for case in &runnable {
        let name = &case.name;
        if case.ignored && !plan.include_ignored {
            ignored += 1;
            output::detail(&format!(
                "SKIP {name} (#[ignore], lane runs without --include-ignored)"
            ));
            continue;
        }
        // The harness too: the same name in two harnesses is two runs now.
        output::status(&format!("test {}: {}/{} {name}", sweep.label, case.binary.package, case.binary.target));
        let outcome = run_one_isolated_test(
            project_root,
            state_root,
            case,
            runtime,
            plan.include_ignored,
            &env_refs,
            tap,
            commands,
            &sweep.label,
        )?;
        match outcome {
            // A blown budget stops the lane. Collapsing it into `Failed` made it
            // indistinguishable from an assertion failure, so the loop counted it
            // and ran the next selected test - and the next, and the next -
            // returning an ordinary failed-sweep result at the end. Whatever
            // wedged the killed test is still there for its successors to
            // inherit, and the contract says stop.
            IsolatedOutcome::TimedOut => {
                record_isolated_timeout(tap, case);
                // `Reported`: run_one_isolated_test printed the diagnosis, which
                // names the test, its package/target and sweep, and the failing
                // command. `check` does not echo a `Reported` label, so that
                // printed line is the only place the sweep appears.
                return Err(DevError::Reported(format!(
                    "test '{name}' in {}/{} exceeded its time budget in sweep '{}' - stopping",
                    case.binary.package, case.binary.target, sweep.label
                )));
            }
            IsolatedOutcome::Failed => failed += 1,
            IsolatedOutcome::Passed(elapsed) => {
                if let Some(out) = timings.as_deref_mut()
                    && let Some(e) = elapsed
                {
                    out.push(TestTiming {
                        sweep: sweep.label.clone(),
                        name: name.clone(),
                        elapsed: e,
                    });
                }
            }
        }
    }

    let ignored_note = ignored_note(ignored);
    let ran = runnable_count - ignored;

    if failed > 0 {
        output::error(&format!(
            "{}: {failed} of {} process-isolated failed{ignored_note}",
            sweep.label,
            count_tests(ran)
        ));
        return Ok(false);
    }
    output::detail(&format!(
        "{}: {} process-isolated passed{ignored_note}{}",
        sweep.label,
        count_tests(ran),
        skip_note(pkg_skipped)
    ));
    note_tests(ran, ignored, 0);
    note_pkg_skipped(pkg_skipped);
    Ok(true)
}

/// Record an isolated case's timeout and the stop it causes. Two records,
/// because they are two facts: the deadline is charged to that one execution,
/// its test in its binary, so a same-named test in another binary of the
/// lane is not touched; and every case the lane will now never run is
/// stopped by a sibling's timeout, which is not its own.
fn record_isolated_timeout(tap: &LaneTap, case: &IsolatedCase) {
    for rec in isolated_timeout_records(tap.lane(), case) {
        tap.record(rec);
    }
}

/// The records [`record_isolated_timeout`] makes, for lane `lane`.
fn isolated_timeout_records(lane: usize, case: &IsolatedCase) -> [JournalRecord; 2] {
    [
        JournalRecord::Terminated(Termination {
            scope: TerminationScope::Lane,
            lane: Some(lane),
            stream: None,
            cause: TerminationCause::PerTestDeadline,
            test: Some(case.name.clone()),
            charged: Some(ChargedTo { resolution: case.resolution.clone(), unit: case.unit.clone() }),
        }),
        JournalRecord::Terminated(Termination {
            scope: TerminationScope::Lane,
            lane: Some(lane),
            stream: None,
            cause: TerminationCause::SiblingTimeout,
            test: None,
            charged: None,
        }),
    ]
}

/// `", N ignored"` when any test was skipped as `#[ignore]`d, else empty.
fn ignored_note(n: usize) -> String {
    if n > 0 {
        format!(", {n} ignored")
    } else {
        String::new()
    }
}

/// `1 test` / `12 tests`.
fn count_tests(n: usize) -> String {
    if n == 1 {
        "1 test".into()
    } else {
        format!("{n} tests")
    }
}

enum IsolatedOutcome {
    /// Ran and passed; carries the test's own wall time when libtest
    /// reported one.
    Passed(Option<std::time::Duration>),
    /// Failed; already reported with its command. The lane keeps going, because
    /// a per-test failure list is the point of running isolated.
    Failed,
    /// Blew its time budget. Distinct from `Failed` because it ends the lane:
    /// see the loop in [`run_isolated_sweep`].
    TimedOut,
}

/// The runnable binary/test pairs and the package-qualified-skipped count.
fn plan_runnable(plan: &IsolatedPlan, label: &str) -> Option<(Vec<IsolatedCase>, usize)> {
    let runnable = plan.cases.clone();
    let pkg_skipped = plan.pkg_skipped;

    if runnable.is_empty() {
        output::error(&format!(
            "sweep '{label}' enumerated zero runnable tests under its filters \
             and skips - a process-isolated lane that runs nothing must not \
             read as green"
        ));
        return None;
    }
    output::detail(&format!(
        "{label}: {}, one process each{}",
        count_tests(runnable.len()),
        skip_note(pkg_skipped)
    ));
    Some((runnable, pkg_skipped))
}

/// `", N pkg-skipped"` when a package-qualified skip excluded names.
fn skip_note(n: usize) -> String {
    if n > 0 {
        format!(", {n} pkg-skipped")
    } else {
        String::new()
    }
}

/// One test in one prebuilt harness.
#[derive(Clone)]
struct IsolatedCase {
    binary: TestBinary,
    unit: BinaryUnit,
    resolution: Option<String>,
    name: String,
    ignored: bool,
}

/// What a process-isolated sweep will run, from the prepared plan.
struct IsolatedPlan {
    cases: Vec<IsolatedCase>,
    pkg_skipped: usize,
    include_ignored: bool,
}

/// One prebuilt binary with one exact test, under the standard per-test cap.
fn isolated_args(name: &str, include_ignored: bool) -> Vec<&str> {
    let mut args = vec![
        "--exact",
        name,
        "--test-threads=1",
        "-Z",
        "unstable-options",
        "--format",
        "json",
    ];
    if include_ignored {
        args.push("--include-ignored");
    }
    args
}

/// One process for one test. Its stream is attributed to exactly that test,
/// which is what lets a kill or a crash be charged to it.
#[allow(clippy::too_many_arguments)]
fn run_one_isolated_test(
    project_root: &Path,
    state_root: &Path,
    case: &IsolatedCase,
    runtime: &DirectRuntime,
    include_ignored: bool,
    env_refs: &[(&str, &str)],
    tap: &LaneTap,
    commands: bool,
    sweep_label: &str,
) -> Result<IsolatedOutcome, DevError> {
    let name = &case.name;
    let args = isolated_args(name, include_ignored);

    let command = format!("{} {}", case.binary.executable, args.join(" "));
    cargo_line(commands, &command);
    let (cwd, env) = runtime.envelope(&case.binary, env_refs)?;
    let cwd = if cwd.as_os_str() == "." {
        project_root.to_path_buf()
    } else {
        cwd
    };
    let env_pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let origin = StreamOrigin::Binary {
        resolution: case.resolution.clone(),
        unit: case.unit.clone(),
        one_test: Some(name.clone()),
    };
    let sink_origin = origin.clone();
    let sink = tap.sink(move |_| sink_origin.clone(), false);
    let run = test_runner::run_libtest_parallel(
        &case.binary.executable,
        &args,
        &cwd,
        state_root,
        &env_pairs,
        test_runner::TEST_TIMEOUT,
        test_runner::TEST_TIMEOUT,
        None,
        Some(&sink),
        |_| {},
        |_| {},
        |_| {},
    );
    if let Err(DevError::Spawn { error, .. }) = &run {
        tap.record(JournalRecord::SpawnFailed { lane: tap.lane(), origin, detail: error.to_string() });
    }
    let run = run?;

    // A wall kill, or a hung test: this process has exactly one selected test,
    // so the kill is attributed from the selection even if its output hid a
    // JSON event.
    if run.timed_out || matches!(run.outcome, LibtestOutcome::HungTest(_)) {
        output::error(&format!(
            "test '{name}' in {}/{} exceeded its time budget in sweep '{sweep_label}' - stopping",
            case.binary.package, case.binary.target
        ));
        output::error(&format!("failing command: {command}"));
        return Ok(IsolatedOutcome::TimedOut);
    }
    let stdout = String::from_utf8_lossy(&run.captured.stdout);

    if !run.captured.status.success() {
        output::error(&format!("FAIL {name}"));
        output::error(&format!("failing command: {command}"));
        let stderr = String::from_utf8_lossy(&run.captured.stderr);
        output::error(&cargo_filter::filter_test(&stdout, &stderr));
        return Ok(IsolatedOutcome::Failed);
    }

    // The caller already routed `#[ignore]`d names away from execution, so
    // an invocation that ran zero tests means the name stopped matching
    // between enumeration and execution - an anomaly, not a skip.
    let stdout_lines: Vec<&str> = stdout.lines().collect();

    let parsed = cargo_filter::parse_test_output(&stdout_lines);
    if zero_test_run(&parsed) {
        output::error(&format!(
            "FAIL {name}: invocation ran zero tests (name no longer matches?)"
        ));
        output::error(&format!("failing command: {command}"));
        return Ok(IsolatedOutcome::Failed);
    }

    // This lane selects one test by name, so its terminal event is the whole
    // point of the invocation: a stream that stopped before reporting it has not
    // shown that the test passed, whatever the exit status says. `zero_test_run`
    // returns false for an incomplete stream by design, which left this path
    // reaching `Passed` on evidence that never arrived.
    if let cargo_filter::Completeness::Incomplete { reason } = &parsed.completeness {
        output::error(&format!(
            "FAIL {name}: the test stream did not finish reporting: {reason}. The process exited \
             successfully, but nothing reported this test's result, so there is no pass to record."
        ));
        output::error(&format!("failing command: {command}"));
        return Ok(IsolatedOutcome::Failed);
    }

    // The roll-call: one line per test, into the run log. It used to print,
    // because this is the slowest-per-test lane in the run and so the one
    // where progress is worth watching - but a passing run's reader pays for
    // every line, and the live view is the status line's job: it names the
    // test in flight on a terminal, which is what the roll-call was for.
    let elapsed = run.completed.first().map(|(_, e)| *e);
    match elapsed {
        Some(e) => output::detail(&format!("PASS {name} ({:.1}s)", e.as_secs_f64())),
        None => output::detail(&format!("PASS {name}")),
    }
    Ok(IsolatedOutcome::Passed(elapsed))
}

/// Parse libtest `--list` output: one `module::name: test` line per test
/// (interleaved with cargo status lines and per-binary summaries, which
/// don't match the suffix). Sorted + deduped within this binary only.
///
/// `None` means the output is **not a libtest listing at all**, which is a
/// different fact from "a libtest listing containing no tests" and must not be
/// confused with it. Every libtest listing ends with a `N tests, M benchmarks`
/// tally, even when both are zero; a binary built with `harness = false`, a
/// custom harness, or anything else that ignores `--list` and exits 0 produces
/// no such line. Returning an empty vec for those silently shrank the universe
/// the coverage audit certifies - the audit would pass while attesting to
/// nothing, which is worse than failing, because a green audit is taken as
/// evidence.
///
/// A tally is also checked against what was listed: the tallies' tests and
/// benchmarks must equal the `: test` and `: benchmark` entries counted. A
/// listing cut short keeps a valid-looking tally only if the tally itself
/// survived, and then it disagrees with the entries above it - which, unchecked,
/// certified a partial universe.
fn parse_list_output(stdout: &str) -> Option<Vec<String>> {
    let mut tallied = (0_u64, 0_u64);
    let mut saw_tally = false;
    let mut listed = (0_u64, 0_u64);
    let mut out: Vec<String> = Vec::new();
    for line in stdout.lines().map(str::trim) {
        if let Some((tests, benches)) = list_tally(line) {
            saw_tally = true;
            tallied.0 += tests;
            tallied.1 += benches;
        } else if let Some(name) = line.strip_suffix(": test") {
            listed.0 += 1;
            if !name.is_empty() {
                out.push(name.to_owned());
            }
        } else if line.ends_with(": benchmark") {
            listed.1 += 1;
        }
    }
    if !saw_tally || tallied != listed {
        return None;
    }
    out.sort();
    out.dedup();
    Some(out)
}

/// The `#[bench]` names in a libtest `--list` output. Outside the coverage
/// claim (they are not tests), but a `cargo test` run still executes each once
/// in test mode and reports it like a test - so the plan must know them, or
/// their records would read as tests nobody planned.
fn parse_list_benchmarks(stdout: &str) -> Vec<String> {
    let mut out: Vec<String> = stdout
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_suffix(": benchmark"))
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The tally line libtest closes a `--list` with: `N tests, M benchmarks`,
/// singularised at 1. Its presence is what distinguishes a real (possibly empty)
/// listing from a binary that never understood `--list`. Returns the
/// `(tests, benchmarks)` it states, or `None` for any other line.
fn list_tally(line: &str) -> Option<(u64, u64)> {
    let (tests, benches) = line.split_once(", ")?;
    let counted = |part: &str, noun: &str| {
        let (n, word) = part.split_once(' ')?;
        let n = n.parse::<u64>().ok()?;
        (word == noun || word == format!("{noun}s")).then_some(n)
    };
    Some((counted(tests, "test")?, counted(benches, "benchmark")?))
}

#[cfg(test)]
mod isolate_tests {
    #![allow(clippy::unwrap_used)]

    use super::{
        BinaryUnit, IsolatedCase, IsolatedPlan, TestBinary, isolated_args, list_tally,
        parse_list_output, plan_runnable,
    };
    use std::path::PathBuf;

    fn case(package: &str, executable: &str, ignored: bool) -> IsolatedCase {
        let binary = TestBinary {
            package: package.into(),
            package_id: package.into(),
            target: "suite".into(),
            kind: "test".into(),
            executable: executable.into(),
            manifest_dir: PathBuf::from("/workspace"),
        };
        IsolatedCase {
            unit: BinaryUnit::of(&binary),
            binary,
            resolution: None,
            name: "same_name".into(),
            ignored,
        }
    }

    #[test]
    fn plan_keeps_same_named_tests_in_distinct_harnesses() {
        let plan = IsolatedPlan {
            cases: vec![case("a", "/a/suite", false), case("b", "/b/suite", true)],
            pkg_skipped: 1,
            include_ignored: false,
        };
        let (runnable, skipped) = plan_runnable(&plan, "sweep").expect("runnable plan");
        assert_eq!(runnable.len(), 2);
        assert_eq!(runnable[0].binary.executable, "/a/suite");
        assert_eq!(runnable[1].binary.executable, "/b/suite");
        assert!(!runnable[0].ignored);
        assert!(runnable[1].ignored);
        assert_eq!(skipped, 1);
    }

    /// Binary A's `same_name` timing out is charged to A's execution alone.
    /// The lane-wide record used to carry only the test name, and B's queued
    /// `same_name` - same name, other binary, never started - read as timed
    /// out too; it is a sibling's timeout, unobserved.
    #[test]
    fn an_isolated_timeout_is_charged_to_its_binary_not_to_a_same_named_test() {
        use super::{
            AccountingPlan, Detail, JournalRecord, LaneKind, LaneRecord, ObsEvent, Outcome, PairId,
            StreamOrigin, isolated_timeout_records, reconcile,
        };
        let a = case("a", "/a/suite", false);
        let b = case("b", "/b/suite", false);
        let pair = |c: &IsolatedCase| PairId {
            shape: "s".into(),
            resolution: None,
            unit: c.unit.clone(),
            test: c.name.clone(),
        };
        let plan = AccountingPlan {
            run_id: "t".into(),
            complete: true,
            lanes: vec![LaneRecord {
                prepared: true,
                executions: vec![pair(&a), pair(&b)],
                ..LaneRecord::empty(0, "isolated".into(), LaneKind::Isolated, "s".into())
            }],
            ..AccountingPlan::default()
        };
        let mut records = vec![JournalRecord::Observed {
            lane: 0,
            stream: 1,
            origin: StreamOrigin::Binary { resolution: None, unit: a.unit.clone(), one_test: Some(a.name.clone()) },
            event: ObsEvent::Started { name: a.name.clone() },
        }];
        records.extend(isolated_timeout_records(0, &a));
        let r = reconcile(&plan, &records, true, Vec::new());
        let of = |c: &IsolatedCase| {
            let x = r.accounted.iter().find(|x| x.id.pair.unit == c.unit).expect("planned");
            (x.outcome, x.detail)
        };
        assert_eq!(of(&a), (Outcome::TimedOut, Some(Detail::PerTestDeadline)));
        assert_eq!(of(&b), (Outcome::Unobserved, Some(Detail::SiblingTimeout)));
    }

    #[test]
    fn one_case_selects_exactly_one_test_in_its_binary() {
        assert_eq!(
            isolated_args("module::case", false),
            [
                "--exact",
                "module::case",
                "--test-threads=1",
                "-Z",
                "unstable-options",
                "--format",
                "json"
            ]
        );
        assert_eq!(
            isolated_args("module::case", true).last(),
            Some(&"--include-ignored")
        );
    }

    #[test]
    fn list_output_keeps_test_names_only() {
        // Interleaved cargo status lines, per-binary summaries, and
        // benchmark listings must all fall away; duplicate names inside
        // one listing are counted once.
        let stdout = "\
serial_tests::test_logging_to_file: test
serial_tests::test_module_level_filtering: test

2 tests, 0 benchmarks
logging::macros::tests::test_colored_logging_macros: test
serial_tests::test_logging_to_file: test
some_bench: benchmark
2 tests, 1 benchmark
";
        let names = parse_list_output(stdout).expect("a real libtest listing");
        assert_eq!(
            names,
            vec![
                "logging::macros::tests::test_colored_logging_macros",
                "serial_tests::test_logging_to_file",
                "serial_tests::test_module_level_filtering",
            ]
        );
    }

    /// A real listing that happens to contain nothing: empty, but a listing.
    #[test]
    fn list_output_empty_on_no_matches() {
        assert_eq!(
            parse_list_output("0 tests, 0 benchmarks\n"),
            Some(Vec::new()),
            "an empty libtest listing is still a listing"
        );
        // Benchmarks are counted but are not test names.
        assert_eq!(parse_list_output("b: benchmark\n0 tests, 1 benchmark\n"), Some(Vec::new()));
    }

    /// A tally that disagrees with the entries above it is a listing cut
    /// short, not a smaller one - accepting it certified a partial universe.
    #[test]
    fn a_tally_that_disagrees_with_the_entries_is_not_a_listing() {
        assert_eq!(parse_list_output("a: test\n2 tests, 0 benchmarks\n"), None);
        assert_eq!(parse_list_output("1 test, 1 benchmark\n"), None);
        assert_eq!(
            parse_list_output("a: test\nb: test\n2 tests, 0 benchmarks\n"),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
    }

    /// Output that is not a libtest listing at all must be distinguishable from
    /// one containing no tests. A `harness = false` target, or any custom harness
    /// that ignores `--list` and exits 0, used to contribute an empty set - and
    /// the coverage audit would then certify a universe it never saw.
    #[test]
    fn non_libtest_output_is_not_an_empty_listing() {
        assert_eq!(parse_list_output(""), None, "silence is not a listing");
        assert_eq!(
            parse_list_output("running my own harness\nall good\n"),
            None,
            "a custom harness that ignores --list is not an empty listing"
        );
        // Names but no tally: a truncated listing, not a complete empty one.
        assert_eq!(parse_list_output("a::b: test\n"), None);
    }

    #[test]
    fn benchmarks_are_listed_apart_from_tests() {
        let stdout = "a::t: test\nb::parse: benchmark\n1 test, 1 benchmark\n";
        assert_eq!(parse_list_output(stdout), Some(vec!["a::t".to_owned()]));
        assert_eq!(super::parse_list_benchmarks(stdout), vec!["b::parse".to_owned()]);
    }

    #[test]
    fn the_tally_line_is_recognised_in_both_singular_and_plural() {
        assert_eq!(list_tally("0 tests, 0 benchmarks"), Some((0, 0)));
        assert_eq!(list_tally("1 test, 1 benchmark"), Some((1, 1)));
        assert_eq!(list_tally("12 tests, 3 benchmarks"), Some((12, 3)));
        assert_eq!(list_tally("2 tests"), None);
        assert_eq!(list_tally("some tests, some benchmarks"), None);
        assert_eq!(list_tally("a::b: test"), None);
    }
}
