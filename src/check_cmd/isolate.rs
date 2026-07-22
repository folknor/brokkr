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

    // Enumerate with the lane's real filter argv - the ground truth for
    // what would run, with no reimplementation of libtest's filter
    // semantics. `--tests` always: an isolated sweep never runs doctests
    // (cargo cannot run one doctest per process), other lanes own them.
    let mut list_args: Vec<String> = vec!["test".into()];
    list_args.extend(selection.iter().cloned());
    list_args.push("--tests".into());
    list_args.push("--".into());
    for f in &sweep.name_filters {
        list_args.push(f.clone());
    }
    for a in &sweep.libtest_args {
        list_args.push(a.clone());
    }
    list_args.push("--list".into());

    // The standard sweep announce (shape carries `process-isolated`); with
    // --commands this prints the full enumeration command instead.
    output::run_msg(&sweep_run_line("test", sweep, &list_args, true, commands, package));
    let Some(names) = isolate_list(project_root, &list_args, &env_refs, false, commands)? else {
        return Ok(false);
    };

    if names.is_empty() {
        output::error(&format!(
            "sweep '{}' enumerated zero tests under its filters - a \
             process-isolated lane that runs nothing must not read as green",
            sweep.label
        ));
        return Ok(false);
    }
    println!(
        "[test]    {}: {}, one process each",
        sweep.label,
        count_tests(names.len())
    );

    // libtest's `--list` includes `#[ignore]`d names regardless of
    // `--include-ignored` (verified empirically), so a lane that runs
    // without the flag must subtract them here - `--list --ignored`
    // lists only the ignored set. `--include-ignored` is also what an
    // ignored test needs to actually run under `--exact`.
    let include_ignored = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
    let ignored_names: std::collections::BTreeSet<String> = if include_ignored {
        std::collections::BTreeSet::new()
    } else {
        let Some(list) = isolate_list(project_root, &list_args, &env_refs, true, commands)? else {
            return Ok(false);
        };
        list.into_iter().collect()
    };

    let mut failed = 0usize;
    let mut ignored = 0usize;
    for name in &names {
        if ignored_names.contains(name) {
            ignored += 1;
            println!("[test]    SKIP {name} (#[ignore], lane runs without --include-ignored)");
            continue;
        }
        let outcome = run_one_isolated_test(
            project_root,
            &selection,
            name,
            include_ignored,
            &env_refs,
            raw,
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
    let ran = names.len() - ignored;

    if failed > 0 {
        output::error(&format!(
            "{}: {failed} of {} process-isolated failed{ignored_note}",
            sweep.label,
            count_tests(ran)
        ));
        return Ok(false);
    }
    println!(
        "[test]    {}: {} process-isolated passed{ignored_note}",
        sweep.label,
        count_tests(ran)
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

/// One `cargo test … --list` invocation, parsed. `only_ignored` inserts
/// `--ignored` before `--list`, listing only the `#[ignore]`d subset.
/// `Ok(None)` means the listing failed and was already reported (the
/// sweep should return `Ok(false)`).
fn isolate_list(
    project_root: &Path,
    list_args: &[String],
    env_refs: &[(&str, &str)],
    only_ignored: bool,
    commands: bool,
) -> Result<Option<Vec<String>>, DevError> {
    let mut args = list_args.to_vec();

    if only_ignored {
        args.insert(args.len() - 1, "--ignored".into());
    }

    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &refs, project_root, env_refs)?;

    if !captured.status.success() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        output::error(&String::from_utf8_lossy(&captured.stderr));
        return Ok(None);
    }
    Ok(Some(parse_list_output(&String::from_utf8_lossy(
        &captured.stdout,
    ))))
}

/// One `cargo test <selection> -- --exact <name>` invocation: a fresh
/// process for exactly one test, under the standard per-test watchdog.
fn run_one_isolated_test(
    project_root: &Path,
    selection: &[String],
    name: &str,
    include_ignored: bool,
    env_refs: &[(&str, &str)],
    raw: bool,
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

    let elapsed = run.completed.first().map(|(_, e)| *e);
    match elapsed {
        Some(e) => println!("[test]    PASS {name} ({:.1}s)", e.as_secs_f64()),
        None => println!("[test]    PASS {name}"),
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
