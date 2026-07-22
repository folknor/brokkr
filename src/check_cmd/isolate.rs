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
    let list_refs: Vec<&str> = list_args.iter().map(String::as_str).collect();
    let listed = output::run_captured_with_env("cargo", &list_refs, project_root, &env_refs)?;

    if !listed.status.success() {
        output::error(&format!("failing command: cargo {}", list_args.join(" ")));
        output::error(&String::from_utf8_lossy(&listed.stderr));
        return Ok(false);
    }
    let names = parse_list_output(&String::from_utf8_lossy(&listed.stdout));

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

    // Needed for an #[ignore]d test to actually run under `--exact`;
    // harmless for the rest.
    let include_ignored = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
    let mut failed = 0usize;
    let mut ignored = 0usize;
    for name in &names {
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
            IsolatedOutcome::Ignored => ignored += 1,
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

    let ignored_note = if ignored > 0 {
        format!(", {ignored} ignored")
    } else {
        String::new()
    };

    if failed > 0 {
        output::error(&format!(
            "{}: {failed} of {} process-isolated failed{ignored_note}",
            sweep.label,
            count_tests(names.len())
        ));
        return Ok(false);
    }
    println!(
        "[test]    {}: {} process-isolated passed{ignored_note}",
        sweep.label,
        count_tests(names.len())
    );
    Ok(true)
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
    /// `#[ignore]`d name in a lane that runs without `--include-ignored`:
    /// libtest lists it, `--exact` runs zero tests, exit 0. Visible, not
    /// fatal - the ignored set is lane policy, not a bug.
    Ignored,
    /// Failed, hung, or was killed; already reported with its command.
    Failed,
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

    let stdout_lines: Vec<&str> = stdout.lines().collect();

    if zero_test_run(&cargo_filter::parse_test_output(&stdout_lines)) {
        println!("[test]    SKIP {name} (#[ignore], lane runs without --include-ignored)");
        return Ok(IsolatedOutcome::Ignored);
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
