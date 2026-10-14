//! Libtest output parsing and compact test failure rendering.
//!
//! This belongs with test execution: it interprets harness suites, panic
//! output, and cargo's test-process failure wrapper. Compiler diagnostics use
//! the shared cargo JSON reader and [`crate::diagnostic::Diagnostic`].

use std::path::Path;

use crate::rustc_text::{self, BlockKind, Level};

// --- Shared test output parser ---

/// Parse `"N <label>"` from a test result line.
fn parse_count(line: &str, label: &str) -> Option<usize> {
    let idx = line.find(label)?;
    let before = line[..idx].trim_end();
    let num_str = before.rsplit(|c: char| !c.is_ascii_digit()).next()?;
    num_str.parse().ok()
}

/// Parse "finished in N.NNs" from a test result line.
fn parse_duration(line: &str) -> Option<f64> {
    let marker = "finished in ";
    let idx = line.find(marker)?;
    let rest = &line[idx + marker.len()..];
    let end = rest.find('s')?;
    rest[..end].parse().ok()
}

/// A single parsed test failure.
#[derive(Clone)]
pub(crate) struct ParsedTestFailure {
    pub name: String,
    pub location: Option<String>,
    pub message: Option<String>,
}

/// Aggregated test results from one or more test suites.
pub(crate) struct ParsedTestResults {
    pub failures: Vec<ParsedTestFailure>,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub filtered_out: usize,
    /// Number of suites whose *terminal* summary was observed.
    ///
    /// Terminal, not started: a process can report suite one in full and then
    /// die inside suite two, and a count of started suites would make that look
    /// finished. See [`Completeness`] for the part this number cannot carry.
    pub suites: usize,
    pub duration: Option<f64>,
    /// Whether the numbers above describe a run that actually finished
    /// reporting.
    ///
    /// Every count here defaults to zero, and zero is indistinguishable from
    /// "the run never said". That ambiguity is how a crashed harness reads as a
    /// clean empty run: `passed == 0 && failed == 0` looks exactly like "the
    /// filter matched nothing". Callers must consult this before drawing a
    /// conclusion from a zero.
    pub completeness: Completeness,
}

/// Whether a parsed run finished reporting, and if not, why not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Completeness {
    /// Every suite that started also reported a terminal summary.
    #[default]
    Complete,
    /// The stream stopped, or contradicted itself, before every started suite
    /// reported. The counts are whatever was observed up to that point - useful
    /// for a report, never sufficient for a verdict.
    Incomplete { reason: String },
}

impl ParsedTestResults {
    /// Whether these counts may be read as a finished run.
    pub fn is_complete(&self) -> bool {
        matches!(self.completeness, Completeness::Complete)
    }

    /// Tests actually accounted for. Meaningless unless [`Self::is_complete`].
    pub fn accounted(&self) -> usize {
        self.passed + self.failed + self.ignored
    }
}

/// Parse cargo test stdout into structured results.
///
/// Handles each suite's two `failures:` sections (detail then name-list),
/// extracts panic locations and messages, and aggregates `test result:`
/// summary lines. The stream may carry many suites back to back - one per
/// test binary - and the section state resets at every `running N tests`,
/// so a failure in the second and later binaries is reported too.
/// Works on any iterator of lines - callers can pre-filter JSON lines out
/// before passing non-JSON lines here.
pub(crate) fn parse_test_output(lines: &[&str]) -> ParsedTestResults {
    parse_test_output_with_stderr(lines, &[])
}

/// Whether every suite that announced itself also reported a summary.
///
/// A suite that started and never summarised is the shape a crashed or killed
/// harness leaves behind, and it is exactly the shape that reads as a clean empty
/// run when only the counts are consulted: every total stays zero because nothing
/// reported them, which is indistinguishable from "the filter matched nothing".
/// Naming the difference is the whole point of [`Completeness`].
fn judge_completeness(started: usize, summarised: usize) -> Completeness {
    // Strict inequality both ways. Only rejecting `started > summarised` left two
    // holes: a lone forged or malformed `test result:` line counted as a complete
    // run, and a summary printed by test output could numerically compensate for
    // a suite summary that never arrived. If the two do not agree exactly, the
    // stream is not describing a run this parser can vouch for.
    if started != summarised {
        return Completeness::Incomplete {
            reason: format!(
                "{started} suite(s) announced themselves but {summarised} reported a summary - \
                 the stream is inconsistent, so its counts describe no run"
            ),
        };
    }
    // Zero of each is not agreement, it is silence: no suite ever announced
    // itself and none summarised, which is what unrecognised output, an empty
    // stream, or a harness that never started looks like. Complete would make
    // every count below a fact about a run that was never observed.
    if started == 0 {
        return Completeness::Incomplete {
            reason: "no test suite was observed at all - the output carries no libtest run".into(),
        };
    }
    Completeness::Complete
}

/// Like [`parse_test_output`], but also scans stderr for inline panic
/// lines. Rust panics print to *stderr*, so under `--nocapture` (no
/// captured `---- name stdout ----` blocks) the failure location and
/// message are only recoverable from there. The `failures:` name list
/// on stdout still vets which panics belong to actual failures.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // state-machine parser - splitting hurts clarity
pub(crate) fn parse_test_output_with_stderr(
    lines: &[&str],
    stderr_lines: &[&str],
) -> ParsedTestResults {
    let mut failures: Vec<ParsedTestFailure> = Vec::new();
    let mut summary_lines: Vec<String> = Vec::new();
    let mut in_failure_detail = false;
    let mut seen_failure_section = false;
    let mut current_name = String::new();
    let mut current_panic_loc = String::new();
    let mut current_panic_msg = String::new();
    // Under --nocapture there are no `---- name stdout ----` blocks (the
    // failure detail section is empty), so the loop below never collects
    // anything. The panic instead streams inline as
    // `thread '<name>' panicked at <loc>:` followed by the message line.
    // Collect those as a fallback; the thread name is the test name under
    // --test-threads=1.
    let mut inline_panics = InlinePanicCollector::default();
    // Names from the `failures:` name-list section, used to vet the
    // inline panics: a passing test may legitimately print panic lines
    // (`catch_unwind`), and only listed names actually failed.
    let mut failed_names: Vec<String> = Vec::new();
    let mut in_name_list = false;
    // Suites that announced themselves. Compared against the number that
    // summarised, to tell a finished run from a truncated one.
    let mut started_suites: usize = 0;
    // `test NAME ... FAILED` verdicts of the suite currently open, cleared at
    // every suite boundary and summary. What survives to the end belongs to a
    // suite that never summarised - a harness that crashed after failing some
    // tests. Its failures block was never rendered (the reconstructor holds it
    // for the summary), so these verdicts are the only record that those tests
    // failed. Consulted only for that unclosed suite: a verdict-shaped line a
    // passing test prints cannot add a failure to a run that finished.
    let mut open_suite_verdicts: Vec<String> = Vec::new();

    for line in lines {
        // Start of a new test binary. Cargo concatenates every suite's
        // output into one stream and each suite prints its own *pair* of
        // `failures:` sections (detail, then name list), so the section
        // state is per-suite. Latching `seen_failure_section` across the
        // whole stream made suite 2's detail block parse as a name list,
        // and every failure after the first failing binary's was dropped
        // from the rendered list - the exit code stayed honest, so a red
        // run silently under-reported which tests failed.
        //
        // Guarded on `!in_failure_detail`: a captured `---- x stdout ----`
        // block can contain the test's own output, `running ...` included,
        // and that is not a suite boundary.
        let suite_start = line.starts_with("running ") && !in_failure_detail;
        if suite_start {
            started_suites += 1;
            flush_parsed_failure(
                &current_name,
                &current_panic_loc,
                &current_panic_msg,
                &mut failures,
            );
            current_name.clear();
            current_panic_loc.clear();
            current_panic_msg.clear();
            seen_failure_section = false;
            in_name_list = false;
            open_suite_verdicts.clear();
        }
        if line.starts_with("test result:") {
            open_suite_verdicts.clear();
        } else if !in_failure_detail
            && let Some(name) = line
                .strip_prefix("test ")
                .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            open_suite_verdicts.push(name.to_owned());
        }

        let trimmed = line.trim_start();
        if suite_start
            || line.starts_with("running ")
            || trimmed.starts_with("Compiling")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Finished")
            || (line.starts_with("test ") && line.ends_with("... ok"))
        {
            continue;
        }

        if *line == "failures:" {
            if !seen_failure_section {
                in_failure_detail = true;
                seen_failure_section = true;
            } else {
                in_failure_detail = false;
                in_name_list = true;
                flush_parsed_failure(
                    &current_name,
                    &current_panic_loc,
                    &current_panic_msg,
                    &mut failures,
                );
                current_name.clear();
                current_panic_loc.clear();
                current_panic_msg.clear();
            }
            continue;
        }

        if in_name_list {
            let t = line.trim();
            if t.is_empty() || line.starts_with("test result:") {
                in_name_list = false;
            } else {
                failed_names.push(t.to_string());
                continue;
            }
        }

        if in_failure_detail {
            if line.starts_with("test result:") {
                in_failure_detail = false;
                flush_parsed_failure(
                    &current_name,
                    &current_panic_loc,
                    &current_panic_msg,
                    &mut failures,
                );
                current_name.clear();
                current_panic_loc.clear();
                current_panic_msg.clear();
                summary_lines.push(line.to_string());
            } else if line.starts_with("---- ") && line.ends_with(" stdout ----") {
                flush_parsed_failure(
                    &current_name,
                    &current_panic_loc,
                    &current_panic_msg,
                    &mut failures,
                );
                current_name = line
                    .strip_prefix("---- ")
                    .unwrap_or("")
                    .strip_suffix(" stdout ----")
                    .unwrap_or("")
                    .to_string();
                current_panic_loc.clear();
                current_panic_msg.clear();
            } else if line.contains("panicked at ") {
                if let Some(idx) = line.find("panicked at ") {
                    let (loc, msg) = parse_panicked_at(&line[idx + "panicked at ".len()..]);
                    current_panic_loc = loc;
                    current_panic_msg = msg.unwrap_or_default();
                }
            } else if current_panic_msg.is_empty()
                && !current_panic_loc.is_empty()
                && !line.trim().is_empty()
            {
                current_panic_msg = line.trim().to_string();
            }
        }

        if !in_failure_detail {
            inline_panics.observe(line);
        }

        if !in_failure_detail && line.starts_with("test result:") {
            summary_lines.push(line.to_string());
        }
    }

    flush_parsed_failure(
        &current_name,
        &current_panic_loc,
        &current_panic_msg,
        &mut failures,
    );

    // Aggregate summary lines.
    let mut passed: usize = 0;
    let mut failed: usize = 0;
    let mut ignored: usize = 0;
    let mut filtered_out: usize = 0;
    let mut duration: f64 = 0.0;
    let mut has_duration = false;
    let mut suites: usize = 0;

    for line in &summary_lines {
        suites += 1;
        if let Some(n) = parse_count(line, "passed") {
            passed += n;
        }
        if let Some(n) = parse_count(line, "failed") {
            failed += n;
        }
        if let Some(n) = parse_count(line, "ignored") {
            ignored += n;
        }
        if let Some(n) = parse_count(line, "filtered out") {
            filtered_out += n;
        }
        if let Some(d) = parse_duration(line) {
            duration += d;
            has_duration = true;
        }
    }

    // The suite still open at the end never summarised, so its failed tests
    // reached no name list: its verdicts stand in for one. See
    // `open_suite_verdicts` for why nothing else consults them.
    let unclosed_failures = started_suites > suites && !open_suite_verdicts.is_empty();
    if unclosed_failures {
        for name in open_suite_verdicts {
            if !failed_names.contains(&name) {
                failed_names.push(name);
            }
        }
    }

    // --nocapture fallback: no `---- name stdout ----` blocks, so the
    // detail section yielded nothing, but the stream carried inline
    // panics. Gated on a failure being on record - a summary's count or an
    // unclosed suite's verdict - because a *passing* test can print
    // panic lines too (`catch_unwind`). When the name-list section is
    // present, vet by name and take the *last* panic per failing test
    // (a caught panic may precede the fatal one); fall back to the raw
    // inline list only if no name matched (thread-name mismatch).
    if failures.is_empty() && (failed > 0 || unclosed_failures) {
        // Panic lines live on stderr; run them through their own
        // collector (the message line follows its panic line within the
        // same stream) and pool with any stdout-side hits before vetting.
        let mut stderr_panics = InlinePanicCollector::default();
        for line in stderr_lines {
            stderr_panics.observe(line);
        }
        inline_panics.panics.extend(stderr_panics.panics);
        failures = inline_panics.into_failures(&failed_names);
    }

    // The detail sections are best-effort - a suite can fail a test whose
    // `---- name stdout ----` block the parser never saw (an aborted suite
    // truncating the stream, a detail block nested inside another test's
    // captured output, a libtest format we don't model). The `failures:`
    // NAME LIST is the authoritative roster, so anything it names that the
    // detail pass missed is appended bare rather than dropped. Filtering
    // failing tests out of a failure report is how a red run under-reports
    // which tests are down while the exit code stays honest.
    complete_roster(&mut failures, &failed_names);

    let completeness = judge_completeness(started_suites, suites);

    ParsedTestResults {
        failures,
        passed,
        failed,
        ignored,
        filtered_out,
        suites,
        duration: if has_duration { Some(duration) } else { None },
        completeness,
    }
}

/// Append every `failures:`-name-list entry the detail pass did not already
/// account for, as a bare name with no location or message.
///
/// Skipped when the `--nocapture` fallback fell back to raw thread names
/// (nothing it produced is on the roster): there the existing entries describe
/// the same failures under a different naming, so appending the roster on top
/// would double-count them.
fn complete_roster(failures: &mut Vec<ParsedTestFailure>, failed_names: &[String]) {
    let comparable = failures.is_empty()
        || failures
            .iter()
            .any(|f| failed_names.iter().any(|n| n == &f.name));
    if !comparable {
        return;
    }
    for name in failed_names {
        if !failures.iter().any(|f| &f.name == name) {
            failures.push(ParsedTestFailure {
                name: name.clone(),
                location: None,
                message: None,
            });
        }
    }
}

/// Collects `thread '<name>' panicked at <loc>:` lines streamed outside
/// the failure-detail section, plus the message line that follows in the
/// rustc 1.73+ format. Feeds the --nocapture fallback in
/// [`parse_test_output`].
#[derive(Default)]
struct InlinePanicCollector {
    panics: Vec<ParsedTestFailure>,
    awaiting_msg: bool,
}

impl InlinePanicCollector {
    fn observe(&mut self, line: &str) {
        if let Some((name, loc, msg)) = parse_inline_panic_line(line) {
            self.awaiting_msg = msg.is_none();
            self.panics.push(ParsedTestFailure {
                name,
                location: Some(loc),
                message: msg,
            });
            return;
        }
        if self.awaiting_msg && !line.trim().is_empty() {
            // First non-blank line after `panicked at <loc>:` is the
            // panic message - unless it's libtest's own verdict line
            // (a panic with an empty message glues straight to it).
            self.awaiting_msg = false;
            let trimmed = line.trim();
            if !matches!(trimmed, "ok" | "FAILED" | "ignored")
                && let Some(last) = self.panics.last_mut()
            {
                last.message = Some(trimmed.to_string());
            }
        }
    }

    /// Resolve into the failure list: vet by the `failures:` name list
    /// when present, taking the *last* panic per failing test (a caught
    /// panic may precede the fatal one); fall back to the raw inline
    /// list only if no name matched (thread-name mismatch).
    fn into_failures(self, failed_names: &[String]) -> Vec<ParsedTestFailure> {
        let vetted: Vec<ParsedTestFailure> = failed_names
            .iter()
            .filter_map(|n| self.panics.iter().rfind(|p| &p.name == n).cloned())
            .collect();
        if vetted.is_empty() {
            self.panics
        } else {
            vetted
        }
    }
}

/// Parse the tail of a `panicked at ` line into (location, message).
///
/// Two rustc shapes:
/// - pre-1.73: `panicked at 'msg', src/lib.rs:15:9` - message inline,
///   location after the closing quote;
/// - 1.73+: `panicked at src/lib.rs:15:9:` - location only, message on
///   the following line(s).
fn parse_panicked_at(rest: &str) -> (String, Option<String>) {
    let rest = rest.trim_end().trim_end_matches(':');
    if let Some(body) = rest.strip_prefix('\'')
        && let Some(end_quote) = body.find('\'')
    {
        let msg = body[..end_quote].to_string();
        let loc = body[end_quote + 1..].trim_start_matches(", ").to_string();
        return (loc, Some(msg));
    }
    (rest.to_string(), None)
}

/// Parse a streamed (non-detail-section) panic line:
/// `thread '<name>' panicked at <loc>:` - rustc 1.73+ also inserts the
/// thread id: `thread '<name>' (12345) panicked at <loc>:`. Returns
/// (test name, location, inline message if the pre-1.73 shape).
fn parse_inline_panic_line(line: &str) -> Option<(String, String, Option<String>)> {
    let rest = line.strip_prefix("thread '")?;
    let (name, after) = rest.split_once('\'')?;
    if name.is_empty() {
        return None;
    }
    let idx = after.find("panicked at ")?;
    let (loc, msg) = parse_panicked_at(&after[idx + "panicked at ".len()..]);
    if loc.is_empty() {
        return None;
    }
    Some((name.to_string(), loc, msg))
}

fn flush_parsed_failure(
    name: &str,
    panic_loc: &str,
    panic_msg: &str,
    failures: &mut Vec<ParsedTestFailure>,
) {
    if name.is_empty() {
        return;
    }
    failures.push(ParsedTestFailure {
        name: name.to_string(),
        location: if panic_loc.is_empty() {
            None
        } else {
            Some(panic_loc.to_string())
        },
        message: if panic_msg.is_empty() {
            None
        } else {
            Some(panic_msg.to_string())
        },
    });
}

/// Filter cargo test output - one line per failure, compact summary on success.
///
/// On success:
/// ```text
/// cargo test: 137 passed (4 suites, 1.45s)
/// ```
///
/// On failure:
/// ```text
/// cargo test: 2 failures, 14 passed
///   FAILED foo::test_b src/lib.rs:15:9 assertion `left == right` failed
///   FAILED foo::test_c src/lib.rs:20:9 something went wrong
/// ```
///
/// On compilation error (no test results):
/// Falls back to `cargo_filter::filter_test_build_failure` to show the build errors.
pub(crate) fn filter_test(stdout: &str, stderr: &str) -> String {
    // Compilation failure - no test results, just build errors.
    let has_test_result = stdout.lines().any(|l| l.starts_with("test result:"));
    let has_compile_error = rustc_text::rustc_blocks(stderr).iter().any(|block| {
        block.level == Level::Error
            && !block
                .text
                .lines()
                .next()
                .unwrap_or("")
                .contains("test run failed")
            && matches!(
                rustc_text::classify_block(block),
                BlockKind::Finding | BlockKind::FailureSummary
            )
    });

    if !has_test_result && has_compile_error {
        return crate::cargo_filter::filter_test_build_failure(stderr);
    }

    let lines: Vec<&str> = stdout.lines().collect();
    let parsed = parse_test_output(&lines);

    // `filter_test` only runs on the failure path (cargo exited
    // non-zero). So even when libtest recorded every test as passing,
    // *something* failed - the evidence is a harness/process-level
    // error that never produced a per-test `FAILED` line: a destructor
    // that double-panics into SIGABRT, an at-exit nonzero, cargo's own
    // `test failed` / `process didn't exit successfully` wrapper. Those
    // live only in stderr, so the compact "all passed" summary would
    // bury the one thing the user needs to see.
    let harness_failure = stderr_has_harness_failure(stderr);

    // All passed by libtest's count - compact summary, unless the
    // process aborted around it (in which case surface the abort too).
    //
    // Gated on the COUNT, not on the roster: an empty failure list with a
    // non-zero `failed` tally means the parser could not attribute the
    // failures, not that there were none, and routing that to the compact
    // "all passed" summary is how a red run reads as green-ish.
    if parsed.failures.is_empty() && parsed.failed == 0 && parsed.suites > 0 {
        if harness_failure {
            return format!(
                "{} - but the test process exited non-zero:\n{}",
                format_test_summary(&parsed),
                harness_failure_excerpt(stderr),
            );
        }
        return format_test_summary(&parsed);
    }

    // Parser found no failures. Surface a FAILED line the parser missed, or
    // a harness-level abort - never silently succeed. Skipped when libtest
    // counted failures: those belong in the failure report below, which
    // states the tally and the unattributed shortfall.
    if parsed.failures.is_empty() && parsed.failed == 0 {
        let has_failed = stdout.lines().any(|l| l.contains("FAILED"));
        if has_failed {
            let mut raw = String::new();
            if !stderr.is_empty() {
                raw.push_str(stderr);
            }
            if !stdout.is_empty() {
                if !raw.is_empty() {
                    raw.push('\n');
                }
                raw.push_str(stdout);
            }
            return raw;
        }
        if harness_failure {
            return format!(
                "cargo test: the test process exited non-zero:\n{}",
                harness_failure_excerpt(stderr),
            );
        }
    }

    // Failures present - format as one-liners. A harness killed by a signal
    // is named beside them: its failures before the crash are on the roster,
    // but the crash itself is a verdict line of cargo's on stderr that no
    // roster carries, and a list that omits it reads as "these tests failed"
    // when a whole harness died mid-run.
    let mut out = format_test_failures(&parsed, stderr);
    for crash in harness_crashes(stdout, stderr) {
        out.push_str("\n  ");
        out.push_str(&crash);
    }
    out
}

/// The harnesses a run lost to a signal, from cargo's own "process didn't
/// exit successfully" lines whose tail is `(signal: N, NAME: ...)` - an exit
/// status is an ordinary failing harness and stays out - followed by the
/// suspect line the isolated runner writes for a suite it had to close.
fn harness_crashes(stdout: &str, stderr: &str) -> Vec<String> {
    let mut out: Vec<String> = stderr
        .lines()
        .filter_map(|l| {
            let rest = l
                .trim_start()
                .strip_prefix("process didn't exit successfully: `")?;
            let (cmd, tail) = rest.split_once('`')?;
            let signal = tail.trim().strip_prefix("(signal: ")?.strip_suffix(')')?;
            let mut words = cmd.split_whitespace();
            let mut exe = words.next()?;
            // Under harness isolation cargo names its runner - brokkr, then
            // the shim marker (`test_runner::harness_shim::RUNNER_ARG`) -
            // and the harness comes after it.
            if words.next() == Some("__brokkr_harness_shim") {
                exe = words.next()?;
            }
            let name = Path::new(exe).file_name()?.to_str()?;
            // Cargo names test executables `<target>-<hash>`.
            let target = name.rsplit_once('-').map_or(name, |(t, _)| t);
            Some(format!("CRASHED {target} (signal: {signal})"))
        })
        .collect();
    out.extend(
        stdout
            .lines()
            .filter(|l| l.starts_with("brokkr: this test harness ended with its suite still open"))
            .map(str::to_owned),
    );
    out
}

/// True when stderr carries a cargo/libtest harness-level failure that
/// produced no per-test `FAILED` line - the shapes that survive an
/// otherwise all-passing libtest count: a process that aborts during
/// teardown (`panic in a destructor` -> SIGABRT), a non-unwinding
/// panic, or cargo's `test failed` / `process didn't exit successfully`
/// wrapper. `filter_test` only runs when cargo already exited non-zero,
/// so these are the failure-without-a-FAILED-line cases.
fn stderr_has_harness_failure(stderr: &str) -> bool {
    stderr.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("error: test failed")
            || t.starts_with("process didn't exit successfully")
            || t.contains("(signal:")
            || t.contains("non-unwinding panic")
            || t.contains("panic in a destructor")
    })
}

/// The meaningful lines of a harness-level failure, with cargo's build
/// and per-suite progress chatter (`Compiling`, `Running tests/...`,
/// `Finished`) stripped so only the panic / abort / `Caused by` lines
/// remain. Leading and trailing blank lines are trimmed.
fn harness_failure_excerpt(stderr: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in stderr.lines() {
        let t = line.trim_start();
        if is_noise(t) || t.starts_with("Running ") {
            continue;
        }
        out.push(line);
    }
    while out.first().is_some_and(|l| l.trim().is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Format parsed test results as a compact summary line.
fn format_test_summary(parsed: &ParsedTestResults) -> String {
    let mut parts = vec![format!("{} passed", parsed.passed)];
    if parsed.failed > 0 {
        parts.push(format!("{} failed", parsed.failed));
    }
    if parsed.ignored > 0 {
        parts.push(format!("{} ignored", parsed.ignored));
    }
    if parsed.filtered_out > 0 {
        parts.push(format!("{} filtered out", parsed.filtered_out));
    }

    let counts = parts.join(", ");
    let suite_text = if parsed.suites == 1 {
        "1 suite".to_string()
    } else {
        format!("{} suites", parsed.suites)
    };

    if let Some(d) = parsed.duration {
        format!("cargo test: {counts} ({suite_text}, {d:.2}s)")
    } else {
        format!("cargo test: {counts} ({suite_text})")
    }
}

/// Format parsed test failures as one-liner text output. `stderr` is cargo's,
/// shown only when some failures could not be attributed to a name.
fn format_test_failures(parsed: &ParsedTestResults, stderr: &str) -> String {
    // The headline count is the larger of the parsed roster and libtest's own
    // tally - never the roster alone, which is what let "1 failure" head a run
    // with two tests down.
    let count = parsed.failed.max(parsed.failures.len());
    let mut result = format!(
        "cargo test: {count} failure{}\n",
        if count == 1 { "" } else { "s" }
    );
    for f in &parsed.failures {
        result.push_str("  FAILED ");
        result.push_str(&f.name);
        if let Some(loc) = &f.location {
            result.push(' ');
            result.push_str(loc);
        }
        if let Some(msg) = &f.message {
            result.push(' ');
            result.push_str(msg);
        }
        result.push('\n');
    }

    // libtest's own `test result:` tally is the ground truth for HOW MANY
    // failed; the list above is only as complete as the output we could
    // parse. When they disagree the tally wins and the shortfall is stated,
    // so a reader never mistakes a short list for the whole story.
    //
    // There is no unfiltered mode to point at (the `--raw` flag this once
    // named is gone), so the evidence rides along instead: cargo's stderr
    // names the failing binary (`to rerun pass `-p X --test Y``), which is
    // the one handle a reader has on failures libtest never named.
    if parsed.failed > parsed.failures.len() {
        result.push_str(&format!(
            "  (libtest counted {} failed; {} could not be attributed to a test name)\n",
            parsed.failed,
            parsed.failed - parsed.failures.len(),
        ));
        let excerpt = harness_failure_excerpt(stderr);
        if !excerpt.is_empty() {
            for line in excerpt.lines() {
                result.push_str("  ");
                result.push_str(line);
                result.push('\n');
            }
        }
    }

    result.trim_end().to_string()
}

// --- helpers ---

fn is_noise(trimmed: &str) -> bool {
    rustc_text::classify_line(trimmed) == BlockKind::Progress
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
    use super::*;

    #[test]
    fn test_all_pass_single_suite() {
        let stdout = "\
running 15 tests
test utils::test_a ... ok
test utils::test_b ... ok
test utils::test_c ... ok

test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let result = filter_test(stdout, "");
        assert_eq!(result, "cargo test: 15 passed (1 suite, 0.01s)");
    }

    #[test]
    fn test_all_pass_multi_suite() {
        let stdout = "\
running 50 tests
test result: ok. 50 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.45s

running 30 tests
test result: ok. 30 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.30s
";
        let result = filter_test(stdout, "");
        assert_eq!(result, "cargo test: 80 passed (2 suites, 0.75s)");
    }

    #[test]
    fn test_failure_one_liner() {
        let stdout = "\
running 5 tests
test foo::test_a ... ok
test foo::test_b ... FAILED
test foo::test_c ... ok

failures:

---- foo::test_b stdout ----
thread 'foo::test_b' panicked at 'assert_eq!(1, 2)', src/lib.rs:15:9

failures:
    foo::test_b

test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";
        let result = filter_test(stdout, "");
        assert!(result.contains("1 failure"), "got: {result}");
        assert!(result.contains("FAILED foo::test_b"), "got: {result}");
        // Should be one line per failure, not multi-line.
        let failure_lines: Vec<&str> = result
            .lines()
            .filter(|l| l.starts_with("  FAILED"))
            .collect();
        assert_eq!(failure_lines.len(), 1, "got: {result}");
    }

    /// A suite that started and never summarised must not read as a finished
    /// run. Its zeros mean "never reported", and a caller that cannot tell the
    /// difference calls a crashed harness an empty test selection.
    #[test]
    fn a_truncated_stream_is_incomplete_not_an_empty_run() {
        let lines = ["", "running 3 tests", "test a::one ... ok"];
        let parsed = parse_test_output(&lines);
        assert!(
            !parsed.is_complete(),
            "a suite with no summary is not complete"
        );
        assert_eq!(
            parsed.accounted(),
            0,
            "nothing was reported, so nothing is accounted"
        );
        match &parsed.completeness {
            Completeness::Incomplete { reason } => {
                assert!(reason.contains("announced"), "{reason}");
            }
            Completeness::Complete => panic!("expected Incomplete"),
        }
    }

    /// A summary with no suite that announced itself is just as inconsistent as
    /// the reverse. A lone forged or malformed `test result:` line used to count
    /// as a complete run, and test output could numerically compensate for a real
    /// suite summary that never arrived.
    #[test]
    fn a_lone_summary_with_no_suite_is_incomplete() {
        let lines = [
            "test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s",
        ];
        let parsed = parse_test_output(&lines);
        assert!(
            !parsed.is_complete(),
            "a summary with no suite start describes no run"
        );
    }

    /// Silence is not agreement. No suite announced and none summarised is what
    /// unrecognised output, an empty stream, or a harness that never started
    /// looks like - and calling it complete would make every zero below a fact
    /// about a run nobody observed.
    #[test]
    fn no_suite_at_all_is_incomplete_not_complete() {
        assert!(!parse_test_output(&[]).is_complete());
        assert!(!parse_test_output(&["my own harness says hello"]).is_complete());
    }

    /// The genuinely empty suite: it announced itself, summarised, and reported
    /// nothing. Complete, and a real - if empty - run.
    #[test]
    fn a_legitimately_empty_suite_is_complete() {
        let lines = [
            "",
            "running 0 tests",
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s",
        ];
        let parsed = parse_test_output(&lines);
        assert!(parsed.is_complete());
        assert_eq!(parsed.accounted(), 0);
        assert_eq!(parsed.filtered_out, 0);
    }

    /// Several suites, all of which reported: complete, and the counts add up.
    #[test]
    fn every_suite_reporting_is_complete() {
        let lines = [
            "",
            "running 1 test",
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.01s",
            "",
            "running 1 test",
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s",
        ];
        let parsed = parse_test_output(&lines);
        assert!(parsed.is_complete());
        assert_eq!(parsed.suites, 2, "suites counts terminal summaries");
        assert_eq!(parsed.passed, 2);
        assert_eq!(parsed.filtered_out, 2);
    }

    #[test]
    fn nocapture_failure_extracted_from_stderr_panic() {
        // The `brokkr test` shape: --nocapture means no `---- name
        // stdout ----` blocks; stdout carries the framing and `failures:`
        // sections, while the panic prints to *stderr* (rustc 1.73+
        // format with thread id). The FAIL footer needs
        // name/location/message recovered from stderr.
        let stdout = "\
running 1 test
some test output before the failure
FAILED

failures:

failures:
    test_cmd::tests::scratch

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let stderr = "\
thread 'test_cmd::tests::scratch' (2365348) panicked at src/test_cmd.rs:601:9:
assertion `left == right` failed: intentional failure
  left: 4
 right: 5
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
";
        let stdout_lines: Vec<&str> = stdout.lines().collect();
        let stderr_lines: Vec<&str> = stderr.lines().collect();
        let parsed = parse_test_output_with_stderr(&stdout_lines, &stderr_lines);
        assert_eq!(parsed.failed, 1);
        assert_eq!(parsed.failures.len(), 1, "expected inline-panic fallback");
        let f = &parsed.failures[0];
        assert_eq!(f.name, "test_cmd::tests::scratch");
        assert_eq!(f.location.as_deref(), Some("src/test_cmd.rs:601:9"));
        assert_eq!(
            f.message.as_deref(),
            Some("assertion `left == right` failed: intentional failure")
        );
    }

    #[test]
    fn nocapture_caught_panic_in_passing_run_is_not_a_failure() {
        // A passing test that uses catch_unwind prints a panic line on
        // stderr. With 0 failed, the inline fallback must not fire -
        // run_one checks failures *before* the exit status.
        let stdout = "\
running 3 tests
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let stderr = "\
thread 'foo::probes_panic_path' (1234) panicked at src/probe.rs:10:5:
expected panic, caught fine
";
        let stdout_lines: Vec<&str> = stdout.lines().collect();
        let stderr_lines: Vec<&str> = stderr.lines().collect();
        let parsed = parse_test_output_with_stderr(&stdout_lines, &stderr_lines);
        assert_eq!(parsed.failed, 0);
        assert!(
            parsed.failures.is_empty(),
            "caught panic misread as failure"
        );
    }

    #[test]
    fn nocapture_name_list_vets_stderr_panics_and_takes_last() {
        // One test catches a panic then fails with a second, fatal one;
        // another test's caught panic must not appear in failures. The
        // name list says only `foo::fails` failed; its *last* panic wins.
        let stdout = "\
running 2 tests
FAILED

failures:

failures:
    foo::fails

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
";
        let stderr = "\
thread 'foo::passes' (1) panicked at src/a.rs:1:1:
caught and ignored
thread 'foo::fails' (2) panicked at src/b.rs:5:5:
caught inside the test
thread 'foo::fails' (2) panicked at src/b.rs:9:9:
the fatal assertion
";
        let stdout_lines: Vec<&str> = stdout.lines().collect();
        let stderr_lines: Vec<&str> = stderr.lines().collect();
        let parsed = parse_test_output_with_stderr(&stdout_lines, &stderr_lines);
        assert_eq!(parsed.failures.len(), 1);
        let f = &parsed.failures[0];
        assert_eq!(f.name, "foo::fails");
        assert_eq!(f.location.as_deref(), Some("src/b.rs:9:9"));
        assert_eq!(f.message.as_deref(), Some("the fatal assertion"));
    }

    #[test]
    fn nocapture_old_style_stderr_panic_parses_msg_and_loc() {
        // pre-1.73 format: message inline in quotes, location after.
        let stdout = "\
running 1 test
FAILED

failures:

failures:
    foo::old

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let stderr = "thread 'foo::old' panicked at 'assert_eq!(1, 2)', src/lib.rs:15:9\n";
        let stdout_lines: Vec<&str> = stdout.lines().collect();
        let stderr_lines: Vec<&str> = stderr.lines().collect();
        let parsed = parse_test_output_with_stderr(&stdout_lines, &stderr_lines);
        assert_eq!(parsed.failures.len(), 1);
        let f = &parsed.failures[0];
        assert_eq!(f.name, "foo::old");
        assert_eq!(f.location.as_deref(), Some("src/lib.rs:15:9"));
        assert_eq!(f.message.as_deref(), Some("assert_eq!(1, 2)"));
    }

    #[test]
    fn detail_blocks_take_precedence_over_inline_panics() {
        // When the `----` blocks exist (capture mode), they're strictly
        // richer - the inline fallback must not fire or duplicate.
        let stdout = "\
thread 'foo::test_b' panicked at 'assert_eq!(1, 2)', src/lib.rs:15:9

failures:

---- foo::test_b stdout ----
thread 'foo::test_b' panicked at 'assert_eq!(1, 2)', src/lib.rs:15:9

failures:
    foo::test_b

test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";
        let lines: Vec<&str> = stdout.lines().collect();
        let parsed = parse_test_output(&lines);
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].name, "foo::test_b");
    }

    #[test]
    fn failures_in_every_suite_are_reported_not_just_the_first() {
        // The masking bug: cargo runs the lib unit tests and each
        // integration binary in one stream, and the section state used to
        // latch on the first suite - so a lib failure hid every failure
        // after it, exit code honest, list short. Both must be named.
        let stdout = "\
running 2 tests
test validator::tests::rejects_empty ... FAILED

failures:

---- validator::tests::rejects_empty stdout ----
thread 'validator::tests::rejects_empty' panicked at src/validator.rs:88:9:
assertion failed: parsed.is_err()

failures:
    validator::tests::rejects_empty

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

running 1 test
test provider_hints_prioritize_binance_for_perpetuals ... FAILED

failures:

---- provider_hints_prioritize_binance_for_perpetuals stdout ----
thread 'provider_hints_prioritize_binance_for_perpetuals' panicked at tests/provider_hints.rs:17:5:
assertion `left == right` failed

failures:
    provider_hints_prioritize_binance_for_perpetuals

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
";
        let lines: Vec<&str> = stdout.lines().collect();
        let parsed = parse_test_output(&lines);
        assert_eq!(parsed.suites, 2);
        assert_eq!(parsed.failed, 2);
        let names: Vec<&str> = parsed.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "validator::tests::rejects_empty",
                "provider_hints_prioritize_binance_for_perpetuals",
            ],
            "second suite's failure was dropped from the rendered list",
        );
        assert_eq!(
            parsed.failures[1].location.as_deref(),
            Some("tests/provider_hints.rs:17:5"),
        );

        // ...and it survives the render, since that is what the user reads.
        let rendered = filter_test(stdout, "");
        assert!(rendered.contains("cargo test: 2 failures"), "{rendered}");
        assert!(
            rendered.contains("provider_hints_prioritize_binance_for_perpetuals"),
            "{rendered}",
        );
    }

    #[test]
    fn suite_boundary_inside_a_captured_stdout_block_is_not_a_boundary() {
        // A test that prints `running ...` itself must not reset the
        // per-suite state mid-detail-block and split its own failure off.
        let stdout = "\
running 1 test

failures:

---- foo::chatty stdout ----
running the widget pipeline
thread 'foo::chatty' panicked at src/foo.rs:3:1:
boom

failures:
    foo::chatty

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";
        let lines: Vec<&str> = stdout.lines().collect();
        let parsed = parse_test_output(&lines);
        assert_eq!(parsed.suites, 1);
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].name, "foo::chatty");
        assert_eq!(
            parsed.failures[0].location.as_deref(),
            Some("src/foo.rs:3:1")
        );
    }

    #[test]
    fn test_compile_error_falls_back() {
        let stderr = "\
error[E0425]: cannot find value `missing_symbol` in this scope
 --> tests/foo.rs:3:13
  |
3 |     let _ = missing_symbol;
  |             ^^^^^^^^^^^^^^ not found in this scope

error: could not compile `pbfhogg` (test) due to 1 previous error
";
        let result = filter_test("", stderr);
        assert!(result.contains("cargo test:"), "got: {result}");
        assert!(result.contains("1 error,"), "got: {result}");
        assert!(
            result.contains(
                "error[E0425] tests/foo.rs:3:13 cannot find value `missing_symbol` in this scope"
            ),
            "got: {result}"
        );
        assert!(!result.contains("could not compile"), "got: {result}");
    }

    #[test]
    fn all_pass_but_process_aborted_surfaces_stderr() {
        // libtest counts every test as passing, but the process aborts
        // during teardown (a destructor double-panic -> SIGABRT). The
        // compact summary must NOT swallow the abort - this is the
        // piners `--all-features`/hotpath regression.
        let stdout = "\
running 12 tests
test result: ok. 461 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.60s
";
        let stderr = "\
   Compiling piners-vm v0.1.0 (/home/folk/Programs/piners/crates/piners-vm)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.10s
     Running tests/compiler.rs (/media/folk/Banan/cargo/debug/deps/compiler-fd3c66bf0d4593bc)

thread 'functions::compiler_limits_unbounded_recursive_user_functions' panicked at library/core/src/panicking.rs:233:5:
panic in a destructor during cleanup
thread caused non-unwinding panic. aborting.
error: test failed, to rerun pass `-p piners-vm --test compiler`

Caused by:
  process didn't exit successfully: `compiler-fd3c66bf0d4593bc --test-threads=1` (signal: 6, SIGABRT: process abort signal)
";
        let result = filter_test(stdout, stderr);
        assert!(result.contains("461 passed"), "got: {result}");
        assert!(result.contains("exited non-zero"), "got: {result}");
        assert!(
            result.contains("panic in a destructor during cleanup"),
            "got: {result}"
        );
        assert!(
            result.contains("-p piners-vm --test compiler"),
            "got: {result}"
        );
        assert!(result.contains("SIGABRT"), "got: {result}");
        // Build/progress chatter is stripped from the surfaced excerpt.
        assert!(!result.contains("Compiling piners-vm"), "got: {result}");
        assert!(
            !result.contains("Running tests/compiler.rs"),
            "got: {result}"
        );
    }

    #[test]
    fn parse_count_works() {
        let line = "test result: ok. 15 passed; 0 failed; 3 ignored; 0 measured; 2 filtered out";
        assert_eq!(parse_count(line, "passed"), Some(15));
        assert_eq!(parse_count(line, "failed"), Some(0));
        assert_eq!(parse_count(line, "ignored"), Some(3));
        assert_eq!(parse_count(line, "filtered out"), Some(2));
    }

    #[test]
    fn parse_duration_works() {
        let line = "test result: ok. 15 passed; finished in 0.45s";
        assert!((parse_duration(line).unwrap() - 0.45).abs() < 0.001);
    }
}

#[cfg(test)]
mod failure_completeness_tests {
    #![allow(clippy::unwrap_used)]
    use super::{filter_test, parse_test_output, parse_test_output_with_stderr};

    /// Two tests down, but only one produced a `---- name stdout ----`
    /// detail block the parser could read. The `failures:` name list is the
    /// authoritative roster and must fill in the rest - the defect was a
    /// report headed "1 failure" while two tests were red.
    const ONE_DETAIL_TWO_NAMED: &str = "\
running 2 tests
test alpha ... FAILED
test beta ... FAILED

failures:

---- alpha stdout ----
thread 'alpha' panicked at src/lib.rs:10:5:
boom

failures:
    alpha
    beta

test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";

    #[test]
    fn the_name_list_completes_a_short_failure_roster() {
        let lines: Vec<&str> = ONE_DETAIL_TWO_NAMED.lines().collect();
        let parsed = parse_test_output(&lines);
        assert_eq!(parsed.failed, 2);
        let names: Vec<&str> = parsed.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        // The one with a detail block keeps its location; the recovered one
        // is bare rather than absent.
        assert!(parsed.failures[0].location.is_some());
        assert!(parsed.failures[1].location.is_none());
    }

    #[test]
    fn the_rendered_report_names_every_failing_test() {
        let out = filter_test(ONE_DETAIL_TWO_NAMED, "error: test failed");
        assert!(out.starts_with("cargo test: 2 failures"), "got: {out}");
        assert!(out.contains("FAILED alpha"), "got: {out}");
        assert!(out.contains("FAILED beta"), "got: {out}");
    }

    /// When nothing can be attributed to a name, the headline still reports
    /// libtest's own tally and says so - never the shorter roster alone.
    #[test]
    fn an_unattributable_failure_is_counted_not_dropped() {
        let stdout = "\
running 3 tests

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let out = filter_test(stdout, "error: test failed, to rerun pass `-p a --test b`");
        assert!(out.starts_with("cargo test: 2 failures"), "got: {out}");
        assert!(out.contains("libtest counted 2 failed"), "got: {out}");
        // The binary cargo names is the reader's only handle on the unnamed
        // failures, and no hint may point at a flag that does not exist.
        assert!(out.contains("-p a --test b"), "got: {out}");
        assert!(!out.contains("--raw"), "got: {out}");
    }

    /// A harness that fails one test and then aborts in the next never
    /// summarises, so its failures block is never rendered. The failed
    /// verdict of that unclosed suite is the only record of the first failure,
    /// and the stderr panic still supplies its message.
    #[test]
    fn a_crashed_suite_keeps_the_failures_it_reported_before_dying() {
        let stdout = "\
running 3 tests
test a::first ... FAILED
";
        let stderr = "\
thread 'a::first' panicked at src/a.rs:3:5:
boom
";
        let lines: Vec<&str> = stdout.lines().collect();
        let err: Vec<&str> = stderr.lines().collect();
        let parsed = parse_test_output_with_stderr(&lines, &err);
        assert!(!parsed.is_complete());
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].name, "a::first");
        assert_eq!(parsed.failures[0].message.as_deref(), Some("boom"));
        assert_eq!(parsed.failures[0].location.as_deref(), Some("src/a.rs:3:5"));
    }

    /// A harness lost to a signal is named in the report, beside the failures
    /// it reported before dying; a harness that merely failed (an exit status)
    /// is not, since its failures already are.
    #[test]
    fn a_signal_death_is_named_beside_the_failures() {
        let stdout = "\
running 1 test
test a_fails ... FAILED

failures:
    a_fails

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
brokkr: this test harness ended with its suite still open; last test seen starting: b_aborts (a suspect - named by the harness's own output)
";
        let stderr = "\
error: test failed, to rerun pass `--test a_crash`

Caused by:
  process didn't exit successfully: `/usr/bin/brokkr __brokkr_harness_shim /t/debug/deps/a_crash-2527e25b x --format json` (signal: 6, SIGABRT: process abort signal)
error: test failed, to rerun pass `--test c_fail`

Caused by:
  process didn't exit successfully: `/t/debug/deps/c_fail-99aa x` (exit status: 101)
";
        let out = filter_test(stdout, stderr);
        assert!(
            out.contains("CRASHED a_crash (signal: 6, SIGABRT: process abort signal)"),
            "{out}"
        );
        assert!(
            !out.contains("CRASHED c_fail"),
            "an exit status is not a crash: {out}"
        );
        assert!(out.contains("last test seen starting: b_aborts"), "{out}");
    }

    /// The verdicts stand in for a name list only for a suite that never
    /// summarised. A passing test printing a verdict-shaped line into a suite
    /// that finished adds nothing.
    #[test]
    fn a_verdict_line_in_a_finished_suite_adds_no_failure() {
        let stdout = "\
running 1 test
test fake::name ... FAILED
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let lines: Vec<&str> = stdout.lines().collect();
        let parsed = parse_test_output(&lines);
        assert!(parsed.is_complete());
        assert!(parsed.failures.is_empty());
    }
}
