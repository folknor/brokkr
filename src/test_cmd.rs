//! `brokkr test <NAME>` - named-test cargo runner.
//!
//! Runs the tests matching `<NAME>` with the host/check features and
//! `--include-ignored --nocapture --test-threads=1`. Per sweep: one prebuild,
//! then every prebuilt harness lists itself directly (libtest's JSON
//! discovery, see `focused`), and only the harnesses holding a match run -
//! each one directly, as its own process, under cargo's reconstructed launch
//! envelope (`check_cmd::DirectRuntime`). A failing or crashing harness never
//! hides the ones after it, and one with no match costs a listing, not a cargo
//! invocation. `harness = false` targets are excluded and named. A doc-only
//! sweep is still one `cargo test --doc`, since ordinary discovery cannot see
//! doctests. Defaults to release;
//! `--debug` switches to the dev profile, `--release` forces it back, and
//! when neither is passed the `[test] debug` toml field decides. Streams
//! the test's own
//! stdout/stderr live (filtering out cargo/test-harness framing noise), then
//! prints a `[test]` PASS/FAIL footer per sweep with wall time. The cargo and
//! harness invocations go to the run log; the console shows one only when it
//! failed (beside its diagnostics, or after a failing harness's footer, as the
//! reproduction line). No `sweep:` header lines: a line that belongs to one
//! sweep names it when several run. Under `-N`, the once-only lines print for
//! run 1 only - repeats collapse to their footer line.
//!
//! Feature selection follows the same priority ladder as
//! `brokkr check`'s test phase, with two intentional differences:
//! profile libtest filters (`only` / `skip` / `tests`) are dropped
//! (the user's `<NAME>` is the filter), and CLI `--features` is not
//! accepted. Profile-declared `env` vars *are* propagated, so a
//! profile that gates platform tests behind `BROKKR_TEST_PLATFORM=1`
//! still works under `brokkr test`. A sweep's configured features are
//! projected onto the one package the run selects (and its support builds
//! onto each support package), exactly as `check -p` projects them: a token
//! only another of the sweep's packages routes is dropped, and said so.
//!
//! Every test gets a 20s hard cap; exceeding it fails the run and stops it,
//! between `-N` iterations included. `--timeout <SECS>` (1-280) is the only
//! exception anywhere in brokkr, and it also makes the *attribution* exact:
//! discovery resolves `<NAME>` to one (harness, full test name), and that one
//! binary is run with libtest `--exact`, making the process the unit of one
//! test. Resolving the full name matters - `--exact` on the user's substring
//! would match nothing and silently run zero tests. What it bounds is that
//! process from spawn to exit: its static constructors, the test, teardown.
//! The build and every other harness's listing fall outside it. Because a
//! higher ceiling only makes sense for one isolated test, it is gated: a
//! `<NAME>` matching more than one test in a sweep - two harnesses holding the
//! same name count twice - is a hard error before anything runs.
//!
//! A `<NAME>` matching every discovered test of the package is refused (see
//! `matches_whole_suite`) - a whole-suite run is `brokkr check`'s job.
//!
//! `--sweep <LABEL>` narrows the resolved profile's sweep set to the one
//! matching sweep (e.g. `--sweep all`), instead of running every sweep the
//! profile lists. An unknown label is a hard error listing the available
//! ones. Combined with `--timeout`, this is the usual way to iterate on a
//! single hung test without paying for the other sweeps' rebuilds.

pub(crate) mod focused;

mod display;
mod plan;
mod run;
mod verdict;

pub(crate) use run::run;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{display::*, plan::*, run::*, verdict::*};
    use crate::check_cmd::{self, LaneEntry};
    use crate::config::{CheckEntry, SweepProfile, TestConfig};
    use crate::error::DevError;
    use crate::profile::PlannedSweep;
    use crate::project::Project;
    use crate::test_runner;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// `sweep`'s one request under `brokkr test -p pkg`: the package flags
    /// as the run's override spells them, with the sweep's features.
    fn one_package(sweep: &PlannedSweep, pkg: &str) -> check_cmd::ResolutionRun {
        let sel = selection_for(std::slice::from_ref(sweep), pkg);
        single_run(
            sweep,
            sel.attempt(crate::check_cmd::LaneIdx::new(0)).unwrap(),
        )
        .unwrap()
        .clone()
    }

    /// `brokkr test`'s selection for `pkg`, every sweep resolving to release.
    fn selection_for(sweeps: &[PlannedSweep], pkg: &str) -> check_cmd::PhaseSelection {
        check_cmd::PhaseSelection::for_brokkr_test(
            sweeps,
            pkg,
            check_cmd::PackageSource::Cli,
            &|_| false,
            // Every package unknown: projection is the identity, so these
            // tests see the configured features. Routing has its own tests
            // in `check_cmd`'s selection module.
            &check_cmd::FeatureOracle::empty(),
        )
        .unwrap()
    }

    /// The sweeps a `brokkr test -p pkg` run would build, in order.
    fn attempted<'a>(sweeps: &'a [PlannedSweep], pkg: &str) -> Vec<&'a str> {
        let sel = selection_for(sweeps, pkg);
        sweeps
            .iter()
            .zip(sel.entries().iter())
            .filter(|(_, e)| e.attempt().is_some())
            .map(|(s, _)| s.label.as_str())
            .collect()
    }

    #[test]
    fn stdout_filter_strips_test_framing() {
        assert!(!keep_stdout_line("running 1 test"));
        assert!(!keep_stdout_line("running 12 tests"));
        assert!(!keep_stdout_line("test foo ... ok"));
        assert!(!keep_stdout_line("test my_mod::bar ... FAILED"));
        assert!(!keep_stdout_line("test slow_thing ... ignored"));
        assert!(!keep_stdout_line(
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in 0.01s"
        ));
    }

    #[test]
    fn test_phase_noise_strips_backtrace_hint_and_rerun_line() {
        assert!(is_test_phase_noise(
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"
        ));
        assert!(is_test_phase_noise(
            "error: test failed, to rerun pass `-p brokkr --bin brokkr`"
        ));
        // Real panic content and other notes survive.
        assert!(!is_test_phase_noise(
            "thread 'foo' panicked at src/x.rs:1:1:"
        ));
        assert!(!is_test_phase_noise("note: something else entirely"));
        assert!(!is_test_phase_noise("error: a genuine test eprintln"));
    }

    #[test]
    fn compile_error_detector_ignores_cargo_test_failure_line() {
        // Cargo's test-failure line is a hard-crash/failure signal, not a
        // build failure - must NOT be classified as a compile error.
        assert!(!stderr_indicates_compile_error(
            "error: test failed, to rerun pass '--bin foo'"
        ));
        // A real rustc error still counts.
        assert!(stderr_indicates_compile_error(
            "error: cannot find value `x` in this scope"
        ));
        assert!(stderr_indicates_compile_error(
            "error[E0425]: cannot find value `x` in this scope"
        ));
    }

    #[test]
    fn repeat_state_first_sighting_only_once_per_signature() {
        let state = RepeatState::default();
        assert!(state.first_sighting("src/a.rs:1:1"));
        assert!(!state.first_sighting("src/a.rs:1:1"));
        // A different signature is its own first sighting.
        assert!(state.first_sighting("src/b.rs:2:2"));
    }

    fn failure(target: &str, what: &str, msg: Option<&str>, loc: Option<&str>) -> Failure {
        Failure {
            kind: FailureKind::Test,
            target: target.to_owned(),
            what: what.to_owned(),
            msg: msg.map(str::to_owned),
            loc: loc.map(str::to_owned),
        }
    }

    fn report(outcome: Outcome, failures: Vec<Failure>) -> RunReport {
        RunReport {
            outcome,
            timed_out: false,
            failures,
        }
    }

    #[test]
    fn repeat_summary_counts_and_groups_by_location() {
        let at = |msg: &str, loc: &str| vec![failure("test:t", "flaky", Some(msg), Some(loc))];
        let reports = vec![
            report(Outcome::Pass, vec![]),
            report(Outcome::Fail, at("rolled 0", "src/a.rs:7:9")),
            report(Outcome::Pass, vec![]),
            // Same location, different message - groups with the first,
            // whose message represents the group.
            report(Outcome::Fail, at("rolled 2", "src/a.rs:7:9")),
            report(Outcome::Fail, at("boom", "src/b.rs:1:1")),
        ];
        let lines = format_repeat_summary(&reports);
        assert_eq!(lines[0], "[test]    summary: 5 runs - 2 PASS, 3 FAIL");
        assert_eq!(
            lines[1],
            "[test]      2x [test:t] flaky: rolled 0 @ src/a.rs:7:9"
        );
        assert_eq!(
            lines[2],
            "[test]      1x [test:t] flaky: boom @ src/b.rs:1:1"
        );
        assert_eq!(lines.len(), 3);
    }

    /// Runs group on their WHOLE failure set. Keying on the first failure
    /// folded a run that failed A and B into the group of a run that failed A
    /// alone, and the summary never mentioned B.
    #[test]
    fn repeat_summary_groups_on_the_full_failure_set() {
        let a = failure("lib:p", "a", Some("x"), Some("src/a.rs:1:1"));
        let b = failure("test:cli", "b", Some("y"), Some("tests/cli.rs:2:2"));
        let reports = vec![
            report(Outcome::Fail, vec![a.clone()]),
            report(Outcome::Fail, vec![a.clone(), b.clone()]),
            // Same set, reported in the other order: the same group.
            report(Outcome::Fail, vec![b, a]),
        ];
        let lines = format_repeat_summary(&reports);
        assert_eq!(lines[1], "[test]      1x [lib:p] a: x @ src/a.rs:1:1");
        assert_eq!(lines[2], "[test]      2x [lib:p] a: x @ src/a.rs:1:1");
        assert_eq!(
            lines[3],
            "[test]         + [test:cli] b: y @ tests/cli.rs:2:2"
        );
        assert_eq!(lines.len(), 4);
    }

    /// The same test name failing in two harnesses is two failures.
    #[test]
    fn a_failure_key_carries_its_harness() {
        let lib = failure("lib:p", "tests::works", None, Some("src/lib.rs:9:9"));
        let cli = failure("test:cli", "tests::works", None, Some("src/lib.rs:9:9"));
        assert_ne!(lib.key(), cli.key());
        assert_ne!(
            failure_set_key(std::slice::from_ref(&lib)),
            failure_set_key(&[lib, cli])
        );
    }

    /// A sweep's iteration fails if any harness failed, and is a SKIP only when
    /// no harness matched - a harness matching nothing is the normal case.
    #[test]
    fn merging_harness_reports_follows_the_worst_outcome() {
        let merged = RunReport::merge(vec![
            report(Outcome::NoMatch, vec![]),
            report(Outcome::Pass, vec![]),
            report(Outcome::NoMatch, vec![]),
        ]);
        assert!(merged.outcome == Outcome::Pass);
        let merged = RunReport::merge(vec![
            report(Outcome::Pass, vec![]),
            report(Outcome::Fail, vec![failure("test:a", "x", None, None)]),
            report(Outcome::BuildFailed, vec![]),
            report(Outcome::Fail, vec![failure("test:b", "y", None, None)]),
        ]);
        assert!(merged.outcome == Outcome::Fail);
        assert_eq!(
            merged.failures.len(),
            2,
            "every harness's failures survive the merge"
        );
        let merged = RunReport::merge(vec![report(Outcome::NoMatch, vec![])]);
        assert!(merged.outcome == Outcome::NoMatch);
        assert!(RunReport::merge(Vec::new()).outcome == Outcome::NoMatch);
    }

    /// The exit detail is the harness's, from cargo's own line - cargo's 101
    /// says nothing about how the harness died - and the in-flight test is
    /// offered as a suspect, never filed as the failure itself.
    #[test]
    fn a_harness_failure_names_the_signal_and_only_suspects_a_test() {
        use std::os::unix::process::ExitStatusExt;
        let status =
            crate::output::Ending::from_exit_status(&std::process::ExitStatus::from_raw(101 << 8));
        let stderr = [
            "error: test failed, to rerun pass `-p pkg --test cli`",
            "",
            "Caused by:",
            "  process didn't exit successfully: `/t/deps/cli-1 x` (signal: 6, SIGABRT: process abort signal)",
        ];
        let f = harness_failure(
            "test:cli",
            &status,
            false,
            &stderr,
            &["cli::overflows".to_owned()],
        );
        assert_eq!(
            f.what,
            "test harness failed (signal: 6, SIGABRT: process abort signal)"
        );
        let msg = f.msg.expect("a suspect");
        assert!(
            msg.contains("cli::overflows") && msg.contains("suspect"),
            "{msg}"
        );

        let bare = harness_failure("test:cli", &status, false, &[], &[]);
        assert_eq!(
            bare.what,
            "test harness failed (cargo exited with code 101)"
        );
        assert!(bare.msg.is_none(), "no start seen, no suspect");
    }

    /// A directly run harness's status is its own, so the detail is read
    /// from it - never from cargo lines that a direct run does not have.
    #[test]
    fn a_direct_harness_failure_reads_its_own_status() {
        use std::os::unix::process::ExitStatusExt;
        let aborted = crate::output::Ending::from_exit_status(&std::process::ExitStatus::from_raw(
            libc::SIGABRT,
        ));
        let f = harness_failure("test:cli", &aborted, true, &[], &[]);
        assert_eq!(f.what, "test harness failed (signal: 6, SIGABRT)");
        let exited =
            crate::output::Ending::from_exit_status(&std::process::ExitStatus::from_raw(101 << 8));
        let f = harness_failure("test:cli", &exited, true, &[], &[]);
        assert_eq!(f.what, "test harness failed (exit status: 101)");
    }

    /// Only a single package is ever selected.
    #[test]
    fn a_package_glob_is_refused() {
        assert!(single_package("*".into()).is_err());
        assert!(single_package("pkg-?".into()).is_err());
        assert!(single_package("pkg-[ab]".into()).is_err());
        assert_eq!(single_package("pkg".into()).unwrap(), "pkg");
    }

    /// The prebuild compiles what a named `cargo test -p PKG NAME` compiles -
    /// every test target, not cargo's default set, which would also build
    /// every example.
    #[test]
    fn the_prebuild_selects_the_test_targets() {
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default());
        let run = one_package(&sweep, "pkg");
        let shape = BuildShape {
            sweep: &sweep,
            allow_args: &[],
            run: &run,
            jobs: None,
            debug: true,
        };
        let pre = prebuild_argv(&shape);
        assert_eq!(
            pre,
            [
                "test",
                "--no-run",
                "--message-format=json-render-diagnostics",
                "-p",
                "pkg",
                "--tests"
            ]
        );
    }

    /// A directly run harness gets the filter first and the focused-run flags;
    /// `--exact` only when the caller resolved a full name.
    #[test]
    fn direct_args_carry_the_filter_and_exact_only_on_request() {
        let plain = direct_libtest_args("some::test", false);
        assert_eq!(
            plain,
            [
                "some::test",
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
                "-Z",
                "unstable-options",
                "--format",
                "json"
            ]
        );
        let exact = direct_libtest_args("some::test", true);
        assert_eq!(exact.last().map(String::as_str), Some("--exact"));
    }

    /// The doc-only run is `cargo test --doc NAME` with the same libtest half.
    #[test]
    fn the_doc_only_run_shares_the_libtest_half() {
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            lane_exec: crate::profile::LaneExec::doc_only(),
            ..crate::profile::SweepDecl::default()
        });
        let one = one_package(&sweep, "pkg");
        let shape = BuildShape {
            sweep: &sweep,
            allow_args: &[],
            run: &one,
            jobs: None,
            debug: true,
        };
        let run = test_argv(&shape, "x");
        let sep = run.iter().position(|a| a == "--").expect("separator");
        assert_eq!(&run[..sep], ["test", "-p", "pkg", "--doc", "x"]);
        assert_eq!(&run[sep + 1..], &direct_libtest_args("x", false)[1..]);
    }

    #[test]
    fn repeat_summary_all_pass_has_no_group_lines() {
        let reports = vec![report(Outcome::Pass, vec![]), report(Outcome::Pass, vec![])];
        let lines = format_repeat_summary(&reports);
        assert_eq!(lines, vec!["[test]    summary: 2 runs - 2 PASS, 0 FAIL"]);
    }

    #[test]
    fn repeat_summary_includes_skip_and_build_failed_when_present() {
        let reports = vec![
            report(Outcome::Pass, vec![]),
            report(Outcome::BuildFailed, vec![]),
            report(Outcome::NoMatch, vec![]),
            // Exit-code failure with nothing attributed.
            report(Outcome::Fail, vec![]),
        ];
        let lines = format_repeat_summary(&reports);
        assert_eq!(
            lines[0],
            "[test]    summary: 4 runs - 1 PASS, 1 FAIL, 1 BUILD FAILED, 1 SKIP"
        );
        assert_eq!(lines[1], "[test]      1x unknown failure");
    }

    #[test]
    fn stdout_filter_strips_bare_verdict_lines() {
        // --nocapture puts the verdict on its own line after the test's
        // output; the "test NAME ... FAILED" suffix match never fires.
        assert!(!keep_stdout_line("FAILED"));
        assert!(!keep_stdout_line("ok"));
        assert!(!keep_stdout_line("ignored"));
        assert!(!keep_stdout_line("ok <0.001s>"));
        // Not bare verdicts - real test output survives.
        assert!(keep_stdout_line("FAILED to connect to server"));
        assert!(keep_stdout_line("ok, moving on"));
    }

    fn drive_condenser(lines: &[&str]) -> Vec<String> {
        let mut cond = StdoutCondenser::new();
        lines.iter().flat_map(|l| cond.next(l)).collect()
    }

    #[test]
    fn condenser_collapses_duplicate_failures_headers() {
        // The --nocapture shape: empty output-block section, blank,
        // name-list section. One header survives, glued to the list.
        let out = drive_condenser(&["failures:", "", "failures:", "    my_mod::my_test"]);
        assert_eq!(out, vec!["failures:", "    my_mod::my_test"]);
    }

    #[test]
    fn condenser_drops_dangling_empty_failures_header() {
        // A failures: header with nothing after it (stream ends) is
        // never emitted.
        let out = drive_condenser(&["real output", "failures:", ""]);
        assert_eq!(out, vec!["real output"]);
    }

    #[test]
    fn condenser_collapses_blank_runs_and_leading_blanks() {
        let out = drive_condenser(&["", "", "a", "", "", "b"]);
        assert_eq!(out, vec!["a", "", "b"]);
    }

    /// Libtest's framing is dropped unconditionally now that there is no
    /// flag to keep it: the test's own output is the signal, and the
    /// verdict lines are what the `[test]` footer already says.
    #[test]
    fn condenser_drops_framing_and_bare_verdicts() {
        let out = drive_condenser(&["test result: ok. 1 passed", "FAILED", "real output"]);
        assert_eq!(out, vec!["real output"]);
    }

    #[test]
    fn stdout_filter_keeps_test_output() {
        assert!(keep_stdout_line("hello from test"));
        assert!(keep_stdout_line(""));
        assert!(keep_stdout_line(
            "thread 'foo' panicked at tests/bar.rs:10:5:"
        ));
        assert!(keep_stdout_line("assertion `left == right` failed"));
        assert!(keep_stdout_line("failures:"));
        assert!(keep_stdout_line("---- foo stdout ----"));
        // Messages that start with "test" but aren't framing must survive -
        // a user's println! starting with "test" wouldn't match the exact
        // " ... ok" / "... FAILED" / "... ignored" suffixes.
        assert!(keep_stdout_line("test the things now"));
    }

    #[test]
    fn cargo_running_line_matches_suite_launch_shapes() {
        assert!(is_cargo_running_line(
            "     Running unittests src/bin/bench.rs (/media/folk/Banan/cargo/debug/deps/bench-d5eda320d87aa0a1)"
        ));
        assert!(is_cargo_running_line(
            "     Running tests/montecarlo_threads.rs (/x/target/debug/deps/montecarlo_threads-dcc49cf)"
        ));
    }

    #[test]
    fn cargo_running_line_matches_the_depsless_target_layout() {
        // Cargo no longer puts compiled units under `target/<profile>/deps/`;
        // a `/deps/` substring requirement made this predicate - and with it
        // the whole test-phase stderr filter - dead on such a toolchain.
        assert!(is_cargo_running_line(
            "     Running unittests src/lib.rs (target/debug/build/broadarrow-daemon/87f43d5b0d3f3d44/out/broadarrow_daemon-87f43d5b0d3f3d44)"
        ));
        assert!(is_cargo_running_line(
            "     Running tests/boot_event_stream.rs (target/unify-package/debug/build/broadarrow-daemon/77ac71235dfced6c/out/boot_event_stream-77ac71235dfced6c)"
        ));
    }

    #[test]
    fn cargo_running_line_spares_test_output() {
        // A test's own eprintln! starting with "Running " lacks the
        // `<target>.rs (<path>)` shape and must pass through.
        assert!(!is_cargo_running_line("Running 500 monte carlo paths"));
        assert!(!is_cargo_running_line("Running phase 2 (warmup)"));
        assert!(!is_cargo_running_line("   Compiling brokkr v0.1.0"));
        // Parenthesized, but the pre-paren token is prose, not a source file.
        assert!(!is_cargo_running_line("Running the sweep (a/b/c)"));
        // A source-file-shaped target with no path in the parens.
        assert!(!is_cargo_running_line("Running tests/foo.rs (cached)"));
    }

    #[test]
    fn stderr_filter_strips_compile_progress() {
        let mut in_block = false;
        assert!(!keep_stderr_compile_line(
            "   Compiling brokkr v0.1.0",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line(
            "   Downloading crates ...",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line(
            "    Checking serde v1.0.0",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line(
            "    Finished `release` profile [optimized] target(s) in 45.13s",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line(
            "    Blocking waiting for file lock on build directory",
            &mut in_block
        ));
        assert!(!in_block);
    }

    #[test]
    fn stderr_filter_strips_warning_block() {
        let mut in_block = false;
        assert!(!keep_stderr_compile_line(
            "warning: unused variable: `x`",
            &mut in_block
        ));
        assert!(in_block);
        assert!(!keep_stderr_compile_line(
            "  --> src/lib.rs:10:5",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line("   |", &mut in_block));
        assert!(!keep_stderr_compile_line("10 | let x = 1;", &mut in_block));
        assert!(!keep_stderr_compile_line(
            "   |     ^ help: rename to _x",
            &mut in_block
        ));
        // Blank line terminates the block.
        assert!(!keep_stderr_compile_line("", &mut in_block));
        assert!(!in_block);
        // Normal content after the block passes through again.
        assert!(keep_stderr_compile_line("real test output", &mut in_block));
    }

    #[test]
    fn stderr_filter_strips_error_block() {
        let mut in_block = false;
        assert!(!keep_stderr_compile_line(
            "error[E0425]: cannot find value `foo`",
            &mut in_block
        ));
        assert!(in_block);
        assert!(!keep_stderr_compile_line(
            "  --> src/lib.rs:1:1",
            &mut in_block
        ));
        assert!(!keep_stderr_compile_line("", &mut in_block));
        assert!(!in_block);
        // `error:` (no brackets) also triggers the block.
        assert!(!keep_stderr_compile_line(
            "error: aborting due to previous error",
            &mut in_block
        ));
    }

    #[test]
    fn stderr_filter_strips_warning_summary_line() {
        let mut in_block = false;
        assert!(!keep_stderr_compile_line(
            "warning: `pbfhogg` (lib) generated 3 warnings",
            &mut in_block
        ));
        // The summary line triggers a block because it starts with `warning:`,
        // but the very next blank line closes it so subsequent content flows.
        assert!(in_block);
        assert!(!keep_stderr_compile_line("", &mut in_block));
        assert!(!in_block);
    }

    #[test]
    fn stderr_filter_keeps_non_compile_content() {
        let mut in_block = false;
        assert!(keep_stderr_compile_line(
            "some random line that isn't cargo",
            &mut in_block
        ));
        // Blank lines when not inside a block pass through - a blank line
        // between real output shouldn't be silently swallowed.
        assert!(keep_stderr_compile_line("", &mut in_block));
    }

    #[test]
    fn decide_sweeps_no_config_returns_legacy_default() {
        // No `[test]`, no `[[check]]` - the project hasn't migrated.
        // Single `--all-features` sweep, matching pre-redesign behaviour.
        let sweeps = decide_sweeps(None, &[]).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[0].features.configured_key(), vec!["--all-features"]);
        assert!(sweeps[0].build_packages.is_empty());
    }

    #[test]
    fn decide_sweeps_iterates_check_entries_when_no_default_profile() {
        // `[[check]]` configured, but no default_profile - every entry
        // runs in declaration order.
        let entries = vec![
            CheckEntry {
                name: "all".into(),
                features: vec!["test-hooks".into(), "linux-direct-io".into()],
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
        let sweeps = decide_sweeps(None, &entries).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(
            sweeps[0].features.configured_key(),
            vec!["--features", "test-hooks,linux-direct-io"]
        );
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert_eq!(sweeps[1].label, "consumer");
        assert_eq!(
            sweeps[1].features.configured_key(),
            vec!["--no-default-features", "--features", "commands"]
        );
    }

    #[test]
    fn decide_sweeps_uses_default_profile_when_set() {
        let toml_text = r#"
default_profile = "tier1"

[profiles.tier1]
sweeps = ["all", "consumer"]
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![
            CheckEntry {
                name: "all".into(),
                features: vec!["a".into()],
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
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert_eq!(sweeps[1].label, "consumer");
        assert_eq!(sweeps[1].build_packages, vec!["pbfhogg-cli"]);
    }

    /// The lanes fixture shared by the dedupe / selection tests: one
    /// `[[check]]` entry (`all`) referenced by two lanes, whose only
    /// difference is a dropped libtest filter - so both lanes share a build
    /// shape.
    fn lanes_cfg() -> (TestConfig, Vec<CheckEntry>) {
        let toml_text = r#"
default_profile = "pre-commit"

[profiles.tier1]
sweeps = ["all"]
skip = ["serial::"]

[profiles.serial]
sweeps = ["all"]
only = ["serial::"]
test_threads = 1

[profiles.pre-commit]
lanes = ["tier1", "serial"]
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        (test_cfg, entries)
    }

    #[test]
    fn decide_sweeps_keeps_every_lane_undeduped() {
        // decide_sweeps does not dedupe: it hands `run` the full lane list so
        // `--sweep` can see both lane-qualified labels. The collapse is the
        // run's selection's job, after `--sweep`.
        let (test_cfg, entries) = lanes_cfg();
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "tier1/all");
        assert_eq!(sweeps[1].label, "serial/all");
        assert_eq!(
            resolve_sweeps(Some(&test_cfg), &entries, None)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn the_selection_collapses_lanes_that_execute_identically() {
        // With the user's <name> as the only filter, the two lanes' runs are
        // identical, so the selection attempts the first and dedupes the
        // second onto it.
        let (test_cfg, entries) = lanes_cfg();
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(attempted(&sweeps, "pkg"), vec!["tier1/all"]);
        let sel = selection_for(&sweeps, "pkg");
        assert!(
            matches!(sel.entry(crate::check_cmd::LaneIdx::new(1)), Some(LaneEntry::Deduped(d)) if d.onto() == crate::check_cmd::LaneIdx::new(0))
        );
        assert_eq!(
            sweep_plan_line(&sweeps, &sel, "pkg"),
            "[test]    1 sweep for pkg: tier1/all (deduped: serial/all (as tier1/all))"
        );
    }

    #[test]
    fn select_then_dedupe_resolves_lane_qualified_label() {
        // S3-07: `--sweep serial/all` must resolve even though serial/all
        // executes what tier1/all does and would be deduped away. Selecting
        // before deduping is what makes the documented lane-qualified label
        // reachable.
        let (test_cfg, entries) = lanes_cfg();
        let sweeps = resolve_sweeps(Some(&test_cfg), &entries, Some("serial/all")).unwrap();
        assert_eq!(attempted(&sweeps, "pkg"), vec!["serial/all"]);
    }

    #[test]
    fn the_selection_spares_sweeps_differing_only_in_test_exclusions() {
        // S3-08: two lanes with the same build shape but different
        // `test_exclude_packages` are NOT identical `brokkr test` runs - one
        // skips the `-p` target, the other runs it. The selection keeps the
        // permitting one, so declaration order can't decide whether
        // `-p PKG NAME` finds its test.
        let excludes = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            label: "tier1/all".into(),
            features: crate::profile::FeatureConfig {
                tokens: vec!["a".into()],
                ..Default::default()
            },
            test_exclude_packages: vec!["pkg-x".into()],
            ..Default::default()
        });
        let permits = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            label: "serial/all".into(),
            features: crate::profile::FeatureConfig {
                tokens: vec!["a".into()],
                ..Default::default()
            },
            test_exclude_packages: Vec::new(),
            ..Default::default()
        });
        let sweeps = vec![excludes, permits];
        assert_eq!(attempted(&sweeps, "pkg-x"), vec!["serial/all"]);
        let sel = selection_for(&sweeps, "pkg-x");
        assert!(matches!(
            sel.entry(crate::check_cmd::LaneIdx::new(0)),
            Some(LaneEntry::Excluded(_))
        ));
    }

    /// Regression: a sweep and its doctest twin are two runs. The old dedupe
    /// key was the build shape, which leaves `doc_only` out, so the twin
    /// collapsed onto its sibling and `brokkr test NAME` never searched the
    /// doctests.
    #[test]
    fn a_doctest_twin_is_not_collapsed_onto_its_sibling() {
        let lib = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            label: "default".into(),
            ..crate::profile::SweepDecl::default()
        });
        let twin = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            label: "doctests".into(),
            lane_exec: crate::profile::LaneExec::doc_only(),
            ..crate::profile::SweepDecl::default()
        });
        let sweeps = vec![lib, twin];
        assert_eq!(attempted(&sweeps, "pkg"), vec!["default", "doctests"]);
    }

    #[test]
    fn decide_sweeps_carries_profile_env_through() {
        // B3 regression: a profile that exports `env = { FOO = "1" }`
        // used to round-trip through `brokkr check` but get silently
        // dropped on `brokkr test`. After consolidation, both paths
        // share decide_active_sweeps and env is preserved.
        let toml_text = r#"
default_profile = "platform"

[profiles.platform]
sweeps = ["all"]
include_ignored = true
env = { BROKKR_TEST_PLATFORM = "1", FOO = "bar" }
"#;
        let test_cfg: TestConfig = toml::from_str(toml_text).unwrap();
        let entries = vec![CheckEntry {
            name: "all".into(),
            features: vec!["a".into()],
            no_default_features: false,
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(
            sweeps[0]
                .env
                .get("BROKKR_TEST_PLATFORM")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(sweeps[0].env.get("FOO").map(String::as_str), Some("bar"));
        // libtest filters dropped (per `brokkr test` design).
        assert_eq!(sweeps[0].filters, crate::profile::LaneFilters::default());
    }

    #[test]
    fn decide_sweeps_default_profile_filters_dropped() {
        // `brokkr test <name>` uses the user's name as the filter; any
        // `only` / `skip` / `tests` / `include_ignored` / `test_threads`
        // declared by the profile is intentionally dropped (mixing them
        // with `<name>` caused silent zero-match failures).
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
            build_packages: Vec::new(),
            ..Default::default()
        }];
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 1);
        // Sweep struct only carries label / feature_args / build_packages -
        // any libtest filter from the profile is intentionally absent.
        assert_eq!(sweeps[0].label, "all");
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A filter every test name contains is a whole-suite run in disguise; one
    /// that leaves even a single test out is a selection.
    #[test]
    fn a_filter_matching_every_test_is_whole_suite() {
        let listed = names(&[
            "disclose::tests::mutes_a",
            "disclose::tests::mutes_b",
            "market::tests::routes_c",
            "market::tests::routes_d",
            "run_prep_e",
        ]);
        for broad in ["_", "e", ""] {
            assert!(
                matches_whole_suite(&listed, broad),
                "{broad:?} matches every test"
            );
        }
        for narrow in ["::", "tests::", "market::", "run_prep_e", "nothing"] {
            assert!(
                !matches_whole_suite(&listed, narrow),
                "{narrow:?} is a selection"
            );
        }
    }

    /// Below the floor a match-everything name is a plausible "run this small
    /// crate's tests", so it runs.
    #[test]
    fn a_package_below_the_floor_is_never_whole_suite() {
        let listed = names(&["a::x", "a::y", "a::z", "a::w"]);
        assert!(listed.len() < WHOLE_SUITE_FLOOR);
        assert!(!matches_whole_suite(&listed, "a::"));
        assert!(
            !matches_whole_suite(&[], "anything"),
            "an empty package matches nothing"
        );
    }

    /// The pre-build compiles the lane like the test run does: the pinned
    /// unification and the `[lints] allow` `--config` share both ride it, or a
    /// package-mode lane pre-builds under ambient resolution and a denied lint
    /// fails the pre-build before `cargo test` is reached.
    #[test]
    fn pre_build_carries_the_unification_pin_and_the_allow_config() {
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            packages: vec!["daemon".to_owned()],
            feature_unification: crate::config::FeatureUnification::Package,
            ..crate::profile::SweepDecl::default()
        });
        let allow = vec![
            "--config".to_owned(),
            "build.rustflags=[\"-A\",\"x\"]".to_owned(),
        ];
        let args = pre_build_argv(
            &sweep,
            &support_for(&sweep, "daemon", "daemon"),
            &allow,
            false,
        );
        assert!(
            args.iter()
                .any(|a| a == "resolver.feature-unification=\"package\""),
            "{args:?}"
        );
        assert!(
            args.iter().any(|a| a == "build.rustflags=[\"-A\",\"x\"]"),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "--release"), "{args:?}");

        // An unpinned sweep with no allows stays byte-identical to before.
        let plain_sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default());
        let plain = pre_build_argv(
            &plain_sweep,
            &support_for(&plain_sweep, "core", "bin"),
            &[],
            true,
        );
        assert_eq!(
            plain,
            [
                "build",
                "--message-format=json-render-diagnostics",
                "-p",
                "bin"
            ]
        );
    }

    /// The support request the run's selection plans for `support` when
    /// `brokkr test -p pkg` runs `sweep` with `support` as a build package.
    fn support_for(sweep: &PlannedSweep, pkg: &str, support: &str) -> check_cmd::SupportRequest {
        let with = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            build_packages: vec![support.to_owned()],
            ..sweep.declaration().clone()
        });
        let sel = selection_for(std::slice::from_ref(&with), pkg);
        sel.attempt(crate::check_cmd::LaneIdx::new(0))
            .unwrap()
            .support()[0]
            .clone()
    }

    /// `brokkr test -p strategy` projects a sweep's features onto the one
    /// package, and its support build onto the support package: the bug
    /// where cargo rejected every token the narrowed package did not define.
    #[test]
    fn the_run_and_its_support_build_carry_projected_features() {
        let ws = check_cmd::feature_fixture::piners();
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            label: "piners".into(),
            packages: vec![
                "piners-vm".into(),
                "piners-strategy".into(),
                "piners-harness".into(),
            ],
            build_packages: vec!["piners-runner".into()],
            features: crate::profile::FeatureConfig {
                tokens: vec![
                    "piners-vm/opcode-counts".into(),
                    "piners-strategy/hotpath".into(),
                    "piners-harness/bench".into(),
                ],
                ..Default::default()
            },
            ..crate::profile::SweepDecl::default()
        });
        let sel = check_cmd::PhaseSelection::for_brokkr_test(
            std::slice::from_ref(&sweep),
            "piners-strategy",
            check_cmd::PackageSource::Cli,
            &|_| false,
            &check_cmd::FeatureOracle::fixed(ws),
        )
        .unwrap();
        let attempt = sel.attempt(crate::check_cmd::LaneIdx::new(0)).unwrap();
        let run = single_run(&sweep, attempt).unwrap();
        let shape = BuildShape {
            sweep: &sweep,
            allow_args: &[],
            run,
            jobs: None,
            debug: true,
        };
        let pre = prebuild_argv(&shape);
        assert!(
            pre.windows(2).any(|w| w
                == [
                    "--features",
                    "piners-vm/opcode-counts,piners-strategy/hotpath"
                ]),
            "{pre:?}"
        );
        let support = pre_build_argv(&sweep, &attempt.support()[0], &[], true);
        assert!(
            support
                .windows(2)
                .any(|w| w == ["--features", "piners-vm/opcode-counts"]),
            "{support:?}"
        );
        assert!(
            support.ends_with(&["-p".to_owned(), "piners-runner".to_owned()]),
            "{support:?}"
        );
    }

    fn ready_sweep<'s>(
        sweep: &'s PlannedSweep,
        harness: &check_cmd::TestBinary,
        support: &[check_cmd::SupportArtifact],
    ) -> ReadySweep<'s> {
        let hash = test_runner::hash_file(Path::new(&harness.executable)).unwrap();
        let attempt = selection_for(std::slice::from_ref(sweep), "core")
            .attempt(crate::check_cmd::LaneIdx::new(0))
            .unwrap()
            .clone();
        ReadySweep {
            sweep,
            attempt,
            debug: true,
            env: Vec::new(),
            allow_args: Vec::new(),
            focused: Some(FocusedPlan {
                runs: Vec::new(),
                searched: 1,
                exact: None,
                artifacts: vec![check_cmd::PlannedArtifact::new(
                    None,
                    check_cmd::BinaryUnit::of(harness),
                    harness.executable.clone(),
                    hash,
                )],
            }),
            exact: None,
            support_fingerprint: check_cmd::support_fingerprint(support).unwrap(),
            runtime_fingerprint: check_cmd::BuildRuntimeIndex::default()
                .fingerprint()
                .unwrap(),
        }
    }

    /// Validation case: preparing every sweep up front must not let one
    /// sweep's build stand in for another's. Two feature sweeps share one
    /// `build_packages` executable path, so the second sweep's preparation
    /// overwrote the first's - and the first sweep's tests then spawned the
    /// second sweep's CLI. Each sweep is held to its prepared record when its
    /// turn comes: a clean rebuild is unchanged, a support binary another
    /// sweep replaced is drift, and so is a harness rebuilt with other content.
    #[test]
    fn a_sweep_is_held_to_its_prepared_record_before_it_runs() {
        let dir = crate::test_scratch::scratch("test_cmd", "sweep_drift");
        let server = dir.join("server");
        std::fs::write(&server, b"first sweep's server").unwrap();
        let harness_path = dir.join("suite-1");
        std::fs::write(&harness_path, b"suite").unwrap();
        let mut harness = check_cmd::test_binary_for_tests("core", "test", "suite");
        harness.executable = harness_path.to_string_lossy().into_owned();
        let support = vec![check_cmd::SupportArtifact {
            package: "server".into(),
            target: "server".into(),
            executable: server.to_string_lossy().into_owned(),
        }];
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default());
        let prepared = ready_sweep(&sweep, &harness, &support);
        let rebuild = |_: usize| {
            Ok(Some((
                vec![harness.clone()],
                check_cmd::BuildRuntimeIndex::default(),
            )))
        };

        assert!(
            sweep_drift(&prepared, rebuild, &support)
                .unwrap()
                .unwrap()
                .is_empty(),
            "an untouched sweep"
        );

        // The second sweep's build re-uplifted the shared support binary.
        std::fs::write(&server, b"second sweep's server").unwrap();
        let drift = sweep_drift(&prepared, rebuild, &support).unwrap().unwrap();
        assert!(
            drift.iter().any(|d| d.contains("support build")),
            "{drift:?}"
        );
        // Rebuilding restores it, and the sweep verifies again.
        std::fs::write(&server, b"first sweep's server").unwrap();
        assert!(
            sweep_drift(&prepared, rebuild, &support)
                .unwrap()
                .unwrap()
                .is_empty()
        );

        // A harness rebuilt with other content.
        std::fs::write(&harness_path, b"another sweep's suite").unwrap();
        let drift = sweep_drift(&prepared, rebuild, &support).unwrap().unwrap();
        assert!(
            drift
                .iter()
                .any(|d| d.contains("rebuilt with different content")),
            "{drift:?}"
        );

        // A rebuild that fails is not drift, it is a failed build.
        assert!(
            sweep_drift(&prepared, |_| Ok(None), &support)
                .unwrap()
                .is_none()
        );

        // A doc-only sweep has no harness to rebuild but still has its support.
        std::fs::write(&harness_path, b"suite").unwrap();
        let mut doc = ready_sweep(&sweep, &harness, &support);
        doc.focused = None;
        assert!(
            sweep_drift(&doc, |_| Ok(None), &support)
                .unwrap()
                .unwrap()
                .is_empty()
        );
        std::fs::write(&server, b"second sweep's server").unwrap();
        assert!(
            !sweep_drift(&doc, |_| Ok(None), &support)
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    /// Wiring: `run_ready` itself rebuilds and re-verifies each sweep BEFORE
    /// it executes anything, through the real `settle_sweep` and
    /// `recheck_sweep`. The sweep here is doc-only (so the recheck builds
    /// nothing) with a recorded support fingerprint the rebuild no longer
    /// produces - drift - and runs under `--timeout`, so the re-preparation it
    /// triggers ends in the doc-only refusal without cargo. (Only an attempted
    /// sweep is ever prepared, so the refusal is the cargo-free end of a
    /// re-preparation; the excluded-sweep skip it used to end in now happens
    /// before preparation.) If `run_ready` stopped calling `settle_sweep`, the
    /// sweep would go straight to `run_iteration` (a `cargo test --doc` in a
    /// directory with no project) and the error would not be the refusal.
    #[test]
    fn run_ready_rechecks_a_sweep_before_it_runs() {
        let dir = crate::test_scratch::scratch("test_cmd", "run_ready_recheck");
        let server = dir.join("server");
        std::fs::write(&server, b"the planned server").unwrap();
        let support = vec![check_cmd::SupportArtifact {
            package: "server".into(),
            target: "server".into(),
            executable: server.to_string_lossy().into_owned(),
        }];
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            label: "docs".into(),
            lane_exec: crate::profile::LaneExec::doc_only(),
            ..crate::profile::SweepDecl::default()
        });
        let mut harness = check_cmd::test_binary_for_tests("core", "test", "suite");
        harness.executable = server.to_string_lossy().into_owned();
        let mut prepared = ready_sweep(&sweep, &harness, &support);
        prepared.focused = None;
        let target_dir = dir.join("target");
        let cx = SweepContext {
            project: Project::Brokkr,
            project_root: &dir,
            state_root: &dir,
            test_cfg: None,
            target_dir: &target_dir,
            allow_flags: &[],
            name: "t",
            jobs: None,
            multi: false,
            profile_override: None,
            timeout: Some(30),
        };
        let mut reports = Vec::new();
        let mut notes = Vec::new();
        let err = run_ready(
            std::slice::from_ref(&prepared),
            &cx,
            1,
            Duration::from_secs(20),
            None,
            &mut reports,
            &mut notes,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("doc-only sweep 'docs'"),
            "the drifted sweep was re-prepared, not run: {err}"
        );
        assert!(reports.is_empty());
    }

    #[test]
    fn resolve_debug_cli_override_wins_over_config() {
        let debug_cfg = TestConfig {
            debug: true,
            ..Default::default()
        };
        // --release (Some(false)) beats `[test] debug = true`.
        assert!(!resolve_debug(Some(false), Some(&debug_cfg), None));
        // --debug (Some(true)) holds even when config says release.
        assert!(resolve_debug(
            Some(true),
            Some(&TestConfig::default()),
            None
        ));
        // The CLI also beats a sweep's own pinned profile - the
        // per-invocation answer is the most specific one there is.
        assert!(resolve_debug(Some(true), None, Some(SweepProfile::Release)));
        assert!(!resolve_debug(Some(false), None, Some(SweepProfile::Dev)));
    }

    #[test]
    fn a_sweeps_profile_beats_the_project_wide_default() {
        // The case the key exists for: a repo pins the fast dev build
        // project-wide, and one sweep holds a wall-clock contract that only
        // means anything optimized. `brokkr test <that test>` must run it
        // release without the caller remembering `--release`.
        let debug_cfg = TestConfig {
            debug: true,
            ..Default::default()
        };
        assert!(!resolve_debug(
            None,
            Some(&debug_cfg),
            Some(SweepProfile::Release)
        ));
        // And the mirror: brokkr's own default is release, so a sweep that
        // pins dev (for compile speed) gets dev with no flag either.
        assert!(resolve_debug(
            None,
            Some(&TestConfig::default()),
            Some(SweepProfile::Dev)
        ));
    }

    #[test]
    fn resolve_debug_falls_back_to_config_then_release() {
        let debug_cfg = TestConfig {
            debug: true,
            ..Default::default()
        };
        // No CLI flag, no sweep profile: `[test] debug = true` decides.
        assert!(resolve_debug(None, Some(&debug_cfg), None));
        // No CLI flag, config defaults to release.
        assert!(!resolve_debug(None, Some(&TestConfig::default()), None));
        // No CLI flag, no `[test]` section at all -> release.
        assert!(!resolve_debug(None, None, None));
    }

    #[test]
    fn aggregate_exit_fails_on_any_fail() {
        let outcomes = [Outcome::Pass, Outcome::Fail];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default().describe()).is_err());
    }

    #[test]
    fn aggregate_exit_fails_on_any_build_failed() {
        let outcomes = [Outcome::Pass, Outcome::BuildFailed];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default().describe()).is_err());
    }

    #[test]
    fn aggregate_exit_fails_when_all_no_match() {
        let outcomes = [Outcome::NoMatch, Outcome::NoMatch];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default().describe()).is_err());
    }

    #[test]
    fn the_no_match_error_names_what_was_searched() {
        let message = |searched: &Searched| match aggregate_exit(
            &[Outcome::NoMatch],
            "pkg",
            "abc",
            &searched.describe(),
        ) {
            Err(DevError::Reported(m)) => m,
            other => panic!("expected a Reported error, got {other:?}"),
        };
        let two = Searched {
            harnesses: 44,
            doc_only: 0,
            labels: vec!["default".into(), "strategy-features".into()],
        };
        assert_eq!(
            message(&two),
            "no test matches `abc` in pkg (44 harnesses searched in default and strategy-features)"
        );
        let three = Searched {
            labels: vec!["a".into(), "b".into(), "c".into()],
            ..two
        };
        assert!(message(&three).ends_with("(44 harnesses searched in a, b and c)"));
        // A single sweep has no plan line, so its search must name it too.
        let one = Searched {
            harnesses: 1,
            doc_only: 0,
            labels: vec!["default".into()],
        };
        assert_eq!(
            message(&one),
            "no test matches `abc` in pkg (1 harness searched in default)"
        );
        // A doc-only sweep searches doctests, not countable harnesses.
        let doc = Searched {
            harnesses: 0,
            doc_only: 1,
            labels: vec!["docs".into()],
        };
        assert_eq!(
            message(&doc),
            "no test matches `abc` in pkg (doctests searched in docs)"
        );
        // A lone sweep scoped out of the package searched nothing, and said why itself.
        assert_eq!(
            message(&Searched::default()),
            "no test matches `abc` in pkg"
        );
    }

    #[test]
    fn only_a_failing_run_shows_its_invocation() {
        let state = RepeatState::default();
        // The dedupe set is what decides a console print: a pass never enters
        // it, a failure enters it once.
        show_reproduction(&state, false, "bin args (cwd .)");
        assert!(state.first_hint("bin args (cwd .)"), "a pass left no mark");
        show_reproduction(&state, true, "other args (cwd .)");
        assert!(
            !state.first_hint("other args (cwd .)"),
            "a failure printed (and was recorded)"
        );
        show_reproduction(&state, true, "cargo test --doc");
        assert!(
            !state.first_hint("cargo test --doc"),
            "a doc-only build failure printed its command"
        );
        show_failed_invocation(&state, "missing-bin args (cwd .)");
        assert!(
            !state.first_hint("missing-bin args (cwd .)"),
            "a runner error printed its command"
        );
    }

    #[test]
    fn hung_test_tag_requires_a_resolved_test_and_a_per_test_reason() {
        use test_runner::{Attribution, Ceilings, HungTest, TimeoutReason};

        let render = |name: &str, wall: &str| format!("pkg::{name} ({wall})");
        let mut shape = OneRun {
            tag: "pkg::filter",
            test_tag: Some(&render),
            target: "lib",
            ceilings: Ceilings::one_test(Duration::from_secs(20), "tests::hang"),
            announce: false,
            expected: Some(1),
            listed: None,
            observe: test_runner::Observe::default(),
        };
        let exact = || Attribution::Exact {
            name: "tests::hang".into(),
        };
        let mut hung = HungTest {
            reason: TimeoutReason::Watched,
            attribution: exact(),
            elapsed: Duration::from_secs(20),
            ceiling: Duration::from_secs(20),
            snapshot_dir: PathBuf::new(),
            cargo_pid: 0,
            test_pids: Vec::new(),
            snapshot_pid: None,
            wchan: None,
            stack: None,
            snapshot_error: None,
        };
        // The runner attributes both the resolved test's tracker expiry and
        // its invocation's wall expiry to the caller's name, even without a
        // start event.
        assert_eq!(
            hung_test_tag(&shape, &hung).map(|tag| tag("tests::hang", "20.00s")),
            Some("pkg::tests::hang (20.00s)".into())
        );
        hung.reason = TimeoutReason::OneTestWall;
        assert!(hung_test_tag(&shape, &hung).is_some());
        shape.ceilings = Ceilings::shared_harness();
        hung.reason = TimeoutReason::Watched;
        hung.attribution = Attribution::Suspect {
            name: "tests::hang".into(),
            stale: false,
        };
        assert!(
            hung_test_tag(&shape, &hung).is_none(),
            "a suspect is not a verified name"
        );
        shape.ceilings = Ceilings::one_test(Duration::from_secs(20), "tests::hang");
        hung.reason = TimeoutReason::Idle;
        hung.attribution = Attribution::None;
        assert!(hung_test_tag(&shape, &hung).is_none());
        hung.reason = TimeoutReason::Watched;
        hung.attribution = exact();
        shape.test_tag = None;
        assert!(
            hung_test_tag(&shape, &hung).is_none(),
            "no renderer in a doc-only run"
        );
    }

    #[test]
    fn aggregate_exit_label_names_what_failed() {
        let label = |outcomes: &[Outcome]| match aggregate_exit(
            outcomes,
            "f",
            "n",
            &Searched::default().describe(),
        ) {
            Err(DevError::Reported(label)) => label,
            other => panic!("expected a Reported error, got {other:?}"),
        };
        assert_eq!(
            label(&[Outcome::Pass, Outcome::BuildFailed]),
            "build failed"
        );
        assert_eq!(label(&[Outcome::Fail, Outcome::NoMatch]), "test failed");
        assert_eq!(
            label(&[Outcome::Fail, Outcome::BuildFailed]),
            "test failed and build failed"
        );
        // The no-match fact is stated once, by the closing label.
        assert!(label(&[Outcome::NoMatch]).starts_with("no test matches `n` in f"));
    }

    #[test]
    fn aggregate_exit_passes_when_any_pass_with_no_match() {
        // The important case: feature-gated test passes in one sweep, SKIPs
        // in the consumer sweep. Exit code should be 0.
        let outcomes = [Outcome::Pass, Outcome::NoMatch];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default().describe()).is_ok());
    }

    #[test]
    fn aggregate_exit_passes_on_all_pass() {
        let outcomes = [Outcome::Pass, Outcome::Pass];
        assert!(aggregate_exit(&outcomes, "f", "n", &Searched::default().describe()).is_ok());
    }

    /// The one exclusion note of `sweep` for `pkg`, or `None` when the
    /// selection attempts it.
    fn exclusion_of(sweep: &PlannedSweep, pkg: &str) -> Option<check_cmd::AdmissionRule> {
        match selection_for(std::slice::from_ref(sweep), pkg)
            .entry(crate::check_cmd::LaneIdx::new(0))
        {
            Some(LaneEntry::Excluded(e)) => exclusion_rule(e.notes()),
            _ => None,
        }
    }

    #[test]
    fn a_sweep_with_no_packages_list_admits_any_target() {
        // A sweep with no `packages` list covers every target.
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl::default());
        assert!(exclusion_of(&sweep, "nautilus-hyperliquid").is_none());
        // And the target replaces the sweep's own selection.
        let sel = selection_for(std::slice::from_ref(&sweep), "nautilus-hyperliquid");
        assert_eq!(
            check_cmd::package_args(
                sel.attempt(crate::check_cmd::LaneIdx::new(0))
                    .unwrap()
                    .selection()
            ),
            ["-p", "nautilus-hyperliquid"]
        );
    }

    #[test]
    fn a_target_outside_the_packages_list_is_excluded() {
        // The reported case: an ffi/live sweep lists only its member packages;
        // `-p nautilus-hyperliquid` isn't one, so it must SKIP rather than
        // force a build of a feature the package doesn't declare.
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            packages: vec!["nautilus-core".into(), "nautilus-common".into()],
            ..crate::profile::SweepDecl::default()
        });
        assert_eq!(
            exclusion_of(&sweep, "nautilus-hyperliquid"),
            Some(check_cmd::AdmissionRule::NotListed)
        );
    }

    #[test]
    fn a_target_inside_the_packages_list_is_attempted() {
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            packages: vec!["nautilus-core".into(), "nautilus-hyperliquid".into()],
            ..crate::profile::SweepDecl::default()
        });
        assert_eq!(
            attempted(std::slice::from_ref(&sweep), "nautilus-hyperliquid").len(),
            1
        );
    }

    #[test]
    fn the_plan_line_keeps_the_two_scope_reasons_apart() {
        let sweeps = vec![
            crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
                label: "default".into(),
                ..crate::profile::SweepDecl::default()
            }),
            crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
                label: "vm".into(),
                packages: vec!["other".into()],
                ..crate::profile::SweepDecl::default()
            }),
            crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
                label: "ffi".into(),
                test_exclude_packages: vec!["pkg".into()],
                ..crate::profile::SweepDecl::default()
            }),
        ];
        assert_eq!(
            sweep_plan_line(&sweeps, &selection_for(&sweeps, "pkg"), "pkg"),
            "[test]    1 sweep for pkg: default (not this package: vm; excluded by \
             test_exclude_packages: ffi)"
        );
    }

    #[test]
    fn an_excluded_target_is_excluded_by_test_exclude_packages() {
        // An otherwise-workspace-wide sweep carving the target out.
        let sweep = crate::profile::PlannedSweep::from(crate::profile::SweepDecl {
            test_exclude_packages: vec!["nautilus-pyo3".into()],
            ..crate::profile::SweepDecl::default()
        });
        assert_eq!(
            exclusion_of(&sweep, "nautilus-pyo3"),
            Some(check_cmd::AdmissionRule::TestExcluded)
        );
        assert_eq!(
            exclusion_describe(&[check_cmd::AdmissionNote {
                package: "nautilus-pyo3".into(),
                rule: check_cmd::AdmissionRule::TestExcluded
            }]),
            "package excluded from this sweep (test_exclude_packages)"
        );
    }
}
