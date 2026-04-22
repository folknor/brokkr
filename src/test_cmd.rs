//! `brokkr test <FILE> <NAME>` - single-test cargo runner.
//!
//! Runs exactly one named cargo test with release + host/check features,
//! `--include-ignored --nocapture --test-threads=1`. Streams the test's own
//! stdout/stderr live (filtering out cargo/test-harness framing noise), then
//! prints a `[test]` PASS/FAIL footer per sweep with wall time.
//!
//! Feature selection mirrors `brokkr check`: the default sweep is
//! `--all-features`; if `[check].consumer_features` is configured in
//! `brokkr.toml`, a second sweep runs with
//! `--no-default-features --features <consumer_features>`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{ChildStderr, ChildStdout};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crate::cargo_filter;
use crate::config::{CheckConfig, DevConfig};
use crate::error::DevError;
use crate::output;
use crate::project::Project;

struct Sweep {
    label: &'static str,
    feature_args: Vec<String>,
}

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
    file: &str,
    name: &str,
    repeat: u32,
    jobs: Option<u32>,
    raw: bool,
) -> Result<(), DevError> {
    let repeat = repeat.max(1);
    let sweeps = decide_sweeps(dev_config.check.as_ref());
    let multi = sweeps.len() > 1;

    let env: Vec<(&str, &str)> = match project {
        Project::Nidhogg => vec![("CARGO_TARGET_TMPDIR", "target/tmp")],
        _ => vec![],
    };

    let mut outcomes: Vec<Outcome> = Vec::new();

    for sweep in &sweeps {
        if multi {
            println!("[test]    sweep: {}", sweep.label);
        }
        for n in 1..=repeat {
            let mut args: Vec<String> = vec!["test".into(), "--release".into()];
            args.extend(sweep.feature_args.iter().cloned());
            if let Some(j) = jobs {
                args.push("-j".into());
                args.push(j.to_string());
            }
            args.push("--test".into());
            args.push(file.into());
            args.push(name.into());
            args.push("--".into());
            args.push("--include-ignored".into());
            args.push("--nocapture".into());
            args.push("--test-threads=1".into());

            let label = sweep.label;
            let tag = match (multi, repeat > 1) {
                (true, true) => format!("{file}::{name} [{label}] run {n}/{repeat}"),
                (true, false) => format!("{file}::{name} [{label}]"),
                (false, true) => format!("{file}::{name} run {n}/{repeat}"),
                (false, false) => format!("{file}::{name}"),
            };

            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            output::run_msg(&format!("cargo {}", arg_refs.join(" ")));

            let outcome = run_one(&arg_refs, project_root, &env, &tag, raw)?;
            outcomes.push(outcome);
        }
    }

    aggregate_exit(&outcomes, file, name)
}

fn aggregate_exit(outcomes: &[Outcome], file: &str, name: &str) -> Result<(), DevError> {
    let any_fail = outcomes
        .iter()
        .any(|o| matches!(o, Outcome::Fail | Outcome::BuildFailed));
    if any_fail {
        return Err(DevError::Build("test failed".into()));
    }
    let all_no_match = outcomes.iter().all(|o| *o == Outcome::NoMatch);
    if all_no_match {
        println!("[test]    no sweep matched `{file}::{name}` - check the file/name.");
        return Err(DevError::Build("no matching test".into()));
    }
    // At least one sweep passed; NoMatch in other sweeps is informational
    // (the test was feature-gated out of those sweeps).
    Ok(())
}

fn decide_sweeps(check_cfg: Option<&CheckConfig>) -> Vec<Sweep> {
    let mut sweeps = vec![Sweep {
        label: "all-features",
        feature_args: vec!["--all-features".into()],
    }];
    if let Some(cfg) = check_cfg
        && !cfg.consumer_features.is_empty()
    {
        sweeps.push(Sweep {
            label: "consumer",
            feature_args: vec![
                "--no-default-features".into(),
                "--features".into(),
                cfg.consumer_features.join(","),
            ],
        });
    }
    sweeps
}

/// Run one `cargo test` invocation. Prints the `[test]` footer and returns
/// the outcome. Err only on spawn failure.
fn run_one(
    args: &[&str],
    project_root: &Path,
    env: &[(&str, &str)],
    tag: &str,
    raw: bool,
) -> Result<Outcome, DevError> {
    let start = Instant::now();
    let mut child = output::spawn_captured("cargo", args, project_root, env)?;

    let stdout_pipe = child.stdout.take().expect("stdout piped");
    let stderr_pipe = child.stderr.take().expect("stderr piped");

    let stdout_buf = Arc::new(Mutex::new(Vec::<String>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<String>::new()));

    let stdout_buf_t = Arc::clone(&stdout_buf);
    let stdout_thread = thread::spawn(move || drain_stdout(stdout_pipe, raw, &stdout_buf_t));
    let stderr_buf_t = Arc::clone(&stderr_buf);
    let stderr_thread = thread::spawn(move || drain_stderr(stderr_pipe, raw, &stderr_buf_t));

    let status = child.wait().map_err(|e| DevError::Subprocess {
        program: "cargo".into(),
        code: None,
        stderr: e.to_string(),
    })?;

    // Drain threads finish when pipes close at child exit.
    stdout_thread.join().ok();
    stderr_thread.join().ok();
    let elapsed = start.elapsed();

    let stdout_lines = Arc::try_unwrap(stdout_buf)
        .map_err(|_| DevError::Build("stdout buffer still held".into()))?
        .into_inner()
        .map_err(|_| DevError::Build("stdout buffer poisoned".into()))?;
    let stderr_lines = Arc::try_unwrap(stderr_buf)
        .map_err(|_| DevError::Build("stderr buffer still held".into()))?
        .into_inner()
        .map_err(|_| DevError::Build("stderr buffer poisoned".into()))?;

    let line_refs: Vec<&str> = stdout_lines.iter().map(String::as_str).collect();
    let parsed = cargo_filter::parse_test_output(&line_refs);

    let has_test_result = stdout_lines.iter().any(|l| l.starts_with("test result:"));
    let has_compile_error = stderr_lines.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("error[") || (t.starts_with("error:") && !t.contains("test run failed"))
    });

    let wall = format!("{:.2}s", elapsed.as_secs_f64());

    if !has_test_result && has_compile_error {
        if !raw {
            let stderr_joined = stderr_lines.join("\n");
            let filtered = cargo_filter::filter_clippy(&stderr_joined);
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

    if !status.success() {
        println!("[test]    FAIL {tag} ({wall}) - exit {:?}", status.code());
        std::io::stdout().flush().ok();
        return Ok(Outcome::Fail);
    }

    println!("[test]    PASS {tag} ({wall})");
    std::io::stdout().flush().ok();
    Ok(Outcome::Pass)
}

fn drain_stdout(pipe: ChildStdout, raw: bool, buf: &Mutex<Vec<String>>) {
    let reader = BufReader::new(pipe);
    let mut out = std::io::stdout().lock();
    // Suppress leading blanks and collapse runs of consecutive blanks into
    // one. Starts `true` so any blank line before we print anything is eaten
    // - that gets rid of the gap cargo leaves between "Finished ..." and the
    // test output.
    let mut prev_blank = true;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let want = raw || keep_stdout_line(&line);
        if want {
            let is_blank = line.trim().is_empty();
            if !(is_blank && prev_blank) {
                writeln!(out, "{line}").ok();
                out.flush().ok();
                prev_blank = is_blank;
            }
        }
        if let Ok(mut v) = buf.lock() {
            v.push(line);
        }
    }
}

fn drain_stderr(pipe: ChildStderr, raw: bool, buf: &Mutex<Vec<String>>) {
    let reader = BufReader::new(pipe);
    let mut err = std::io::stderr().lock();
    // Cargo emits compile noise (warnings, errors, progress) on stderr before
    // launching the test binary. The test's own eprintln! also lands here
    // once the binary runs. Split on the "Running tests/..." line: before it,
    // filter aggressively; after it, pass through (it's the test talking).
    let mut in_test_phase = false;
    let mut in_compile_block = false;
    let mut prev_blank = true;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let want = if raw || in_test_phase {
            true
        } else if line.trim_start().starts_with("Running ") {
            in_test_phase = true;
            false
        } else {
            keep_stderr_compile_line(&line, &mut in_compile_block)
        };
        if want {
            let is_blank = line.trim().is_empty();
            if !(is_blank && prev_blank) {
                writeln!(err, "{line}").ok();
                err.flush().ok();
                prev_blank = is_blank;
            }
        }
        if let Ok(mut v) = buf.lock() {
            v.push(line);
        }
    }
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
    use super::*;

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
    fn decide_sweeps_defaults_to_all_features() {
        let sweeps = decide_sweeps(None);
        assert_eq!(sweeps.len(), 1);
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[0].feature_args, vec!["--all-features"]);
    }

    #[test]
    fn decide_sweeps_adds_consumer_when_configured() {
        let cfg = CheckConfig {
            consumer_features: vec!["commands".into(), "foo".into()],
        };
        let sweeps = decide_sweeps(Some(&cfg));
        assert_eq!(sweeps.len(), 2);
        assert_eq!(sweeps[0].label, "all-features");
        assert_eq!(sweeps[1].label, "consumer");
        assert_eq!(
            sweeps[1].feature_args,
            vec!["--no-default-features", "--features", "commands,foo"]
        );
    }

    #[test]
    fn decide_sweeps_skips_consumer_when_features_empty() {
        let cfg = CheckConfig {
            consumer_features: vec![],
        };
        let sweeps = decide_sweeps(Some(&cfg));
        assert_eq!(sweeps.len(), 1);
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
