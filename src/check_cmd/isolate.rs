// `isolation = "process"` execution (TIERED-CHECK.md feature 10).
//
// `--test-threads=1` serializes tests inside one process per test binary;
// it does not isolate them. Tests that touch process-global state (a
// global logger) pass under CI's nextest - which runs process-per-test -
// and fail in any shared-process libtest lane, because the first test's
// init is still resident for the ninth. This path provides the guarantee
// the tests actually need: enumerate the sweep's filtered set with
// `--list`, then run one `cargo test <selection> -- --exact <name>
// --test-threads=1` per test.
//
// Every per-test invocation reuses the sweep's selection argv verbatim.
// That keeps the build fingerprint identical (no rebuild between tests)
// and lets cargo provide the test environment (CARGO_MANIFEST_DIR,
// OUT_DIR, CARGO_PKG_*, …) that running the binaries directly would have
// to replicate by hand - replicating it is nextest's whole job, not
// brokkr's. The cost is one cargo spawn plus one spawn of every selected
// test binary per test: negligible at the family scale this exists for
// (a dozen serial tests), and a lane that wants it for thousands of
// tests wants nextest, not brokkr.

use std::collections::BTreeMap;

/// Enumerate and run one process-isolated sweep. Runs every test even
/// after failures (the per-test failure list is the point of the mode),
/// returns Ok(false) when any failed.
#[allow(clippy::too_many_arguments)]
fn run_isolated_sweep(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: Option<&str>,
    extra_args: &[String],
    project_env: &[(String, String)],
    raw: bool,
    all: bool,
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

    let selection = sweep_selection_args(sweep, package);
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    output::run_msg(&sweep_run_line("test", sweep, &[], true, false, package));
    let Some(plan) = enumerate_isolated(project_root, sweep, &selection, &env_refs, commands)?
    else {
        return Ok(false);
    };

    let Some((runnable, pkg_skipped)) = plan_runnable(&plan, &sweep.label, all) else {
        return Ok(false);
    };

    let runnable_count = runnable.len();
    let mut failed = 0usize;
    let mut ignored = 0usize;
    for name in &runnable {
        if !plan.include_ignored && plan.ignored.contains(name) {
            ignored += 1;
            if all {
                println!("[test]    SKIP {name} (#[ignore], lane runs without --include-ignored)");
            }
            continue;
        }
        let outcome = run_one_isolated_test(
            project_root,
            &selection,
            name,
            plan.include_ignored,
            &env_refs,
            raw,
            all,
            commands,
        )?;
        match outcome {
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
    println!(
        "[test]    {}: {} process-isolated passed{ignored_note}{}",
        sweep.label,
        count_tests(ran),
        skip_note(pkg_skipped)
    );
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
    /// Failed, hung, or was killed; already reported with its command.
    Failed,
}

/// The plan's runnable name list plus the package-qualified-skipped
/// count; `None` after reporting a qualified-skip collision or a
/// zero-runnable enumeration. Under `--raw` the plan is announced before
/// the run; otherwise the sweep's one summary line reports it after.
fn plan_runnable(plan: &IsolatedPlan, label: &str, raw: bool) -> Option<(Vec<String>, usize)> {
    // A name present in both a qualified-skipped and an unskipped package
    // cannot be split by one `cargo test -- --exact` invocation: error
    // rather than half-obey the skip.
    let collisions: Vec<&str> = plan
        .names
        .iter()
        .filter(|(_, f)| f.0 && f.1)
        .map(|(n, _)| n.as_str())
        .collect();

    if !collisions.is_empty() {
        output::error(&format!(
            "package-qualified skip collision ({}): the name exists in both a \
             skipped and an unskipped package, and one `cargo test -- --exact` \
             invocation cannot split them. Rename the test(s) or adjust the skip.",
            collisions.join(", ")
        ));
        return None;
    }

    let runnable: Vec<String> = plan
        .names
        .iter()
        .filter(|(_, f)| f.0)
        .map(|(n, _)| n.clone())
        .collect();
    let pkg_skipped = plan.names.values().filter(|f| f.1 && !f.0).count();

    if runnable.is_empty() {
        output::error(&format!(
            "sweep '{label}' enumerated zero runnable tests under its filters \
             and skips - a process-isolated lane that runs nothing must not \
             read as green"
        ));
        return None;
    }
    if raw {
        println!(
            "[test]    {label}: {}, one process each{}",
            count_tests(runnable.len()),
            skip_note(pkg_skipped)
        );
    }
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

/// What a process-isolated sweep will run, from per-binary enumeration.
struct IsolatedPlan {
    /// name -> (present in an unskipped binary, present in a
    /// package-qualified-skipped binary). Both true = collision.
    names: BTreeMap<String, (bool, bool)>,
    /// Names `#[ignore]`d at the source (from `--list --ignored`; plain
    /// `--list` includes ignored names, verified empirically).
    ignored: BTreeSet<String>,
    include_ignored: bool,
}

/// Enumerate the sweep per test binary (attribution comes from the
/// `--no-run` artifact stream; listing runs the binaries directly, which
/// is env-safe because no test code executes) and apply the
/// package-qualified skips. `Ok(None)` = failure already reported.
fn enumerate_isolated(
    project_root: &Path,
    sweep: &ResolvedSweep,
    selection: &[String],
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<Option<IsolatedPlan>, DevError> {
    let Some(binaries) = test_binaries(project_root, selection, env_refs, commands)? else {
        return Ok(None);
    };
    let binaries = filter_binaries(&binaries, &sweep.cargo_test_filters);
    let libdir = toolchain_libdir(project_root, env_refs)?;
    let include_ignored = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
    let mut filter_args: Vec<&str> = sweep.name_filters.iter().map(String::as_str).collect();
    filter_args.extend(sweep.libtest_args.iter().map(String::as_str));

    let mut names: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    let mut ignored: BTreeSet<String> = BTreeSet::new();
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
                names.entry(t).or_insert((false, false)).1 = true;
                continue;
            }

            if b_ignored.contains(&t) {
                ignored.insert(t.clone());
            }
            names.entry(t).or_insert((false, false)).0 = true;
        }
    }
    Ok(Some(IsolatedPlan {
        names,
        ignored,
        include_ignored,
    }))
}

/// One `cargo test <selection> -- --exact <name>` invocation: a fresh
/// process for exactly one test, under the standard per-test watchdog.
#[allow(clippy::too_many_arguments)]
fn run_one_isolated_test(
    project_root: &Path,
    selection: &[String],
    name: &str,
    include_ignored: bool,
    env_refs: &[(&str, &str)],
    raw: bool,
    all: bool,
    commands: bool,
) -> Result<IsolatedOutcome, DevError> {
    let mut args: Vec<String> = vec!["test".into()];
    args.extend(selection.iter().cloned());
    args.push("--tests".into());
    args.push("--".into());
    args.push("--exact".into());
    args.push(name.into());
    args.push("--test-threads=1".into());

    if include_ignored {
        args.push("--include-ignored".into());
    }

    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let run = test_runner::streaming_run_libtest(
        &arg_refs,
        project_root,
        env_refs,
        test_runner::TEST_TIMEOUT,
        |_| {},
        |_| {},
        |_| {},
    )?;

    if let LibtestOutcome::HungTest(h) = run.outcome {
        output::error(&test_runner::format_hung_test(&h, project_root));
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        return Ok(IsolatedOutcome::Failed);
    }
    let stdout = String::from_utf8_lossy(&run.captured.stdout);

    if !run.captured.status.success() {
        output::error(&format!("FAIL {name}"));
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        let stderr = String::from_utf8_lossy(&run.captured.stderr);
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
        return Ok(IsolatedOutcome::Failed);
    }

    // The caller already routed `#[ignore]`d names away from execution, so
    // an invocation that ran zero tests means the name stopped matching
    // between enumeration and execution - an anomaly, not a skip.
    let stdout_lines: Vec<&str> = stdout.lines().collect();

    if zero_test_run(&cargo_filter::parse_test_output(&stdout_lines)) {
        output::error(&format!(
            "FAIL {name}: invocation ran zero tests (name no longer matches?)"
        ));
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        return Ok(IsolatedOutcome::Failed);
    }

    // A passing test is not news: the sweep's summary line carries the
    // count, and a failure reports itself in full. One line per test turns
    // a gate run into a scroll. `--all` is the way back to the roll-call.
    let elapsed = run.completed.first().map(|(_, e)| *e);
    if all {
        match elapsed {
            Some(e) => println!("[test]    PASS {name} ({:.1}s)", e.as_secs_f64()),
            None => println!("[test]    PASS {name}"),
        }
    }
    Ok(IsolatedOutcome::Passed(elapsed))
}

/// Parse libtest `--list` output: one `module::name: test` line per test
/// (interleaved with cargo status lines and per-binary summaries, which
/// don't match the suffix). Sorted + deduped: the same name in two test
/// binaries is still one `--exact` invocation, and each binary runs it in
/// its own process anyway.
fn parse_list_output(stdout: &str) -> Vec<String> {
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
    out
}

#[cfg(test)]
mod isolate_tests {
    #![allow(clippy::unwrap_used)]

    use super::parse_list_output;

    #[test]
    fn list_output_keeps_test_names_only() {
        // Interleaved cargo status lines, per-binary summaries, and
        // benchmark listings must all fall away; duplicate names across
        // two binaries collapse to one --exact invocation.
        let stdout = "\
serial_tests::test_logging_to_file: test
serial_tests::test_module_level_filtering: test

2 tests, 0 benchmarks
logging::macros::tests::test_colored_logging_macros: test
serial_tests::test_logging_to_file: test
some_bench: benchmark
1 test, 1 benchmark
";
        let names = parse_list_output(stdout);
        assert_eq!(
            names,
            vec![
                "logging::macros::tests::test_colored_logging_macros",
                "serial_tests::test_logging_to_file",
                "serial_tests::test_module_level_filtering",
            ]
        );
    }

    #[test]
    fn list_output_empty_on_no_matches() {
        assert!(parse_list_output("0 tests, 0 benchmarks\n").is_empty());
    }
}
