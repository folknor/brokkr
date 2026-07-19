/// Project + profile-aware env vars set on every cargo test/build
/// subprocess. Owned `String`s (B7: avoids borrow-from-local traps the
/// previous `Vec<(&str, &str)>` shape allowed) and absolute paths
/// (B8: `CARGO_TARGET_TMPDIR` was relative `target/tmp`, fragile if
/// the cargo subprocess ever ran with a different cwd).
///
/// `target_dir` is cargo's resolved `target_directory` (from
/// `cargo metadata --no-deps`) - workspaces can place it outside the
/// project root, so the caller passes it in rather than us assuming
/// `<project_root>/target`. `profile_dir` is the profile name as it
/// appears under that target: `"debug"` for `brokkr check` (which
/// always tests in the dev profile) and `brokkr test --debug`,
/// `"release"` for the default `brokkr test` invocation.
pub(crate) fn build_test_env(
    project: Option<Project>,
    target_dir: &Path,
    profile_dir: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if matches!(project, Some(Project::Nidhogg)) {
        let tmp = target_dir.join("tmp");
        out.push((
            "CARGO_TARGET_TMPDIR".into(),
            tmp.to_string_lossy().into_owned(),
        ));
    }
    let bin_dir = target_dir.join(profile_dir);
    out.push((
        "BROKKR_TEST_BIN_DIR".into(),
        bin_dir.to_string_lossy().into_owned(),
    ));
    out
}

/// Build one binary package with the sweep's feature flags. Errors
/// surface compile failures the same way the test phase does: filter
/// the stderr through `cargo_filter::filter_clippy` (or pass it
/// through raw). JSON mode emits a `parse_error` synthetic Diagnostic.
fn run_sweep_pre_build(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: &str,
    project_env: &[(String, String)],
    raw: bool,
    json: bool,
) -> Result<(), DevError> {
    let mut args: Vec<String> = vec!["build".into()];
    for f in &sweep.cargo_feature_args {
        args.push(f.clone());
    }
    args.push("--package".into());
    args.push(package.into());

    if !json {
        output::run_msg(&format!(
            "cargo {} (sweep build: {})",
            args.join(" "),
            sweep.label
        ));
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, &env_refs)?;

    if captured.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&captured.stderr);
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if json {
        cargo_json::emit_parse_error("test-build", &stdout, &stderr);
    } else if raw {
        if !stderr.is_empty() {
            output::error(&stderr);
        }
    } else {
        output::error(&cargo_filter::filter_clippy(&stderr));
    }
    Err(DevError::Build(format!(
        "build failed for package '{package}' in sweep '{}'",
        sweep.label
    )))
}

/// Run one cargo test invocation for the given sweep. Returns
/// `Ok(true)` on pass, `Ok(false)` on test failure (already reported),
/// `Err(...)` on subprocess spawn failure. `multi` controls whether
/// the `cargo ... (sweep: <label>)` log line carries the suffix - in
/// single-sweep mode (legacy `--all-features` path or one [[check]]
/// entry) the label noise is unhelpful.
#[allow(clippy::too_many_lines, clippy::too_many_arguments, clippy::cognitive_complexity)]
fn run_one_test_sweep(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: Option<&str>,
    extra_args: &[String],
    project_env: &[(String, String)],
    raw: bool,
    json: bool,
    multi: bool,
    timings: Option<&mut Vec<TestTiming>>,
) -> Result<bool, DevError> {
    let (cargo_extra, libtest_extra) = split_extra_args(extra_args);

    let mut args: Vec<String> = vec!["test".into()];
    // Scope to the sweep's packages (`-p <pkg>`) so `--features` is valid in a
    // virtual workspace, mirroring the clippy phase.
    for pkg in &sweep.packages {
        args.push("-p".into());
        args.push(pkg.clone());
    }
    // Or, exclude packages from the whole workspace (test phase only). Parse
    // rejects setting both `packages` and `test_exclude_packages`, so these
    // two loops never both emit. `--exclude` requires `--workspace`.
    if !sweep.test_exclude_packages.is_empty() {
        args.push("--workspace".into());
        for pkg in &sweep.test_exclude_packages {
            args.push("--exclude".into());
            args.push(pkg.clone());
        }
    }
    for f in &sweep.cargo_feature_args {
        args.push(f.clone());
    }
    if let Some(pkg) = package {
        args.push("--package".into());
        args.push(pkg.into());
    }
    for f in &sweep.cargo_test_filters {
        args.push(f.clone());
    }
    for c in cargo_extra {
        args.push(c.clone());
    }
    if json {
        args.push("--message-format=json".into());
    }

    // Thread policy. A serial sweep (`test_threads` unset or 1) runs under the
    // per-test hang watchdog, which requires `--test-threads=1`. A sweep whose
    // profile set `test_threads` to 0 (libtest default parallelism) or >=2 runs
    // in parallel - no watchdog, one whole-sweep timeout instead.
    let parallel = matches!(sweep.test_threads, Some(n) if n != 1);

    let mut libtest_args = Vec::new();
    for s in &sweep.libtest_args {
        libtest_args.push(s.clone());
    }
    for n in &sweep.name_filters {
        libtest_args.push(n.clone());
    }
    if parallel {
        // Some(0) leaves the flag off (libtest default); Some(n>=2) sets n.
        if let Some(n) = sweep.test_threads
            && n >= 2
        {
            libtest_args.push(format!("--test-threads={n}"));
        }
    } else if test_runner::effective_test_threads(&libtest_args)?.is_none() {
        libtest_args.push("--test-threads=1".into());
    }
    for e in libtest_extra {
        libtest_args.push(e.clone());
    }
    if !parallel && test_runner::effective_test_threads(&libtest_args)? != Some(1) {
        return Err(DevError::Config(
            "brokkr check watchdog requires --test-threads=1; set `test_threads` in \
             the profile to run this sweep in parallel, or drop the --test-threads \
             override".into(),
        ));
    }

    let needs_separator = !libtest_args.is_empty();
    if needs_separator {
        args.push("--".into());
        for arg in &libtest_args {
            args.push(arg.clone());
        }
    }

    if !json {
        let line = if multi {
            format!("cargo {} (sweep: {})", args.join(" "), sweep.label)
        } else {
            format!("cargo {}", args.join(" "))
        };
        output::run_msg(&line);
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_full = merged_env(&sweep.env, project_env);
    let env_refs: Vec<(&str, &str)> = env_full
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let json_mode = json;

    // Serial => the watchdog runner; parallel => the whole-sweep-timeout
    // runner. Both reduce to (captured, optional hung test, timed_out, per-test
    // timings) so the reporting below is shared.
    let (captured, hung, timed_out, completed) = if parallel {
        let run = test_runner::run_libtest_parallel(
            &arg_refs,
            project_root,
            &env_refs,
            test_runner::PARALLEL_SWEEP_TIMEOUT,
            |_| {},
            |_| {},
            move |elapsed| {
                if !json_mode {
                    println!(
                        "[test]    test binaries built in {:.1}s; running tests (parallel)",
                        elapsed.as_secs_f64()
                    );
                }
            },
        )?;
        (run.captured, None, run.timed_out, Vec::new())
    } else {
        let run = test_runner::streaming_run_libtest(
            &arg_refs,
            project_root,
            &env_refs,
            test_runner::TEST_TIMEOUT,
            |_| {},
            |_| {},
            move |elapsed| {
                if !json_mode {
                    println!(
                        "[test]    test binaries built in {:.1}s; running tests",
                        elapsed.as_secs_f64()
                    );
                }
            },
        )?;
        let hung = match run.outcome {
            LibtestOutcome::HungTest(h) => Some(h),
            LibtestOutcome::Completed => None,
        };
        (run.captured, hung, false, run.completed)
    };

    if let Some(out) = timings {
        for (name, elapsed) in completed {
            out.push(TestTiming {
                sweep: sweep.label.clone(),
                name,
                elapsed,
            });
        }
    }

    let stdout = String::from_utf8_lossy(&captured.stdout);
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let label_for_tag = if multi { Some(sweep.label.as_str()) } else { None };

    if timed_out {
        if json {
            emit_json_test_sweep(label_for_tag, &stdout, &stderr, false);
        } else {
            output::error(&format!(
                "sweep '{}' exceeded the parallel test timeout ({}s) and was killed",
                sweep.label,
                test_runner::PARALLEL_SWEEP_TIMEOUT.as_secs(),
            ));
        }
        return Ok(false);
    }

    if let Some(hung) = hung {
        if json {
            emit_json_test_hung(label_for_tag, &hung);
        } else {
            output::error(&test_runner::format_hung_test(&hung, project_root));
        }
        return Ok(false);
    }

    if json {
        emit_json_test_sweep(label_for_tag, &stdout, &stderr, captured.status.success());
        if !captured.status.success() {
            return Ok(false);
        }
        // JSON consumers can read the TestSummary event, but `brokkr check`
        // still owns the exit code - and a zero-test run must be non-zero.
        let stdout_lines: Vec<&str> = stdout.lines().collect();
        let parsed = cargo_filter::parse_test_output(&stdout_lines);
        return Ok(!zero_test_run(&parsed));
    }

    if !captured.status.success() {
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
        return Ok(false);
    }

    if raw {
        if !stderr.is_empty() {
            print!("{stderr}");
        }
        if !stdout.is_empty() {
            print!("{stdout}");
        }
    } else {
        let filtered = cargo_filter::filter_clippy(&stderr);
        if filtered != "cargo clippy: no issues" {
            let relabeled = filtered.replacen("cargo clippy:", "cargo test:", 1);
            output::warn(&relabeled);
        }
    }

    // Successful exit, but a profile/filter combo could still have
    // collected zero tests - cargo exits 0 with `0 passed; 0 failed`
    // and the user thinks `brokkr check` validated something. Fail
    // loudly when at least one suite ran but every test was filtered
    // out, since that's the silent-wrong-run shape. Suites = 0 (parse
    // failure) is also fatal: we can't tell what cargo did.
    let stdout_lines: Vec<&str> = stdout.lines().collect();
    let parsed = cargo_filter::parse_test_output(&stdout_lines);
    if zero_test_run(&parsed) {
        let label = if multi {
            format!(" (sweep: {})", sweep.label)
        } else {
            String::new()
        };
        output::error(&format!(
            "cargo test: zero tests ran{label} ({} suite(s), {} filtered out) - \
             a profile/filter combo collected no work; treat as a wrong-run.",
            parsed.suites, parsed.filtered_out,
        ));
        return Ok(false);
    }
    Ok(true)
}

/// True if a line looks like cargo's `--message-format=json` output.
///
/// Cargo's JSON events always begin with `{"reason":"..."`. A test
/// that writes a brace-leading line (`println!("{...}")`, panics with
/// a message starting in `{`) used to be misrouted into the JSON
/// parser by a bare `line.starts_with('{')` check, where it failed
/// to deserialize and was silently dropped.
fn is_cargo_json_line(line: &str) -> bool {
    line.starts_with("{\"reason\":")
}

/// Status string for a `TestSummary` JSON event.
///
/// Classifies as `"failed"` when there were real test failures *or*
/// when `zero_test_run` is true (every test filtered out, parse
/// failure). The latter used to emit `"ok"` while the brokkr process
/// itself exited non-zero, leaving JSON consumers unable to trust the
/// status field - reading the counts (`passed+failed+ignored == 0`
/// with `filtered_out > 0`) still distinguishes a zero-run from a
/// genuine test failure.
fn json_test_status(parsed: &cargo_filter::ParsedTestResults) -> &'static str {
    if parsed.failed > 0 || zero_test_run(parsed) {
        "failed"
    } else {
        "ok"
    }
}

/// True when a successful `cargo test` run actually validated nothing.
///
/// Two distinct shapes:
/// - `suites == 0`: parser found no `test result:` line at all (cargo
///   succeeded but emitted unexpected output, or all suites were
///   filtered out by `--test cli_x` matching nothing). Treat as fatal:
///   we can't tell what ran.
/// - `passed + failed + ignored == 0` while at least one suite ran and
///   `filtered_out > 0`: every test in the matched suites was excluded
///   by the libtest filter (`--skip` / positional name). The user
///   thinks they tested something; they didn't.
///
/// A suite that legitimately defines zero tests (`#[cfg(test)] mod`
/// with no `#[test]`s) prints `running 0 tests` + `0 filtered out` and
/// is *not* flagged - that's a real, if empty, run.
fn zero_test_run(p: &cargo_filter::ParsedTestResults) -> bool {
    if p.suites == 0 {
        return true;
    }
    let total = p.passed + p.failed + p.ignored;
    total == 0 && p.filtered_out > 0
}

/// JSON path for one test invocation. `sweep_label` is `Some(name)`
/// in multi-sweep runs so downstream consumers can split per-sweep
/// counts; `None` collapses to the legacy single-sweep shape.
fn emit_json_test_sweep(sweep_label: Option<&str>, stdout: &str, stderr: &str, success: bool) {
    let mut json_lines: Vec<&str> = Vec::new();
    let mut test_lines: Vec<&str> = Vec::new();
    for line in stdout.lines() {
        // B6: a test that does `println!("{...}")` (or panics with a
        // brace-prefixed message) produces a line that starts with `{`
        // but isn't cargo JSON. Match the cargo-emitted shape exactly
        // (`{"reason":"..."`) so test output stays in `test_lines`.
        if is_cargo_json_line(line) {
            json_lines.push(line);
        } else {
            test_lines.push(line);
        }
    }

    let json_text = json_lines.join("\n");
    let diag_events = cargo_json::parse_cargo_diagnostics(&json_text, "test", sweep_label);
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for event in &diag_events {
        if let cargo_json::CheckEvent::Diagnostic(d) = event {
            match d.level.as_str() {
                "error" => errors += 1,
                "warning" => warnings += 1,
                _ => {}
            }
        }
        cargo_json::emit(event);
    }
    if errors > 0 || warnings > 0 {
        let diag_status = if errors > 0 { "failed" } else { "ok" };
        cargo_json::emit(&cargo_json::CheckEvent::DiagnosticSummary(
            cargo_json::DiagnosticSummaryEvent {
                tool: "test",
                sweep: sweep_label.map(str::to_owned),
                status: diag_status,
                errors,
                warnings,
            },
        ));
    }

    let parsed = cargo_filter::parse_test_output(&test_lines);
    for f in &parsed.failures {
        cargo_json::emit(&cargo_json::CheckEvent::TestFailure(
            cargo_json::TestFailureEvent {
                name: f.name.clone(),
                location: f.location.clone(),
                message: f.message.clone(),
            },
        ));
    }

    if parsed.failures.is_empty() && diag_events.is_empty() && !success {
        cargo_json::emit_parse_error("test", stdout, stderr);
    }

    if parsed.suites > 0 {
        let test_status = json_test_status(&parsed);
        cargo_json::emit(&cargo_json::CheckEvent::TestSummary(
            cargo_json::TestSummaryEvent {
                status: test_status,
                sweep: sweep_label.map(str::to_owned),
                passed: parsed.passed,
                failed: parsed.failed,
                ignored: parsed.ignored,
                filtered_out: parsed.filtered_out,
                suites: parsed.suites,
                duration_seconds: parsed.duration.map(|d| (d * 100.0).round() / 100.0),
            },
        ));
    }
}

fn emit_json_test_hung(sweep_label: Option<&str>, hung: &test_runner::HungTest) {
    cargo_json::emit(&cargo_json::CheckEvent::TestHung(
        cargo_json::TestHungEvent {
            sweep: sweep_label.map(str::to_owned),
            name: hung.test.clone(),
            elapsed_seconds: (hung.elapsed.as_secs_f64() * 10.0).round() / 10.0,
            snapshot_dir: hung.snapshot_dir.display().to_string(),
            cargo_pid: hung.cargo_pid,
            test_pids: hung.test_pids.clone(),
            snapshot_pid: hung.snapshot_pid,
            wchan: hung.wchan.clone(),
            stack: hung.stack.clone(),
            snapshot_error: hung.snapshot_error.clone(),
        },
    ));
}

/// Combine the sweep's profile-defined env with the project's
/// always-set vars (e.g. nidhogg's `CARGO_TARGET_TMPDIR`). Sweep
/// values come first; project values append (so a sweep can shadow a
/// project default if it really needs to).
pub(crate) fn merged_env(
    sweep_env: &std::collections::BTreeMap<String, String>,
    project_env: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        sweep_env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in project_env {
        if !out.iter().any(|(ek, _)| ek == k) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::useless_vec
    )]
    use super::*;

    #[test]
    fn decide_active_sweeps_legacy_default_when_nothing_configured() {
        let sweeps = decide_active_sweeps(&[], None, None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        // The legacy-fallback label tracks the cargo flag so callers
        // that need to recognize this branch don't have to compare
        // feature-arg vectors. Don't change without updating
        // `brokkr test` (it relied on this distinction pre-fix).
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--all-features"]);
        assert!(sweeps[0].build_packages.is_empty());
        assert!(sweeps[0].libtest_args.is_empty());
    }

    #[test]
    fn decide_active_sweeps_cli_features_create_ad_hoc() {
        // --features commands → single ad-hoc sweep, ignores `[[check]]`
        // and any profile entirely.
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["pbfhogg-cli".into()],
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(
            &entries,
            None,
            None,
            &["commands".to_owned()],
            false,
        )
        .unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--features", "commands"]);
        // No build_packages on ad-hoc - the user is spot-checking.
        assert!(sweeps[0].build_packages.is_empty());
    }

    #[test]
    fn decide_active_sweeps_no_default_features_alone_is_ad_hoc() {
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps = decide_active_sweeps(&entries, None, None, &[], true).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "default");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--no-default-features"]);
    }

    #[test]
    fn decide_active_sweeps_check_entries_no_profile() {
        let entries = vec![
            CheckEntry {
                name: "all".into(),
                features: vec!["a".into(), "b".into()],
                no_default_features: false,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
            CheckEntry {
                name: "consumer".into(),
                features: vec!["commands".into()],
                no_default_features: true,
                build_packages: vec!["pbfhogg-cli".into()],
                ..Default::default()
            },
        ];
        let sweeps = decide_active_sweeps(&entries, None, None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--features", "a,b"]);
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert!(sweeps[0].libtest_args.is_empty());
        assert_eq!(sweeps[1].label, "consumer");
    }

    #[test]
    fn decide_active_sweeps_default_profile_when_no_explicit() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]
skip = ["tier2::"]
include_ignored = false
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: vec!["pbfhogg-cli".into()],
            ..Default::default()
        }];
        let sweeps =
            decide_active_sweeps(&entries, Some(&test_cfg), None, &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].libtest_args, vec!["--skip", "tier2::"]);
    }

    #[test]
    fn decide_active_sweeps_explicit_profile_overrides_default() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all"]

[profiles.full]
sweeps = ["all"]
include_ignored = true
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps =
            decide_active_sweeps(&entries, Some(&test_cfg), Some("full"), &[], false).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert!(sweeps[0].libtest_args.contains(&"--include-ignored".into()));
    }

    #[test]
    fn decide_active_sweeps_profile_without_test_section_errors() {
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let err = decide_active_sweeps(&entries, None, Some("tier1"), &[], false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--profile tier1"), "got: {err}");
    }

    #[test]
    fn sweep_tag_formats() {
        assert_eq!(sweep_tag(&[], 1), None);
        assert_eq!(sweep_tag(&["consumer".into()], 2), Some("[consumer]".into()));
        // Hit in both of two active sweeps -> `[both]` is honest.
        assert_eq!(
            sweep_tag(&["all-features".into(), "consumer".into()], 2),
            Some("[both]".into())
        );
    }

    #[test]
    fn sweep_tag_avoids_both_when_more_than_two_sweeps_active() {
        // B5: `[both]` with three active sweeps would hide which two
        // actually triggered the hit. Fall through to the explicit
        // joined form so the reader sees the real pair.
        assert_eq!(
            sweep_tag(&["a".into(), "b".into()], 3),
            Some("[a+b]".into())
        );
    }

    #[test]
    fn merge_clippy_dedups_and_combines_sweeps() {
        let stderr_a = "\
warning: x [unused_variables]
 --> src/foo.rs:1:1
  |
warning: y [needless_pass_by_value]
 --> src/bar.rs:2:1
  |
";
        let stderr_b = "\
warning: x [unused_variables]
 --> src/foo.rs:1:1
  |
warning: z [too_many_lines]
 --> src/baz.rs:3:1
  |
";
        let parses = vec![
            ("all-features".to_owned(), cargo_filter::parse_clippy(stderr_a)),
            ("consumer".to_owned(), cargo_filter::parse_clippy(stderr_b)),
        ];
        let merged = merge_clippy(&parses);
        // 3 unique diagnostics: foo (both), bar (a), baz (b).
        assert_eq!(merged.len(), 3);
        let foo = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("foo.rs"))
            .unwrap();
        assert_eq!(foo.sweeps, vec!["all-features", "consumer"]);
        let bar = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("bar.rs"))
            .unwrap();
        assert_eq!(bar.sweeps, vec!["all-features"]);
        let baz = merged
            .iter()
            .find(|m| m.diag.path().unwrap().to_string_lossy().contains("baz.rs"))
            .unwrap();
        assert_eq!(baz.sweeps, vec!["consumer"]);
    }

    fn json_compiler_message(
        level: &str,
        code: Option<&str>,
        message: &str,
        file: &str,
        line: u64,
        col: u64,
    ) -> String {
        let code_field = match code {
            Some(c) => format!(r#""code":{{"code":"{c}"}},"#),
            None => "\"code\":null,".to_string(),
        };
        format!(
            r#"{{"reason":"compiler-message","message":{{{code_field}"level":"{level}","message":"{message}","spans":[{{"file_name":"{file}","line_start":{line},"column_start":{col},"line_end":{line},"column_end":{col},"is_primary":true}}],"children":[],"rendered":"rendered"}}}}"#
        )
    }

    #[test]
    fn json_to_clippy_uses_code_for_every_occurrence() {
        // Regression: in cargo's pretty-printed text, only the first
        // occurrence of each lint per crate carries a `= note: #[warn(rule)]`
        // line, so the old text scraper left subsequent warnings as bare
        // `warning`. With JSON ingestion every diagnostic carries
        // `message.code.code`, so they all keep the rule in the header.
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::collapsible_if"),
            "this `if` statement can be collapsed",
            "src/compose.rs",
            219,
            9,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "warning",
            Some("clippy::collapsible_if"),
            "this `if` statement can be collapsed",
            "src/compose.rs",
            228,
            9,
        ));

        let parsed = parse_clippy_from_json(&input, false, false);
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.diagnostics.len(), 2);
        for d in &parsed.diagnostics {
            assert_eq!(d.header, "warning[clippy::collapsible_if]");
        }
    }

    #[test]
    fn json_to_clippy_uses_primary_label_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/foo.rs","line_start":20,"column_start":5,"line_end":20,"column_end":10,"is_primary":true,"label":"expected `i32`, found `&str`"}],"children":[],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        let d = &parsed.diagnostics[0];
        assert_eq!(d.header, "error[E0308]");
        assert_eq!(
            d.format_one(),
            "error[E0308] src/foo.rs:20:5 mismatched types - expected `i32`, found `&str`"
        );
    }

    #[test]
    fn json_to_clippy_falls_back_to_child_note_for_detail() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/lib.rs","line_start":42,"column_start":12,"line_end":42,"column_end":15,"is_primary":true,"label":"arguments to this function are incorrect"}],"children":[{"level":"note","message":"expected reference `&Vec<u8>`\n   found reference `&Vec<i32>`","spans":[]}],"rendered":"rendered"}}"#;
        let parsed = parse_clippy_from_json(input, false, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        let d = &parsed.diagnostics[0];
        assert!(
            d.format_one()
                .contains("- expected reference `&Vec<u8>`, found reference `&Vec<i32>`"),
            "got: {}",
            d.format_one()
        );
    }

    #[test]
    fn json_to_clippy_no_code_falls_back_to_bare_level() {
        // Some diagnostics lack a code (e.g. cargo-emitted notes). The
        // header degrades gracefully to bare `warning` / `error`.
        let input = json_compiler_message(
            "warning",
            None,
            "something happened",
            "src/foo.rs",
            10,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false, false);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].header, "warning");
    }

    #[test]
    fn json_to_clippy_orders_errors_before_warnings() {
        let mut input = json_compiler_message(
            "warning",
            Some("clippy::redundant_closure"),
            "redundant closure",
            "src/a.rs",
            1,
            1,
        );
        input.push('\n');
        input.push_str(&json_compiler_message(
            "error",
            Some("E0308"),
            "mismatched types",
            "src/b.rs",
            2,
            2,
        ));
        let parsed = parse_clippy_from_json(&input, false, false);
        assert_eq!(parsed.diagnostics.len(), 2);
        assert!(parsed.diagnostics[0].is_error);
        assert!(!parsed.diagnostics[1].is_error);
    }

    #[test]
    fn gate_promotes_capped_warning_to_error() {
        // Under `--cap-lints=warn` a deny lint arrives at `warning` level; the
        // gate restores it to `error` for both the flag and the header, so
        // brokkr counts and displays it as the failure it is.
        let input = json_compiler_message(
            "warning",
            Some("clippy::manual_string_new"),
            "empty String is being created manually",
            "src/a.rs",
            3,
            5,
        );
        let parsed = parse_clippy_from_json(&input, false, true);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert!(parsed.diagnostics[0].is_error);
        assert_eq!(
            parsed.diagnostics[0].header,
            "error[clippy::manual_string_new]"
        );
    }

    #[test]
    fn json_to_clippy_sets_parse_failed_when_sweep_failed_with_no_events() {
        // cargo crashed before producing any compiler-message events.
        let parsed = parse_clippy_from_json("", true, false);
        assert!(parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    #[test]
    fn json_to_clippy_no_parse_failed_when_sweep_succeeded() {
        // Empty stdout but successful exit (clean compile). Not a parse
        // failure - just nothing to report.
        let parsed = parse_clippy_from_json("", false, false);
        assert!(!parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }

    fn diag(header: &str, location: &str) -> cargo_filter::ClippyDiagnostic {
        cargo_filter::ClippyDiagnostic {
            is_error: header.starts_with("error"),
            header: header.to_string(),
            location: Some(location.to_string()),
            message: "msg".to_string(),
            detail: None,
        }
    }

    #[test]
    fn clippy_sort_key_orders_errors_before_warnings() {
        let warn = diag("warning[clippy::aaaa]", "src/a.rs:1:1");
        let err = diag("error[E0308]", "src/z.rs:99:99");
        assert!(clippy_sort_key(&err) < clippy_sort_key(&warn));
    }

    #[test]
    fn clippy_sort_key_groups_same_lint_together() {
        // Three warnings - two with the same lint code on different files,
        // one with a different code in between alphabetically. After sort,
        // the same-lint pair should be adjacent.
        let mut diags = vec![
            diag("warning[clippy::collapsible_if]", "src/b.rs:1:1"),
            diag("warning[clippy::needless_return]", "src/a.rs:1:1"),
            diag("warning[clippy::collapsible_if]", "src/a.rs:1:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].header, "warning[clippy::collapsible_if]");
        assert_eq!(diags[1].header, "warning[clippy::collapsible_if]");
        assert_eq!(diags[2].header, "warning[clippy::needless_return]");
        // Within the same lint, file order kicks in: a.rs before b.rs.
        assert_eq!(diags[0].location.as_deref(), Some("src/a.rs:1:1"));
        assert_eq!(diags[1].location.as_deref(), Some("src/b.rs:1:1"));
    }

    #[test]
    fn clippy_sort_key_orders_lines_numerically() {
        // Same lint, same file: line 9 before line 100 (lexical sort
        // would put 100 first - check we're parsing the integer).
        let mut diags = vec![
            diag("warning[clippy::xxx]", "src/a.rs:100:1"),
            diag("warning[clippy::xxx]", "src/a.rs:9:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].location.as_deref(), Some("src/a.rs:9:1"));
        assert_eq!(diags[1].location.as_deref(), Some("src/a.rs:100:1"));
    }

    #[test]
    fn clippy_sort_key_pushes_bare_level_to_end() {
        // A bare `warning` (no code) should sort after every coded
        // warning, since there's no useful key to group it with.
        let mut diags = vec![
            diag("warning", "src/a.rs:1:1"),
            diag("warning[clippy::zzz]", "src/b.rs:1:1"),
            diag("warning[clippy::aaa]", "src/c.rs:1:1"),
        ];
        diags.sort_by_cached_key(clippy_sort_key);
        assert_eq!(diags[0].header, "warning[clippy::aaa]");
        assert_eq!(diags[1].header, "warning[clippy::zzz]");
        assert_eq!(diags[2].header, "warning");
    }

    #[test]
    fn parse_location_handles_normal_path_line_col() {
        assert_eq!(
            parse_location(Some("src/foo.rs:10:5")),
            ("src/foo.rs".to_string(), 10, 5)
        );
    }

    #[test]
    fn parse_location_handles_none() {
        assert_eq!(parse_location(None), (String::new(), 0, 0));
    }

    #[test]
    fn extract_lint_code_pulls_bracketed_name() {
        assert_eq!(extract_lint_code("warning[clippy::foo]"), "clippy::foo");
        assert_eq!(extract_lint_code("error[E0308]"), "E0308");
        assert_eq!(extract_lint_code("warning"), "");
    }

    fn s(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn split_extra_args_no_separator_all_cargo_level() {
        // `brokkr check -- --test read_paths` (clap consumed the leading
        // `--`, leaving us with the two tokens). Cargo gets `--test
        // read_paths`; nothing crosses to libtest.
        let extra = s(&["--test", "read_paths"]);
        let (cargo, libtest) = split_extra_args(&extra);
        assert_eq!(cargo, &["--test", "read_paths"]);
        assert!(libtest.is_empty());
    }

    #[test]
    fn split_extra_args_double_dash_routes_to_libtest() {
        // `brokkr check -- -- --ignored`: clap consumed the first `--`,
        // we see `["--", "--ignored"]`. The literal `--` we observe is
        // the *cargo/libtest* boundary - everything after it is libtest.
        let extra = s(&["--", "--ignored"]);
        let (cargo, libtest) = split_extra_args(&extra);
        assert!(cargo.is_empty());
        assert_eq!(libtest, &["--ignored"]);
    }

    #[test]
    fn split_extra_args_mixed_form_routes_each_side() {
        // `brokkr check -- --test cli -- --ignored --nocapture`: cargo
        // gets the test filter, libtest gets the runtime flags.
        let extra = s(&["--test", "cli", "--", "--ignored", "--nocapture"]);
        let (cargo, libtest) = split_extra_args(&extra);
        assert_eq!(cargo, &["--test", "cli"]);
        assert_eq!(libtest, &["--ignored", "--nocapture"]);
    }

    #[test]
    fn split_extra_args_empty_input() {
        let extra: Vec<String> = Vec::new();
        let (cargo, libtest) = split_extra_args(&extra);
        assert!(cargo.is_empty());
        assert!(libtest.is_empty());
    }

    #[test]
    fn is_cargo_json_line_matches_cargo_shape_only() {
        // The real cargo wrapper.
        assert!(is_cargo_json_line(
            r#"{"reason":"compiler-message","package_id":"x"}"#
        ));
        // A test's println!("{}", val) - looks like JSON but isn't.
        assert!(!is_cargo_json_line(r#"{name: "foo"}"#));
        // A panic message starting with `{`.
        assert!(!is_cargo_json_line("{some-debug-output}"));
        // Plain text.
        assert!(!is_cargo_json_line("running 1 test"));
    }

    #[test]
    fn build_test_env_emits_absolute_paths() {
        // B8: CARGO_TARGET_TMPDIR used to be the literal string
        // "target/tmp", which only resolves correctly when the cargo
        // subprocess inherits cwd=project_root. Make sure the helper
        // emits an absolute path joined onto the cargo-resolved
        // target_dir.
        let target = Path::new("/home/u/proj/target");
        let env = build_test_env(Some(Project::Nidhogg), target, "debug");
        let tmp = env
            .iter()
            .find(|(k, _)| k == "CARGO_TARGET_TMPDIR")
            .map(|(_, v)| v.as_str())
            .expect("CARGO_TARGET_TMPDIR set for nidhogg");
        assert_eq!(tmp, "/home/u/proj/target/tmp");
        let bin = env
            .iter()
            .find(|(k, _)| k == "BROKKR_TEST_BIN_DIR")
            .map(|(_, v)| v.as_str())
            .expect("BROKKR_TEST_BIN_DIR set");
        assert_eq!(bin, "/home/u/proj/target/debug");
    }

    #[test]
    fn build_test_env_no_tmpdir_for_non_nidhogg() {
        let env = build_test_env(Some(Project::Pbfhogg), Path::new("/x/target"), "release");
        assert!(env.iter().all(|(k, _)| k != "CARGO_TARGET_TMPDIR"));
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "BROKKR_TEST_BIN_DIR")
                .map(|(_, v)| v.as_str()),
            Some("/x/target/release"),
        );
    }

    fn parsed(passed: usize, failed: usize, ignored: usize, filtered_out: usize, suites: usize) -> cargo_filter::ParsedTestResults {
        cargo_filter::ParsedTestResults {
            failures: Vec::new(),
            passed,
            failed,
            ignored,
            filtered_out,
            suites,
            duration: None,
        }
    }

    #[test]
    fn zero_test_run_flags_zero_suites() {
        // Cargo exited 0 but the parser never saw a `test result:` line.
        // Either parse failure or `--test cli_x` matched no test crate.
        assert!(zero_test_run(&parsed(0, 0, 0, 0, 0)));
    }

    #[test]
    fn zero_test_run_flags_all_filtered_out() {
        // The classic silent-wrong-run shape: `--skip` or the positional
        // name filter excluded every test in the matched suite(s).
        assert!(zero_test_run(&parsed(0, 0, 0, 5, 1)));
    }

    #[test]
    fn zero_test_run_does_not_flag_empty_suite() {
        // A package that legitimately defines no tests reports
        // `0 passed; 0 filtered out` over one suite. Not a wrong run.
        assert!(!zero_test_run(&parsed(0, 0, 0, 0, 1)));
    }

    #[test]
    fn zero_test_run_does_not_flag_normal_pass() {
        assert!(!zero_test_run(&parsed(12, 0, 0, 3, 1)));
    }

    #[test]
    fn json_test_status_failed_on_zero_run() {
        // Reviewer-flagged: an all-filtered-out sweep used to emit
        // `"status":"ok"` here while the brokkr process exited
        // non-zero. Both conditions now agree on `"failed"`.
        assert_eq!(json_test_status(&parsed(0, 0, 0, 5, 1)), "failed");
    }

    #[test]
    fn json_test_status_failed_on_real_failure() {
        assert_eq!(json_test_status(&parsed(2, 1, 0, 0, 1)), "failed");
    }

    #[test]
    fn json_test_status_ok_on_normal_pass() {
        assert_eq!(json_test_status(&parsed(12, 0, 0, 3, 1)), "ok");
    }

    #[test]
    fn json_test_status_ok_on_empty_suite() {
        // Package with no tests defined -> `0 passed; 0 filtered out`.
        // Not a wrong run, status stays `ok`.
        assert_eq!(json_test_status(&parsed(0, 0, 0, 0, 1)), "ok");
    }

    #[test]
    fn zero_test_run_does_not_flag_only_ignored() {
        // `#[ignore]` tests still count - the user opted in.
        assert!(!zero_test_run(&parsed(0, 0, 4, 0, 1)));
    }

    #[test]
    fn split_extra_args_only_dashes_separator() {
        // Just `-- --` after the brokkr `--`. First `--` is consumed by
        // clap, the second is our split point: both sides empty.
        let extra = s(&["--"]);
        let (cargo, libtest) = split_extra_args(&extra);
        assert!(cargo.is_empty());
        assert!(libtest.is_empty());
    }
}
