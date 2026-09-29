// `isolation = "process"` execution.
//
// `--test-threads=1` serializes tests inside one process per test binary;
// it does not isolate them. Tests that touch process-global state (a
// global logger) pass under CI's nextest - which runs process-per-test -
// and fail in any shared-process libtest lane, because the first test's
// init is still resident for the ninth. This path provides the guarantee
// the tests actually need: prebuild the sweep's selection, enumerate each
// binary, then run that exact binary once per selected test. Re-entering
// cargo with -p changes the feature graph; re-entering with the whole
// selection runs same-named tests in other binaries under one wall cap.
// DirectRuntime supplies cargo's launch environment for the prebuilt binary.

// Not used here any more, but every check_cmd/*.rs shares one module
// (include!'d into check_cmd.rs), and binary_timings.rs reads this import.
use std::collections::BTreeMap;

/// Enumerate and run one process-isolated sweep. Runs every test even
/// after failures (the per-test failure list is the point of the mode),
/// returns Ok(false) when any failed.
#[allow(clippy::too_many_arguments)]
fn run_isolated_sweep(
    project_root: &Path,
    state_root: &Path,
    sweep: &ResolvedSweep,
    packages: &[&str],
    extra_args: &[String],
    project_env: &[(String, String)],
    allow_args: &[String],
    doctests: bool,
    commands: bool,
    mut timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    if !extra_args.is_empty() {
        return Err(DevError::Config(
            "`brokkr check -- …` extra args are not supported on a sweep with \
             `isolation = \"process\"` - the per-test invocations own their argv."
                .into(),
        ));
    }

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

    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    announce_sweep(
        &format!("test {}: {}", sweep.label, describe_sweep(sweep, true, packages)),
        None,
        commands,
    );
    let Some((plan, runtime)) =
        enumerate_isolated(project_root, sweep, packages, allow_args, &env_refs, commands)?
    else {
        return Ok(false);
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
            &runtime,
            plan.include_ignored,
            &env_refs,
            commands,
        )?;
        match outcome {
            // A blown budget stops the lane. Collapsing it into `Failed` made it
            // indistinguishable from an assertion failure, so the loop counted it
            // and ran the next selected test - and the next, and the next -
            // returning an ordinary failed-sweep result at the end. Whatever
            // wedged the killed test is still there for its successors to
            // inherit, and the contract says stop.
            IsolatedOutcome::TimedOut => {
                return Err(DevError::Verify(format!(
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
    name: String,
    ignored: bool,
}

/// What a process-isolated sweep will run, from per-binary enumeration.
struct IsolatedPlan {
    cases: Vec<IsolatedCase>,
    pkg_skipped: usize,
    include_ignored: bool,
}

/// Enumerate the sweep per test binary (attribution comes from the
/// `--no-run` artifact stream; listing runs the binaries directly, which
/// is env-safe because no test code executes) and apply the
/// package-qualified skips. `Ok(None)` = failure already reported.
fn enumerate_isolated(
    project_root: &Path,
    sweep: &ResolvedSweep,
    packages: &[&str],
    allow_args: &[String],
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<Option<(IsolatedPlan, DirectRuntime)>, DevError> {
    // Direct execution would bypass a configured runner; refuse before building.
    refuse_configured_runner(project_root)?;
    let cli_scope: Vec<String> = packages.iter().map(|p| (*p).to_owned()).collect();
    let mut all = Vec::new();
    let mut runtime_index = BuildRuntimeIndex::default();
    for resolution in sweep.resolutions(&cli_scope) {
        let mut selection = allow_args.to_vec();
        match resolution {
            Some(pkg) => {
                selection.extend(sweep_profile_args(sweep));
                selection.extend(sweep.unification_args());
                selection.extend(["-p".to_owned(), pkg]);
                selection.extend(sweep.cargo_feature_args.iter().cloned());
            }
            None => selection.extend(sweep_selection_args(sweep, packages)),
        }
        let Some((binaries, index)) =
            test_binaries_with_runtime(project_root, &selection, env_refs, commands)?
        else {
            return Ok(None);
        };
        all.extend(binaries);
        runtime_index.merge(index);
    }
    let runtime = DirectRuntime::load(project_root, env_refs, runtime_index)?;
    let binaries = filter_binaries(&all, &sweep.cargo_test_filters);
    let libdir = toolchain_libdir(project_root, env_refs)?;
    let include_ignored = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
    let mut filter_args: Vec<&str> = sweep.name_filters.iter().map(String::as_str).collect();
    filter_args.extend(sweep.libtest_args.iter().map(String::as_str));

    let mut cases = Vec::new();
    let mut pkg_skipped = 0;
    for b in binaries {
        let Some(listed) = binary_list(b, project_root, &filter_args, env_refs, &libdir)? else {
            return Ok(None);
        };
        let b_ignored: BTreeSet<String> = if include_ignored {
            BTreeSet::new()
        } else {
            let mut ignored_args = filter_args.clone();
            ignored_args.push("--ignored");
            let Some(l) = binary_list(b, project_root, &ignored_args, env_refs, &libdir)? else {
                return Ok(None);
            };
            l.into_iter().collect()
        };
        for t in listed {
            if sweep.qualified_skips.iter().any(|q| q.matches(&b.package, &t)) {
                pkg_skipped += 1;
                continue;
            }
            cases.push(IsolatedCase {
                binary: b.clone(),
                ignored: b_ignored.contains(&t),
                name: t,
            });
        }
    }
    Ok(Some((
        IsolatedPlan {
            cases,
            pkg_skipped,
            include_ignored,
        },
        runtime,
    )))
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

#[allow(clippy::too_many_arguments)]
fn run_one_isolated_test(
    project_root: &Path,
    state_root: &Path,
    case: &IsolatedCase,
    runtime: &DirectRuntime,
    include_ignored: bool,
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<IsolatedOutcome, DevError> {
    let name = &case.name;
    let args = isolated_args(name, include_ignored);

    let command = format!("{} {}", case.binary.executable, args.join(" "));
    cargo_line(commands, &command);
    let (cwd, env) = runtime.envelope(&case.binary, env_refs);
    let cwd = if cwd.as_os_str() == "." {
        project_root.to_path_buf()
    } else {
        cwd
    };
    let env_pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let run = test_runner::run_libtest_parallel(
        &case.binary.executable,
        &args,
        &cwd,
        state_root,
        &env_pairs,
        test_runner::TEST_TIMEOUT,
        test_runner::TEST_TIMEOUT,
        None,
        |_| {},
        |_| {},
        |_| {},
    )?;

    if run.timed_out {
        output::error(&format!("test '{name}' exceeded its time budget"));
        output::error(&format!("failing command: {command}"));
        return Ok(IsolatedOutcome::TimedOut);
    }
    if let LibtestOutcome::HungTest(_) = run.outcome {
        // This process has exactly one selected test. Attribute the watchdog
        // kill from the selection, even if its output hid a JSON event.
        output::error(&format!("test '{name}' exceeded its time budget"));
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
fn parse_list_output(stdout: &str) -> Option<Vec<String>> {
    if !stdout.lines().any(|l| is_list_tally(l.trim())) {
        return None;
    }
    let mut out: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let name = line.strip_suffix(": test")?;
            (!name.is_empty()).then(|| name.to_owned())
        })
        .collect();
    out.sort();
    out.dedup();
    Some(out)
}

/// The tally line libtest closes a `--list` with: `N tests, M benchmarks`,
/// singularised at 1. Its presence is what distinguishes a real (possibly empty)
/// listing from a binary that never understood `--list`.
fn is_list_tally(line: &str) -> bool {
    let Some((tests, benches)) = line.split_once(", ") else {
        return false;
    };
    let counted = |part: &str, noun: &str| {
        let Some((n, word)) = part.split_once(' ') else {
            return false;
        };
        n.parse::<u64>().is_ok() && (word == noun || word == format!("{noun}s"))
    };
    counted(tests, "test") && counted(benches, "benchmark")
}

#[cfg(test)]
mod isolate_tests {
    #![allow(clippy::unwrap_used)]

    use super::{
        IsolatedCase, IsolatedPlan, TestBinary, is_list_tally, isolated_args, parse_list_output,
        plan_runnable,
    };
    use std::path::PathBuf;

    fn case(package: &str, executable: &str, ignored: bool) -> IsolatedCase {
        IsolatedCase {
            binary: TestBinary {
                package: package.into(),
                package_id: package.into(),
                target: "suite".into(),
                kind: "test".into(),
                executable: executable.into(),
                manifest_dir: PathBuf::from("/workspace"),
            },
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
1 test, 1 benchmark
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
        assert_eq!(parse_list_output("1 test, 1 benchmark\n"), Some(Vec::new()));
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
    fn the_tally_line_is_recognised_in_both_singular_and_plural() {
        assert!(is_list_tally("0 tests, 0 benchmarks"));
        assert!(is_list_tally("1 test, 1 benchmark"));
        assert!(is_list_tally("12 tests, 3 benchmarks"));
        assert!(!is_list_tally("2 tests"));
        assert!(!is_list_tally("some tests, some benchmarks"));
        assert!(!is_list_tally("a::b: test"));
    }
}
