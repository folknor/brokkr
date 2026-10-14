//! Streaming output cleanup, repeat suppression, and reproduction labels.

use std::io::Write;

use crate::output;
use crate::test_runner;

use super::run::SweepPlan;

/// Shared across the `-N` repeat loop: failure signatures (panic
/// location, falling back to message) whose full streamed block has
/// already been shown once. Later runs failing with a seen signature
/// have their block suppressed - the FAIL footer alone carries the
/// per-run message.
#[derive(Default)]
pub(super) struct RepeatState {
    pub(super) seen_failures: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Reproduction lines already printed, so a harness failing on every
    /// iteration names its command once.
    pub(super) seen_hints: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl RepeatState {
    /// Record a reproduction line; true if this is its first occurrence.
    pub(super) fn first_hint(&self, line: &str) -> bool {
        self.seen_hints
            .lock()
            .map(|mut s| s.insert(line.to_owned()))
            .unwrap_or(true)
    }

    /// Record a signature; true if this is its first occurrence.
    pub(super) fn first_sighting(&self, sig: &str) -> bool {
        self.seen_failures
            .lock()
            .map(|mut s| s.insert(sig.to_owned()))
            .unwrap_or(true)
    }
}

/// Display destination for the streamed test output: live (run 1) or
/// buffered (repeats), where the buffer is flushed - or dropped for an
/// already-seen failure - once the outcome is known.
pub(super) type LineSink = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Print a failed run's invocation on the console: the reproduction line. A
/// passing run's goes to the run log only. Once per distinct line per command,
/// so a test failing on every `-N` iteration names its invocation once.
pub(super) fn show_reproduction(repeat_state: &RepeatState, failed: bool, invocation: &str) {
    if failed {
        show_failed_invocation(repeat_state, invocation);
    }
}

pub(super) fn show_failed_invocation(repeat_state: &RepeatState, invocation: &str) {
    if repeat_state.first_hint(invocation) {
        output::run_msg(invocation);
    }
}

/// The footer tag a lone failing test leads with: its full name, then the same
/// qualifiers as the filter-led tag, built from the parts rather than by
/// rewriting that tag. The filter joins the wall time when it differs.
pub(super) fn lone_test_tag(
    plan: &SweepPlan<'_>,
    harness: &str,
    n: u32,
    test: &str,
    wall: &str,
) -> String {
    let (pkg, name, sweep_label, repeat) =
        (plan.shape.pkg(), plan.name, plan.sweep_label, plan.repeat);
    let quals: Vec<&str> = sweep_label
        .into_iter()
        .chain(std::iter::once(harness))
        .collect();
    let run_n = if repeat > 1 {
        format!(" run {n}/{repeat}")
    } else {
        String::new()
    };
    let filter = if test == name {
        String::new()
    } else {
        format!("; filter: {name}")
    };
    format!(
        "{pkg}::{test} [{}]{run_n} ({wall}{filter})",
        quals.join(", ")
    )
}

/// Flush a repeat-run display buffer to stdout, or drop it when the
/// failure block was already shown by an earlier run. No-op in live
/// (run 1) mode where `sink` is `None`.
pub(super) fn flush_sink(sink: Option<LineSink>, suppress: bool) {
    let Some(s) = sink else { return };
    if suppress {
        return;
    }
    let lines = s
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default();
    if lines.is_empty() {
        return;
    }
    let mut out = std::io::stdout().lock();
    for l in &lines {
        writeln!(out, "{l}").ok();
    }
    out.flush().ok();
}

pub(super) fn make_stdout_forwarder(sink: Option<LineSink>) -> impl FnMut(&str) + Send + 'static {
    let mut cond = StdoutCondenser::new();
    move |line| {
        let lines = cond.next(line);
        if lines.is_empty() {
            return;
        }
        if let Some(s) = &sink {
            if let Ok(mut v) = s.lock() {
                v.extend(lines);
            }
            return;
        }
        let mut out = std::io::stdout().lock();
        for l in &lines {
            writeln!(out, "{l}").ok();
        }
        out.flush().ok();
    }
}

/// Display-side condenser for the streamed test stdout. Pure state
/// machine (returns the lines to print) so the framing rules are unit
/// testable without capturing the process's stdout.
///
/// Rules:
/// - framing lines rejected by `keep_stdout_line` are dropped;
/// - a `failures:` header is held back until a non-blank line follows.
///   Libtest prints the section twice (per-test output blocks, then the
///   name list) and under `--nocapture` the first is always empty -
///   consecutive headers collapse to one and a dangling empty header
///   is dropped entirely;
/// - leading blanks and runs of consecutive blanks collapse to one.
pub(super) struct StdoutCondenser {
    pub(super) prev_blank: bool,
    pub(super) pending_failures: bool,
}

impl StdoutCondenser {
    pub(super) fn new() -> Self {
        Self {
            // Starts `true` so any blank line before we print anything is
            // eaten - that gets rid of the gap cargo leaves between
            // "Finished ..." and the test output.
            prev_blank: true,
            pending_failures: false,
        }
    }

    pub(super) fn next(&mut self, line: &str) -> Vec<String> {
        if !keep_stdout_line(line) {
            return Vec::new();
        }
        if line.trim() == "failures:" {
            self.pending_failures = true;
            return Vec::new();
        }
        let is_blank = line.trim().is_empty();
        if self.pending_failures {
            if is_blank {
                return Vec::new();
            }
            self.pending_failures = false;
            self.prev_blank = false;
            return vec!["failures:".to_owned(), line.to_owned()];
        }
        if is_blank && self.prev_blank {
            return Vec::new();
        }
        self.prev_blank = is_blank;
        vec![line.to_owned()]
    }
}

/// `direct` starts the forwarder in the test phase: a directly executed
/// harness has no cargo in front of it, so no compile chatter and no
/// `Running ...` line will ever arrive to switch it over - without this its
/// whole stderr was filtered as compile output.
pub(super) fn make_stderr_forwarder(
    sink: Option<LineSink>,
    direct: bool,
) -> impl FnMut(&str) + Send + 'static {
    // Cargo emits compile noise (warnings, errors, progress) on stderr before
    // launching the test binary. The test's own eprintln! also lands here
    // once the binary runs. Split on the first "Running tests/..." line:
    // before it, filter aggressively; after it, pass through (it's the test
    // talking) - except further `Running <target> (<path>/deps/...)` lines,
    // which cargo re-emits between every test binary in the package and
    // which carry no signal (one such line per suite, ~10 in piners-runner),
    // and the test-phase noise lines (`note: run with RUST_BACKTRACE`,
    // cargo's `error: test failed, to rerun pass ...` - brokkr *is* the
    // rerun tool).
    let mut in_test_phase = direct;
    let mut in_compile_block = false;
    let mut prev_blank = true;
    move |line| {
        let want = if is_cargo_running_line(line) {
            in_test_phase = true;
            false
        } else if in_test_phase {
            !is_test_phase_noise(line)
        } else {
            keep_stderr_compile_line(line, &mut in_compile_block)
        };
        if want {
            let is_blank = line.trim().is_empty();
            if !(is_blank && prev_blank) {
                prev_blank = is_blank;
                if let Some(s) = &sink {
                    if let Ok(mut v) = s.lock() {
                        v.push(line.to_owned());
                    }
                    return;
                }
                let mut err = std::io::stderr().lock();
                writeln!(err, "{line}").ok();
                err.flush().ok();
            }
        }
    }
}

/// Pure-noise lines in the test phase of stderr: the backtrace hint
/// (brokkr's footer already carries the panic message/location) and
/// cargo's rerun suggestion (brokkr *is* the rerun tool).
pub(super) fn is_test_phase_noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("note: run with `RUST_BACKTRACE=1`")
        || t.starts_with("error: test failed, to rerun pass")
}

/// True for a genuine rustc compile error on stderr. Excludes cargo's
/// test-failure line (`error: test failed, to rerun pass ...`) - that
/// signals a *test* failure/crash, not a build failure, and a hard-crashing
/// test (SIGABRT/stack-overflow) that never prints `test result:` must not be
/// mislabeled `BUILD FAILED`. Cargo emits "test failed" (no "run"), so match
/// that substring, mirroring `is_test_phase_noise`.
pub(super) fn stderr_indicates_compile_error(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("error[") || (t.starts_with("error:") && !t.contains("test failed"))
}

/// Cargo's per-suite launch line: `Running unittests src/lib.rs
/// (<binary path>)` or `Running tests/bar.rs (<binary path>)`.
///
/// Matched by shape rather than the bare "Running " prefix, so a test's own
/// eprintln! that happens to start with "Running " still passes through. The
/// shape is the *source* target - `unittests <path>.rs` or `<path>.rs` -
/// followed by a parenthesized filesystem path, and deliberately says nothing
/// about where in the target dir that binary lives. It used to require
/// `/deps/`, which silently stopped matching when cargo moved compiled units
/// out of `target/<profile>/deps/` and under `target/<profile>/build/<pkg>/
/// <hash>/out/`. Because this predicate is what flips the stderr forwarder
/// into test-phase mode, a miss meant every line of a whole test run was
/// filtered as if it were still cargo compile chatter: the `Running` lines
/// leaked, `note: run with RUST_BACKTRACE=1` leaked, and a test's own
/// `warning:`-prefixed output was eaten as a compile-warning block.
pub(super) fn is_cargo_running_line(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix("Running ") else {
        return false;
    };
    let Some(inner) = rest.strip_suffix(')') else {
        return false;
    };
    let Some((target, path)) = inner.rsplit_once(" (") else {
        return false;
    };
    let target = target.strip_prefix("unittests ").unwrap_or(target);
    target.ends_with(".rs") && path.contains('/')
}

/// Strip test-harness framing on stdout. The test's own `println!` output,
/// panic messages, and `failures:` sections pass through.
pub(super) fn keep_stdout_line(line: &str) -> bool {
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
    // Under --nocapture the verdict arrives on its own line after the
    // test's output ("FAILED"/"ok"/"ignored", optional `<X.Xs>` suffix).
    // A test's own bare println!("ok") is indistinguishable and gets
    // dropped from display too - it's still in the captured buffer.
    if test_runner::is_bare_status_line(line) {
        return false;
    }
    true
}

/// Strip cargo's compile-phase chatter on stderr: `Compiling`/`Finished`/
/// `Blocking` progress, `warning:`/`error:` blocks (multi-line, terminated
/// by a blank line), the `N warnings emitted` summary, and rustc's trailing
/// `--explain` hints. Compile errors are still shown via
/// `filter_test_build_failure` in the BUILD FAILED path.
pub(super) fn keep_stderr_compile_line(line: &str, in_block: &mut bool) -> bool {
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
        || trimmed.starts_with("Blocking ")
    {
        return false;
    }
    if trimmed.contains("generated") && trimmed.contains("warning") {
        return false;
    }
    // Rustc's trailing hints after the error blocks: the `[error]` summary
    // already carries the error codes, so these would leak unprefixed.
    if trimmed.starts_with("Some errors have detailed explanations")
        || trimmed.starts_with("For more information about")
    {
        return false;
    }
    true
}
