//! `brokkr test <NAME>` - single-test cargo runner.
//!
//! Runs exactly one named cargo test with the host/check features and
//! `--include-ignored --nocapture --test-threads=1`. Defaults to release;
//! `--debug` switches to the dev profile. Streams the test's own
//! stdout/stderr live (filtering out cargo/test-harness framing noise), then
//! prints a `[test]` PASS/FAIL footer per sweep with wall time. Under `-N`,
//! the `[run] cargo ...` and build-time framing prints for run 1 only -
//! repeats collapse to their footer line.
//!
//! Feature selection follows the same priority ladder as
//! `brokkr check`'s test phase, with two intentional differences:
//! profile libtest filters (`only` / `skip` / `tests`) are dropped
//! (the user's `<NAME>` is the filter), and CLI `--features` is not
//! accepted. Profile-declared `env` vars *are* propagated, so a
//! profile that gates platform tests behind `BROKKR_TEST_PLATFORM=1`
//! still works under `brokkr test`.
//!
//! The per-test watchdog ceiling (shared with `brokkr check`, normally
//! 20s) can be raised with `--timeout <SECS>` (1-280). Because a higher
//! ceiling only makes sense for one isolated test, it is gated: each
//! sweep is enumerated with libtest `--list` first, and `<NAME>` matching
//! more than one test in any sweep is a hard error before anything runs.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::build;
use crate::cargo_filter;
use crate::check_cmd;
use crate::config::DevConfig;
use crate::error::DevError;
use crate::output;
use crate::profile::ResolvedSweep;
use crate::project::Project;
use crate::test_runner::{self, LibtestOutcome};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    BuildFailed,
    /// Cargo ran but no test matched the name. Could mean "wrong name" or
    /// "feature-gated out of this sweep" - only distinguishable by checking
    /// the other sweeps' outcomes. The aggregator in `run` decides how to
    /// exit based on whether any sweep saw a Pass.
    NoMatch,
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    dev_config: &DevConfig,
    project: Project,
    project_root: &Path,
    name: &str,
    package: Option<&str>,
    repeat: u32,
    jobs: Option<u32>,
    raw: bool,
    debug: bool,
    timeout: Option<u64>,
) -> Result<(), DevError> {
    let repeat = repeat.max(1);
    let ceiling = timeout.map_or(test_runner::TEST_TIMEOUT, Duration::from_secs);
    let sweeps = decide_sweeps(dev_config.test.as_ref(), &dev_config.check)?;
    let multi = sweeps.len() > 1;

    let pkg = resolve_package(package, dev_config, project)?;

    // `brokkr test` defaults to `cargo test --release` (debug=false ->
    // <target>/release); `--debug` flips both the cargo invocation and
    // BROKKR_TEST_BIN_DIR over to <target>/debug. Tests that spawn the
    // just-rebuilt binary read this var to skip the
    // `cfg!(debug_assertions)` profile guess (which silently lies when
    // a workspace pins `[profile.test]` overrides).
    let profile_dir = if debug { "debug" } else { "release" };
    let target_dir = build::project_info(Some(project_root))?.target_dir;
    let project_env = check_cmd::build_test_env(Some(project), &target_dir, profile_dir);

    let mut outcomes: Vec<Outcome> = Vec::new();

    for sweep in &sweeps {
        if multi {
            println!("[test]    sweep: {}", sweep.label);
        }

        // Merge profile-declared env onto the project's always-set vars.
        // Profile env wins on collision so a profile can shadow defaults
        // when it really needs to (request 3 / B3: brokkr test was
        // dropping this and a `default_profile` env didn't apply).
        let env_owned = check_cmd::merged_env(&sweep.env, project_env.as_slice());
        let env_refs: Vec<(&str, &str)> = env_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Pre-build any binary packages declared by this sweep. Skipped
        // when build_packages is empty (fallback path). Failure here
        // short-circuits the run with a BuildFailed outcome so the
        // aggregator marks the sweep as failed.
        let mut pre_build_failed = false;
        for build_pkg in &sweep.build_packages {
            if !run_pre_build(project_root, sweep, build_pkg, &env_refs, raw, debug)? {
                pre_build_failed = true;
                outcomes.push(Outcome::BuildFailed);
                break;
            }
        }
        if pre_build_failed {
            // Skip the test phase for this sweep; the next sweep gets
            // its own chance.
            continue;
        }

        // A `--timeout` override is only honored for a single isolated
        // test. Enumerate the matches with libtest `--list` up front and
        // refuse to run if `<name>` is a prefix that pulls in several
        // tests under one raised ceiling. Sweeps that match zero (the
        // test is feature-gated out) are fine - they'll just SKIP.
        if timeout.is_some() {
            let matched = count_matching_tests(&pkg, name, sweep, &env_refs, project_root, debug)?;
            if matched > 1 {
                return Err(DevError::Config(format!(
                    "--timeout only applies to a single test, but `{name}` matches {matched} tests \
                     in sweep `{}`. Narrow it to one fully-qualified test name \
                     (e.g. `my_module::my_test`), or drop --timeout to run them all at the 20s ceiling.",
                    sweep.label
                )));
            }
        }

        for n in 1..=repeat {
            let mut args: Vec<String> = vec!["test".into()];
            if !debug {
                args.push("--release".into());
            }
            args.extend(sweep.cargo_feature_args.iter().cloned());
            if let Some(j) = jobs {
                args.push("-j".into());
                args.push(j.to_string());
            }
            args.push("-p".into());
            args.push(pkg.clone());
            args.push(name.into());
            args.push("--".into());
            args.push("--include-ignored".into());
            args.push("--nocapture".into());
            args.push("--test-threads=1".into());

            let label = sweep.label.as_str();
            let tag = match (multi, repeat > 1) {
                (true, true) => format!("{pkg}::{name} [{label}] run {n}/{repeat}"),
                (true, false) => format!("{pkg}::{name} [{label}]"),
                (false, true) => format!("{pkg}::{name} run {n}/{repeat}"),
                (false, false) => format!("{pkg}::{name}"),
            };

            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            // Under `-N`, the invocation and build-time framing is identical
            // every iteration - print it for run 1 only and let repeats
            // collapse to their PASS/FAIL footer line.
            let announce = n == 1;
            if announce {
                output::run_msg(&format!("cargo {}", arg_refs.join(" ")));
            }

            let outcome =
                run_one(&arg_refs, project_root, &env_refs, &tag, raw, ceiling, announce)?;
            outcomes.push(outcome);
        }
    }

    aggregate_exit(&outcomes, &pkg, name)
}

/// Build one cargo package with the sweep's feature flags before
/// running tests. Returns `Ok(true)` on build success, `Ok(false)` on
/// build failure (already reported), `Err` on spawn failure.
fn run_pre_build(
    project_root: &Path,
    sweep: &ResolvedSweep,
    package: &str,
    env: &[(&str, &str)],
    raw: bool,
    debug: bool,
) -> Result<bool, DevError> {
    let mut args: Vec<String> = vec!["build".into()];
    if !debug {
        args.push("--release".into());
    }
    args.extend(sweep.cargo_feature_args.iter().cloned());
    args.push("--package".into());
    args.push(package.into());

    output::run_msg(&format!(
        "cargo {} (sweep build: {})",
        args.join(" "),
        sweep.label
    ));

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, env)?;

    if captured.status.success() {
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&captured.stderr);
    if raw {
        if !stderr.is_empty() {
            output::error(&stderr);
        }
    } else {
        let filtered = cargo_filter::filter_clippy(&stderr);
        if !filtered.is_empty() {
            output::error(&filtered);
        }
    }
    println!(
        "[test]    BUILD FAILED {package} (sweep: {})",
        sweep.label
    );
    Ok(false)
}

/// Count how many tests `<name>` matches in this sweep via libtest
/// `--list`. Used to gate the `--timeout` override on a single match.
/// Builds (cache-shared with the real run that follows) then lists; each
/// runnable test prints a `path::to::test: test` line, so we count those.
/// A build/list failure returns `Ok(0)` so the subsequent real run is the
/// one that surfaces the compile error through the normal BUILD FAILED path.
fn count_matching_tests(
    pkg: &str,
    name: &str,
    sweep: &ResolvedSweep,
    env: &[(&str, &str)],
    project_root: &Path,
    debug: bool,
) -> Result<usize, DevError> {
    let mut args: Vec<String> = vec!["test".into()];
    if !debug {
        args.push("--release".into());
    }
    args.extend(sweep.cargo_feature_args.iter().cloned());
    args.push("-p".into());
    args.push(pkg.into());
    args.push(name.into());
    args.push("--".into());
    args.push("--include-ignored".into());
    args.push("--list".into());

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, env)?;
    if !captured.status.success() {
        return Ok(0);
    }
    Ok(count_listed_tests(&String::from_utf8_lossy(&captured.stdout)))
}

/// Count libtest `--list` entries that are runnable tests. The list format
/// is one `name: kind` line per entry; `kind` is `test` or `benchmark`.
fn count_listed_tests(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|l| l.trim_end().ends_with(": test"))
        .count()
}

/// Resolve the cargo package name in precedence order:
///   1. Explicit `-p` on the CLI (most specific)
///   2. `[test] default_package` in brokkr.toml (explicit config wins
///      over implicit per-project heuristic)
///   3. `Project::cli_package()` (built-in knowledge for pbfhogg/nidhogg)
///   4. Error with a message pointing the user at options 1/2
fn resolve_package(
    cli_package: Option<&str>,
    dev_config: &DevConfig,
    project: Project,
) -> Result<String, DevError> {
    if let Some(p) = cli_package {
        return Ok(p.to_owned());
    }
    if let Some(cfg) = &dev_config.test
        && let Some(p) = &cfg.default_package
    {
        return Ok(p.clone());
    }
    if let Some(p) = project.cli_package() {
        return Ok(p.to_owned());
    }
    Err(DevError::Config(format!(
        "'brokkr test' needs a cargo package for '-p'. This project ({project}) has no built-in default. \
         Pass `-p <pkg>` on the command line, or set `[test] default_package = \"...\"` in brokkr.toml."
    )))
}

fn aggregate_exit(outcomes: &[Outcome], pkg: &str, name: &str) -> Result<(), DevError> {
    let any_fail = outcomes
        .iter()
        .any(|o| matches!(o, Outcome::Fail | Outcome::BuildFailed));
    if any_fail {
        return Err(DevError::Build("test failed".into()));
    }
    let all_no_match = outcomes.iter().all(|o| *o == Outcome::NoMatch);
    if all_no_match {
        println!("[test]    no sweep matched `{pkg}::{name}` - check the package/name.");
        return Err(DevError::Build("no matching test".into()));
    }
    // At least one sweep passed; NoMatch in other sweeps is informational
    // (the test was feature-gated out of those sweeps).
    Ok(())
}

/// Decide which sweeps `brokkr test` runs.
///
/// Reuses `check_cmd::decide_active_sweeps` (no CLI features, no
/// `--profile` override - resolution falls through to
/// `[test] default_profile` -> `[[check]]` entries -> legacy
/// `--all-features`), then drops the libtest filters that would
/// fight with the user's `<name>` argument. `env` is preserved (B3:
/// silent profile-env drop fixed by this consolidation).
fn decide_sweeps(
    test_cfg: Option<&crate::config::TestConfig>,
    check_entries: &[crate::config::CheckEntry],
) -> Result<Vec<ResolvedSweep>, DevError> {
    let mut sweeps = check_cmd::decide_active_sweeps(check_entries, test_cfg, None, &[], false)?;
    for s in &mut sweeps {
        // The user's `<name>` is the libtest filter. Profile-level
        // `only` / `skip` / `tests` would either narrow it further (rare,
        // surprising) or cause silent zero-match failures; drop them.
        s.libtest_args.clear();
        s.cargo_test_filters.clear();
        s.name_filters.clear();
    }
    Ok(sweeps)
}

/// Run one `cargo test` invocation. Prints the `[test]` footer and returns
/// the outcome. Err only on spawn failure. `announce` gates the
/// "test binaries built in Xs" framing line - false for `-N` repeats
/// after the first, where the build is cached and the line is noise.
#[allow(clippy::too_many_arguments)]
fn run_one(
    args: &[&str],
    project_root: &Path,
    env: &[(&str, &str)],
    tag: &str,
    raw: bool,
    ceiling: Duration,
    announce: bool,
) -> Result<Outcome, DevError> {
    let run = test_runner::streaming_run_libtest(
        args,
        project_root,
        env,
        ceiling,
        make_stdout_forwarder(raw),
        make_stderr_forwarder(raw),
        move |elapsed| {
            if announce {
                println!(
                    "[test]    test binaries built in {:.1}s; running tests",
                    elapsed.as_secs_f64()
                );
            }
        },
    )?;

    let stdout_text = String::from_utf8_lossy(&run.captured.stdout);
    let stderr_text = String::from_utf8_lossy(&run.captured.stderr);
    let stdout_lines: Vec<&str> = stdout_text.lines().collect();
    let stderr_lines: Vec<&str> = stderr_text.lines().collect();
    let parsed = cargo_filter::parse_test_output(&stdout_lines);

    let has_test_result = stdout_lines.iter().any(|l| l.starts_with("test result:"));
    let has_compile_error = stderr_lines.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("error[") || (t.starts_with("error:") && !t.contains("test run failed"))
    });

    // Display the test-runtime wall: total minus the cargo build phase
    // (which the `[test] test binaries built in ...s` line already
    // surfaces). Falls back to total if cargo never reported `Finished`.
    let test_wall = run
        .build_elapsed
        .map_or(run.captured.elapsed, |b| run.captured.elapsed.saturating_sub(b));
    let wall = format!("{:.2}s", test_wall.as_secs_f64());

    if let LibtestOutcome::HungTest(hung) = &run.outcome {
        output::error(&test_runner::format_hung_test(hung, project_root));
        println!(
            "[test]    FAIL {tag} ({wall}) - hung test exceeded {}s",
            hung.ceiling.as_secs()
        );
        std::io::stdout().flush().ok();
        return Ok(Outcome::Fail);
    }

    if !has_test_result && has_compile_error {
        if !raw {
            let filtered = cargo_filter::filter_clippy(stderr_text.as_ref());
            if !filtered.is_empty() {
                output::error(&filtered);
            }
        }
        println!("[test]    BUILD FAILED {tag} ({wall})");
        std::io::stdout().flush().ok();
        return Ok(Outcome::BuildFailed);
    }

    // Zero tests ran: the name didn't match anything in this sweep. Print
    // an informational SKIP; the caller decides whether this is a real
    // error (all sweeps missed) or fine (feature-gated out of this one).
    if parsed.passed == 0 && parsed.failed == 0 {
        println!(
            "[test]    SKIP {tag} ({wall}) - no tests matched (likely feature-gated out of this sweep)"
        );
        std::io::stdout().flush().ok();
        return Ok(Outcome::NoMatch);
    }

    if let Some(fail) = parsed.failures.first() {
        let msg = fail.message.as_deref().unwrap_or("<no panic message>");
        let loc = fail.location.as_deref().unwrap_or("<unknown location>");
        println!("[test]    FAIL {tag} ({wall}) - {msg} @ {loc}");
        std::io::stdout().flush().ok();
        return Ok(Outcome::Fail);
    }

    if !run.captured.status.success() {
        println!(
            "[test]    FAIL {tag} ({wall}) - exit {:?}",
            run.captured.status.code()
        );
        std::io::stdout().flush().ok();
        return Ok(Outcome::Fail);
    }

    println!("[test]    PASS {tag} ({wall})");
    std::io::stdout().flush().ok();
    Ok(Outcome::Pass)
}

fn make_stdout_forwarder(raw: bool) -> impl FnMut(&str) + Send + 'static {
    // Suppress leading blanks and collapse runs of consecutive blanks into
    // one. Starts `true` so any blank line before we print anything is eaten
    // - that gets rid of the gap cargo leaves between "Finished ..." and the
    // test output.
    let mut prev_blank = true;
    move |line| {
        let want = raw || keep_stdout_line(line);
        if want {
            let is_blank = line.trim().is_empty();
            if !(is_blank && prev_blank) {
                let mut out = std::io::stdout().lock();
                writeln!(out, "{line}").ok();
                out.flush().ok();
                prev_blank = is_blank;
            }
        }
    }
}

fn make_stderr_forwarder(raw: bool) -> impl FnMut(&str) + Send + 'static {
    // Cargo emits compile noise (warnings, errors, progress) on stderr before
    // launching the test binary. The test's own eprintln! also lands here
    // once the binary runs. Split on the first "Running tests/..." line:
    // before it, filter aggressively; after it, pass through (it's the test
    // talking) - except further `Running <target> (<path>/deps/...)` lines,
    // which cargo re-emits between every test binary in the package and
    // which carry no signal (one such line per suite, ~10 in piners-runner).
    let mut in_test_phase = false;
    let mut in_compile_block = false;
    let mut prev_blank = true;
    move |line| {
        let want = if raw {
            true
        } else if is_cargo_running_line(line) {
            in_test_phase = true;
            false
        } else if in_test_phase {
            true
        } else {
            keep_stderr_compile_line(line, &mut in_compile_block)
        };
        if want {
            let is_blank = line.trim().is_empty();
            if !(is_blank && prev_blank) {
                let mut err = std::io::stderr().lock();
                writeln!(err, "{line}").ok();
                err.flush().ok();
                prev_blank = is_blank;
            }
        }
    }
}

/// Cargo's per-suite launch line: `Running unittests src/lib.rs
/// (<target>/debug/deps/foo-abc123)` or `Running tests/bar.rs (...)`.
/// Matched by shape (trailing parenthesized path under `deps/`) rather
/// than the bare "Running " prefix, so a test's own eprintln! that
/// happens to start with "Running " still passes through.
fn is_cargo_running_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Running ") && t.ends_with(')') && t.contains("/deps/")
}

/// Strip test-harness framing on stdout. The test's own `println!` output,
/// panic messages, and `failures:` sections pass through.
fn keep_stdout_line(line: &str) -> bool {
    if line.starts_with("running ") && line.contains(" test") {
        return false;
    }
    if line.starts_with("test ")
        && (line.ends_with(" ... ok")
            || line.ends_with(" ... FAILED")
            || line.ends_with(" ... ignored"))
    {
        return false;
    }
    if line.starts_with("test result:") {
        return false;
    }
    true
}

/// Strip cargo's compile-phase chatter on stderr: `Compiling`/`Finished`
/// progress, `warning:`/`error:` blocks (multi-line, terminated by a blank
/// line), and the `N warnings emitted` summary. Compile errors are still
/// shown via `filter_clippy` in the BUILD FAILED path.
fn keep_stderr_compile_line(line: &str, in_block: &mut bool) -> bool {
    let trimmed = line.trim_start();
    if *in_block {
        if trimmed.is_empty() {
            *in_block = false;
        }
        return false;
    }
    if trimmed.starts_with("warning:")
        || trimmed.starts_with("error:")
        || trimmed.starts_with("error[")
    {
        *in_block = true;
        return false;
    }
    if trimmed.starts_with("Compiling ")
        || trimmed.starts_with("Downloading ")
        || trimmed.starts_with("Checking ")
        || trimmed.starts_with("Finished ")
    {
        return false;
    }
    if trimmed.contains("generated") && trimmed.contains("warning") {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )]
    use super::*;
    use crate::config::{CheckEntry, TestConfig};

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
    fn stdout_filter_keeps_test_output() {
        assert!(keep_stdout_line("hello from test"));
        assert!(keep_stdout_line(""));
        assert!(keep_stdout_line("thread 'foo' panicked at tests/bar.rs:10:5:"));
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
    fn cargo_running_line_spares_test_output() {
        // A test's own eprintln! starting with "Running " lacks the
        // parenthesized deps path and must pass through.
        assert!(!is_cargo_running_line("Running 500 monte carlo paths"));
        assert!(!is_cargo_running_line("Running phase 2 (warmup)"));
        assert!(!is_cargo_running_line("   Compiling brokkr v0.1.0"));
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
        assert!(!keep_stderr_compile_line("  --> src/lib.rs:10:5", &mut in_block));
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
        assert!(!keep_stderr_compile_line("  --> src/lib.rs:1:1", &mut in_block));
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
        assert_eq!(sweeps[0].cargo_feature_args, vec!["--all-features"]);
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
            },
            CheckEntry {
                name: "consumer".into(),
                features: vec!["commands".into()],
                no_default_features: true,
                build_packages: vec!["pbfhogg-cli".into()],
            },
        ];
        let sweeps = decide_sweeps(None, &entries).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(
            sweeps[0].cargo_feature_args,
            vec!["--features", "test-hooks,linux-direct-io"]
        );
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert_eq!(sweeps[1].label, "consumer");
        assert_eq!(
            sweeps[1].cargo_feature_args,
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
            },
            CheckEntry {
                name: "consumer".into(),
                features: vec!["commands".into()],
                no_default_features: true,
                build_packages: vec!["pbfhogg-cli".into()],
            },
        ];
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all");
        assert_eq!(sweeps[0].build_packages, vec!["pbfhogg-cli"]);
        assert_eq!(sweeps[1].label, "consumer");
        assert_eq!(sweeps[1].build_packages, vec!["pbfhogg-cli"]);
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
        }];
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 1);
        assert_eq!(
            sweeps[0].env.get("BROKKR_TEST_PLATFORM").map(String::as_str),
            Some("1")
        );
        assert_eq!(sweeps[0].env.get("FOO").map(String::as_str), Some("bar"));
        // libtest filters dropped (per `brokkr test` design).
        assert!(sweeps[0].libtest_args.is_empty());
        assert!(sweeps[0].cargo_test_filters.is_empty());
        assert!(sweeps[0].name_filters.is_empty());
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
        }];
        let sweeps = decide_sweeps(Some(&test_cfg), &entries).unwrap();
        assert_eq!(sweeps.len(), 1);
        // Sweep struct only carries label / feature_args / build_packages -
        // any libtest filter from the profile is intentionally absent.
        assert_eq!(sweeps[0].label, "all");
    }

    #[test]
    fn count_listed_tests_counts_only_tests() {
        // libtest --list output: one `name: kind` line per entry, then a
        // trailing summary line we must not count.
        let listing = "\
foo::bar: test
foo::baz: test
benches::throughput: benchmark
3 tests, 1 benchmark
";
        assert_eq!(count_listed_tests(listing), 2);
    }

    #[test]
    fn count_listed_tests_zero_when_no_matches() {
        assert_eq!(count_listed_tests("0 tests, 0 benchmarks\n"), 0);
        assert_eq!(count_listed_tests(""), 0);
    }

    #[test]
    fn aggregate_exit_fails_on_any_fail() {
        let outcomes = [Outcome::Pass, Outcome::Fail];
        assert!(aggregate_exit(&outcomes, "f", "n").is_err());
    }

    #[test]
    fn aggregate_exit_fails_on_any_build_failed() {
        let outcomes = [Outcome::Pass, Outcome::BuildFailed];
        assert!(aggregate_exit(&outcomes, "f", "n").is_err());
    }

    #[test]
    fn aggregate_exit_fails_when_all_no_match() {
        let outcomes = [Outcome::NoMatch, Outcome::NoMatch];
        assert!(aggregate_exit(&outcomes, "f", "n").is_err());
    }

    #[test]
    fn aggregate_exit_passes_when_any_pass_with_no_match() {
        // The important case: feature-gated test passes in one sweep, SKIPs
        // in the consumer sweep. Exit code should be 0.
        let outcomes = [Outcome::Pass, Outcome::NoMatch];
        assert!(aggregate_exit(&outcomes, "f", "n").is_ok());
    }

    #[test]
    fn aggregate_exit_passes_on_all_pass() {
        let outcomes = [Outcome::Pass, Outcome::Pass];
        assert!(aggregate_exit(&outcomes, "f", "n").is_ok());
    }
}
