//! Filters raw cargo output into one-line-per-diagnostic summaries.
//!
//! Instead of dumping hundreds of lines of cargo decoration (source excerpts,
//! pipe characters, help suggestions, diff hunks), each diagnostic becomes a
//! single line: `error[CODE] file:line:col message`
//!
//! Test output parsing is split into a shared structured layer ([`parse_test_output`])
//! so that both text and JSON modes can consume the same parsed results.
//!
//! # Why two clippy paths exist
//!
//! [`parse_clippy`] (text-mode parser) used to be the primary clippy
//! ingestion path: it scraped cargo's pretty-printed stderr. That worked
//! but lost the lint code on every warning past the first per-rule -
//! cargo only emits `= note: #[warn(rule)]` once per crate compilation,
//! so repeats of the same lint came through as bare `warning` headers.
//!
//! The clippy phase in `src/check_cmd.rs` now ingests cargo's
//! `--message-format=json` output via [`crate::cargo_json`] instead and
//! converts each [`crate::cargo_json::DiagnosticEvent`] into a
//! [`ClippyDiagnostic`] for formatting - so every warning keeps its lint
//! code in the header regardless of repeat count.
//!
//! [`parse_clippy`] and [`filter_clippy`] are kept because the test
//! phase (`run_test_phase` in check_cmd.rs) still falls back to
//! [`filter_clippy`] when a `cargo test` build emits compile errors on
//! stderr - it's not worth a second JSON pass for the rare build-error
//! case.

use std::path::Path;

/// One parsed clippy diagnostic. [`parse_clippy`] returns these so callers
/// can filter/partition by source path before formatting.
pub struct ClippyDiagnostic {
    pub is_error: bool,
    /// `error[E0308]` / `warning[unused_variables]` / bare `error` / bare `warning`.
    pub header: String,
    /// `file:line:col` (unformatted, as clippy prints it).
    pub location: Option<String>,
    pub message: String,
    pub detail: Option<String>,
}

impl ClippyDiagnostic {
    /// Extract the file portion of `location` for scope matching.
    pub fn path(&self) -> Option<&Path> {
        let loc = self.location.as_deref()?;
        let (file, _) = loc.split_once(':')?;
        Some(Path::new(file))
    }

    /// Format as a single line, matching [`filter_clippy`]'s shape.
    pub fn format_one(&self) -> String {
        let base = match &self.location {
            Some(loc) => format!("{} {} {}", self.header, loc, self.message),
            None => format!("{} {}", self.header, self.message),
        };
        match &self.detail {
            Some(d) => format!("{base} - {d}"),
            None => base,
        }
    }
}

/// Parsed clippy output.
///
/// Diagnostics are ordered errors-first, then warnings (stable within each).
/// When cargo emitted `error:`/`warning:` markers but the parser extracted
/// nothing, `parse_failed` is `true` and callers should print the raw output
/// instead of the parsed list.
pub struct ClippyParse {
    pub diagnostics: Vec<ClippyDiagnostic>,
    pub parse_failed: bool,
}

/// Parse cargo clippy text output into structured diagnostics.
pub fn parse_clippy(output: &str) -> ClippyParse {
    let mut diagnostics: Vec<ClippyDiagnostic> = Vec::new();

    let mut in_block = false;
    let mut current_block: Vec<String> = Vec::new();
    let mut current_is_error = false;

    for line in output.lines() {
        if is_noise(line.trim_start()) || is_meta_noise(line) {
            continue;
        }

        let is_error_start = line.starts_with("error:") || line.starts_with("error[");
        let is_warning_start = line.starts_with("warning:") || line.starts_with("warning[");

        if is_error_start || is_warning_start {
            if in_block && !current_block.is_empty() {
                diagnostics.push(parse_block(&current_block, current_is_error));
                current_block.clear();
            }
            current_is_error = is_error_start;
            in_block = true;
            current_block.push(line.to_string());
        } else if in_block {
            current_block.push(line.to_string());
        }
    }

    if in_block && !current_block.is_empty() {
        diagnostics.push(parse_block(&current_block, current_is_error));
    }

    // Errors first, then warnings; each half keeps discovery order.
    let (errors, warnings): (Vec<_>, Vec<_>) =
        diagnostics.into_iter().partition(|d| d.is_error);
    let mut sorted = errors;
    sorted.extend(warnings);

    let parse_failed = sorted.is_empty()
        && output.lines().any(|l| {
            l.starts_with("error:")
                || l.starts_with("error[")
                || l.starts_with("warning:")
                || l.starts_with("warning[")
        });

    ClippyParse {
        diagnostics: sorted,
        parse_failed,
    }
}

/// Filter cargo clippy output into one line per diagnostic.
///
/// Output format:
/// ```text
/// cargo clippy: 2 errors, 3 warnings
///   error[E0308] src/foo.rs:10:5 mismatched types
///   error[E0425] src/bar.rs:20:3 cannot find value `x` in this scope
///   warning[unused_variables] src/a.rs:1:9 unused variable: `x`
///   warning[clippy::needless_return] src/d.rs:4:5 this could be simplified
/// ```
pub fn filter_clippy(output: &str) -> String {
    let parsed = parse_clippy(output);
    if parsed.parse_failed {
        return output.to_string();
    }
    if parsed.diagnostics.is_empty() {
        return "cargo clippy: no issues".into();
    }

    let errors = parsed.diagnostics.iter().filter(|d| d.is_error).count();
    let warnings = parsed.diagnostics.len() - errors;

    let mut result = format!("cargo clippy: {errors} errors, {warnings} warnings\n");
    for d in &parsed.diagnostics {
        result.push_str("  ");
        result.push_str(&d.format_one());
        result.push('\n');
    }
    result.trim_end().to_string()
}

// --- Shared test output parser ---

/// A single parsed test failure.
#[derive(Clone)]
pub struct ParsedTestFailure {
    pub name: String,
    pub location: Option<String>,
    pub message: Option<String>,
}

/// Aggregated test results from one or more test suites.
pub struct ParsedTestResults {
    pub failures: Vec<ParsedTestFailure>,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub filtered_out: usize,
    pub suites: usize,
    pub duration: Option<f64>,
}

/// Parse cargo test stdout into structured results.
///
/// Handles the two `failures:` sections (detail then name-list), extracts
/// panic locations and messages, and aggregates `test result:` summary lines.
/// Works on any iterator of lines - callers can pre-filter JSON lines out
/// before passing non-JSON lines here.
pub fn parse_test_output(lines: &[&str]) -> ParsedTestResults {
    parse_test_output_with_stderr(lines, &[])
}

/// Like [`parse_test_output`], but also scans stderr for inline panic
/// lines. Rust panics print to *stderr*, so under `--nocapture` (no
/// captured `---- name stdout ----` blocks) the failure location and
/// message are only recoverable from there. The `failures:` name list
/// on stdout still vets which panics belong to actual failures.
#[allow(clippy::too_many_lines)] // state-machine parser - splitting hurts clarity
pub fn parse_test_output_with_stderr(
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

    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Compiling")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Finished")
            || line.starts_with("running ")
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

    // --nocapture fallback: no `---- name stdout ----` blocks, so the
    // detail section yielded nothing, but the stream carried inline
    // panics. Gated on `failed > 0` because a *passing* test can print
    // panic lines too (`catch_unwind`). When the name-list section is
    // present, vet by name and take the *last* panic per failing test
    // (a caught panic may precede the fatal one); fall back to the raw
    // inline list only if no name matched (thread-name mismatch).
    if failures.is_empty() && failed > 0 {
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

    ParsedTestResults {
        failures,
        passed,
        failed,
        ignored,
        filtered_out,
        suites,
        duration: if has_duration { Some(duration) } else { None },
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
        if vetted.is_empty() { self.panics } else { vetted }
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
/// Falls back to `filter_clippy` to show the build errors.
pub fn filter_test(stdout: &str, stderr: &str) -> String {
    // Compilation failure - no test results, just build errors.
    let has_test_result = stdout.lines().any(|l| l.starts_with("test result:"));
    let has_compile_error = stderr.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("error[") || (t.starts_with("error:") && !t.contains("test run failed"))
    });

    if !has_test_result && has_compile_error {
        let filtered = filter_clippy(stderr);
        if filtered.starts_with("cargo clippy:") {
            return filtered.replacen("cargo clippy:", "cargo test:", 1);
        }
        return filtered;
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
    if parsed.failures.is_empty() && parsed.suites > 0 {
        if harness_failure {
            return format!(
                "{} - but the test process exited non-zero:\n{}",
                format_test_summary(&parsed),
                harness_failure_excerpt(stderr),
            );
        }
        return format_test_summary(&parsed);
    }

    // Parser found no failures (suites == 0). Surface a FAILED line the
    // parser missed, or a harness-level abort - never silently succeed.
    if parsed.failures.is_empty() {
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

    // Failures present - format as one-liners.
    format_test_failures(&parsed)
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

/// Format parsed test failures as one-liner text output.
fn format_test_failures(parsed: &ParsedTestResults) -> String {
    let mut result = format!(
        "cargo test: {} failure{}\n",
        parsed.failures.len(),
        if parsed.failures.len() == 1 { "" } else { "s" }
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

    result.trim_end().to_string()
}

// --- helpers ---

fn is_noise(trimmed: &str) -> bool {
    trimmed.starts_with("Compiling")
        || trimmed.starts_with("Checking")
        || trimmed.starts_with("Downloading")
        || trimmed.starts_with("Downloaded")
        || trimmed.starts_with("Finished")
        || trimmed.starts_with("Locking")
        || trimmed.starts_with("Updating")
}

fn is_meta_noise(line: &str) -> bool {
    // "warning: `crate` (lib) generated N warnings"
    if line.starts_with("warning:")
        && line.contains("generated")
        && line.contains("warning")
    {
        return true;
    }
    // "error: aborting …" / "error: could not compile …"
    if (line.starts_with("error:") || line.starts_with("error["))
        && (line.contains("aborting due to") || line.contains("could not compile"))
    {
        return true;
    }
    // "warning: build failed, waiting for other jobs to finish..."
    if line.starts_with("warning:") && line.contains("build failed") {
        return true;
    }
    false
}

/// Extract the `-->` location from a block of diagnostic lines.
fn extract_location(block: &[String]) -> Option<String> {
    for line in block {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--> ") {
            return Some(trimmed.strip_prefix("--> ")?.to_string());
        }
    }
    None
}

/// Extract a lint rule name from a `= note: `#[warn(rule)]` on by default` or
/// `= note: `#[deny(rule)]` ...` line. Real cargo/clippy puts warning rule
/// names in these notes rather than in the header line.
fn extract_rule_from_notes(block: &[String]) -> Option<String> {
    for line in block.iter().skip(1) {
        let trimmed = line.trim_start().trim_start_matches('=').trim_start();
        let Some(rest) = trimmed.strip_prefix("note:").map(str::trim) else {
            continue;
        };
        for tag in ["#[warn(", "#[deny("] {
            let needle = format!("`{tag}");
            if let Some(start) = rest.find(&needle)
                && let Some(close) = rest[start + needle.len()..].find(")]`")
            {
                let begin = start + needle.len();
                return Some(rest[begin..begin + close].to_string());
            }
        }
    }
    None
}

/// Parse a diagnostic header into (prefix, message).
///
/// `"error[E0308]: mismatched types"` → `("error[E0308]", "mismatched types")`
/// `"warning: unused variable: \`x\` [unused_variables]"` → `("warning[unused_variables]", "unused variable: \`x\`")`
fn parse_header(line: &str) -> (String, String) {
    // error[CODE]: message
    if (line.starts_with("error[") || line.starts_with("warning["))
        && let Some(bracket_end) = line.find(']') {
            let prefix = &line[..bracket_end + 1];
            let message = line[bracket_end + 1..]
                .trim_start_matches(':')
                .trim();
            return (prefix.to_string(), message.to_string());
        }

    // "warning: message [rule]" or "error: message"
    let (level, rest) = if let Some(rest) = line.strip_prefix("error: ") {
        ("error", rest)
    } else if let Some(rest) = line.strip_prefix("warning: ") {
        ("warning", rest)
    } else {
        return (line.to_string(), String::new());
    };

    // Check for trailing [rule].
    if let Some(bracket_start) = rest.rfind('[')
        && let Some(bracket_end) = rest.rfind(']')
            && bracket_end > bracket_start {
                let rule = &rest[bracket_start + 1..bracket_end];
                let message = rest[..bracket_start].trim();
                return (format!("{level}[{rule}]"), message.to_string());
            }

    (level.to_string(), rest.to_string())
}

/// Extract key detail from a diagnostic block (e.g. "expected X, found Y").
///
/// Scans block lines for:
/// 1. Lines containing both "expected" and "found" (inline type annotations)
/// 2. `= note:` lines with type detail (multi-line expected/found)
fn extract_detail(block: &[String]) -> Option<String> {
    // First pass: look for a single line with both "expected" and "found".
    for line in block.iter().skip(1) {
        let trimmed = line
            .trim_start()
            .trim_start_matches('|')
            .trim_start()
            .trim_start_matches('^')
            .trim_start_matches('-')
            .trim_start();
        if trimmed.contains("expected") && trimmed.contains("found") {
            return Some(trimmed.to_string());
        }
    }

    // Second pass: look for `= note:` expected/found lines (may be split across
    // a `= note: expected ...` line and a continuation `found ...` line).
    let mut expected = None;
    let mut found = None;
    for line in block.iter().skip(1) {
        let trimmed = line.trim_start().trim_start_matches('=').trim_start();
        if let Some(rest) = trimmed.strip_prefix("note:") {
            let rest = rest.trim();
            if rest.starts_with("expected") && expected.is_none() {
                expected = Some(rest.to_string());
            } else if rest.starts_with("found") && found.is_none() {
                found = Some(rest.to_string());
            }
        } else if expected.is_some() && found.is_none() {
            // Continuation line under `= note:` - often the `found` part.
            let trimmed = line.trim();
            if trimmed.starts_with("found") {
                found = Some(trimmed.to_string());
            }
        }
    }
    if let (Some(exp), Some(fnd)) = (expected, found) {
        return Some(format!("{exp}, {fnd}"));
    }

    None
}

/// Parse a diagnostic block into structured fields.
///
/// `["error[E0425]: cannot find value ...", " --> src/foo.rs:10:5", ...]`
/// → `ClippyDiagnostic { header: "error[E0425]", location: Some("src/foo.rs:10:5"), ... }`
fn parse_block(block: &[String], is_error: bool) -> ClippyDiagnostic {
    let (mut header, message) = parse_header(&block[0]);
    // If the header didn't carry a [rule], try to recover one from a
    // `= note: \`#[warn(rule)]\`` line in the block. Real cargo/clippy
    // emits the rule there, not in the header.
    if (header == "warning" || header == "error")
        && let Some(rule) = extract_rule_from_notes(block)
    {
        header = format!("{header}[{rule}]");
    }
    let location = extract_location(block);
    let detail = extract_detail(block);

    ClippyDiagnostic {
        is_error,
        header,
        location,
        message,
        detail,
    }
}

/// Parse "N <label>" from a test result line.
pub(crate) fn parse_count(line: &str, label: &str) -> Option<usize> {
    let idx = line.find(label)?;
    let before = line[..idx].trim_end();
    let num_str = before.rsplit(|c: char| !c.is_ascii_digit()).next()?;
    num_str.parse().ok()
}

/// Parse "finished in N.NNs" from a test result line.
pub(crate) fn parse_duration(line: &str) -> Option<f64> {
    let marker = "finished in ";
    let idx = line.find(marker)?;
    let rest = &line[idx + marker.len()..];
    let end = rest.find('s')?;
    rest[..end].parse().ok()
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
    fn clippy_clean() {
        let output = "    Checking pbfhogg v0.1.0\n    Finished dev target(s) in 1.53s\n";
        let result = filter_clippy(output);
        assert_eq!(result, "cargo clippy: no issues");
    }

    #[test]
    fn clippy_one_line_per_diagnostic() {
        let output = "\
warning: unused variable: `x` [unused_variables]
 --> src/main.rs:10:9
  |
10|     let x = 5;
  |         ^ help: prefix with underscore: `_x`

error[E0308]: mismatched types
 --> src/foo.rs:20:5
  |
20|     \"hello\"
  |     ^^^^^^^ expected `i32`, found `&str`

warning: `pbfhogg` (lib) generated 1 warning
error: aborting due to 1 previous error
";
        let result = filter_clippy(output);
        assert!(result.starts_with("cargo clippy: 1 errors, 1 warnings"), "got: {result}");
        // Error line: one-liner with code, location, message, and type detail.
        assert!(
            result.contains("error[E0308] src/foo.rs:20:5 mismatched types - expected `i32`, found `&str`"),
            "got: {result}"
        );
        // Warning line: one-liner with rule, location, message.
        assert!(
            result.contains("warning[unused_variables] src/main.rs:10:9 unused variable: `x`"),
            "got: {result}"
        );
        // No decoration.
        assert!(!result.contains("aborting"), "got: {result}");
        assert!(!result.contains("generated"), "got: {result}");
        assert!(!result.contains("help:"), "got: {result}");
        assert!(!result.contains("^^^"), "got: {result}");
    }

    #[test]
    fn clippy_errors_before_warnings() {
        let output = "\
warning: unused variable: `x` [unused_variables]
 --> src/main.rs:10:9
  |

error[E0308]: mismatched types
 --> src/foo.rs:20:5
  |
";
        let result = filter_clippy(output);
        let error_pos = result.find("error[E0308]").unwrap();
        let warning_pos = result.find("warning[unused_variables]").unwrap();
        assert!(error_pos < warning_pos, "errors should come before warnings: {result}");
    }

    #[test]
    fn clippy_multiple_same_rule() {
        let output = "\
warning: unused variable: `x` [unused_variables]
 --> src/a.rs:1:9
  |

warning: unused variable: `y` [unused_variables]
 --> src/b.rs:2:9
  |

warning: unused variable: `z` [unused_variables]
 --> src/c.rs:3:9
  |

warning: this could be simplified [clippy::needless_return]
 --> src/d.rs:4:5
  |

";
        let result = filter_clippy(output);
        assert!(result.contains("0 errors, 4 warnings"), "got: {result}");
        // Each warning is its own line - no grouping.
        assert_eq!(result.matches("warning[unused_variables]").count(), 3, "got: {result}");
        assert!(result.contains("warning[clippy::needless_return]"), "got: {result}");
    }

    #[test]
    fn clippy_build_failed_noise_stripped() {
        let output = "\
error[E0425]: cannot find value `x` in this scope
 --> src/foo.rs:10:5
  |

warning: build failed, waiting for other jobs to finish...
";
        let result = filter_clippy(output);
        assert!(!result.contains("build failed"), "got: {result}");
        assert!(result.contains("1 errors, 0 warnings"), "got: {result}");
    }

    #[test]
    fn clippy_note_expected_found() {
        let output = "\
error[E0308]: mismatched types
 --> src/commands/getparents.rs:68:29
  |
68|         let stats = GetparentsStats {
  |                     ^^^^^^^^^^^^^^^^ expected `RenumberStats`, found `GetparentsStats`
  |
  = note: expected struct `RenumberStats`
             found struct `GetparentsStats`

error: aborting due to 1 previous error
";
        let result = filter_clippy(output);
        // The inline expected/found line should be captured.
        assert!(
            result.contains("- expected `RenumberStats`, found `GetparentsStats`"),
            "got: {result}"
        );
    }

    #[test]
    fn clippy_warning_rule_from_note() {
        // Real cargo/clippy emits the warning rule in a `= note` line,
        // not in the header. The formatter should recover it.
        let output = "\
warning: unused imports: `QueryFilter` and `ResultsDb`
 --> src/db/compare.rs:78:21
  |
78|     use crate::db::{QueryFilter, ResultsDb, RunRow};
  |                     ^^^^^^^^^^  ^^^^^^^^^
  |
  = note: `#[warn(unused_imports)]` on by default

warning: this `if` statement can be collapsed
 --> src/lockfile.rs:575:16
  |
  = note: `#[warn(clippy::collapsible_if)]` on by default
";
        let result = filter_clippy(output);
        assert!(
            result.contains("warning[unused_imports] src/db/compare.rs:78:21 unused imports: `QueryFilter` and `ResultsDb`"),
            "got: {result}"
        );
        assert!(
            result.contains("warning[clippy::collapsible_if] src/lockfile.rs:575:16 this `if` statement can be collapsed"),
            "got: {result}"
        );
    }

    #[test]
    fn clippy_note_only_expected_found() {
        // Some diagnostics have the expected/found only in = note: lines,
        // not inline with the source annotation.
        let output = "\
error[E0308]: mismatched types
 --> src/lib.rs:42:12
  |
42|     foo(bar)
  |         ^^^ arguments to this function are incorrect
  |
  = note: expected reference `&Vec<u8>`
             found reference `&Vec<i32>`

error: aborting due to 1 previous error
";
        let result = filter_clippy(output);
        assert!(
            result.contains("- expected reference `&Vec<u8>`, found reference `&Vec<i32>`"),
            "got: {result}"
        );
    }

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
        let failure_lines: Vec<&str> = result.lines().filter(|l| l.starts_with("  FAILED")).collect();
        assert_eq!(failure_lines.len(), 1, "got: {result}");
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
        assert!(parsed.failures.is_empty(), "caught panic misread as failure");
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
        assert!(result.contains("1 errors"), "got: {result}");
        assert!(
            result.contains("error[E0425] tests/foo.rs:3:13 cannot find value `missing_symbol` in this scope"),
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
        assert!(result.contains("panic in a destructor during cleanup"), "got: {result}");
        assert!(result.contains("-p piners-vm --test compiler"), "got: {result}");
        assert!(result.contains("SIGABRT"), "got: {result}");
        // Build/progress chatter is stripped from the surfaced excerpt.
        assert!(!result.contains("Compiling piners-vm"), "got: {result}");
        assert!(!result.contains("Running tests/compiler.rs"), "got: {result}");
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

    #[test]
    fn parse_clippy_returns_structured_diagnostics() {
        let output = "\
error[E0308]: mismatched types
 --> src/foo.rs:20:5
  |

warning: unused variable: `x` [unused_variables]
 --> src/main.rs:10:9
  |
";
        let parsed = parse_clippy(output);
        assert!(!parsed.parse_failed);
        assert_eq!(parsed.diagnostics.len(), 2);
        // Errors come first.
        assert!(parsed.diagnostics[0].is_error);
        assert_eq!(parsed.diagnostics[0].header, "error[E0308]");
        assert_eq!(parsed.diagnostics[0].location.as_deref(), Some("src/foo.rs:20:5"));
        assert_eq!(parsed.diagnostics[0].message, "mismatched types");
        assert!(!parsed.diagnostics[1].is_error);
        assert_eq!(parsed.diagnostics[1].header, "warning[unused_variables]");
    }

    #[test]
    fn clippy_diagnostic_path_extracts_file() {
        let d = ClippyDiagnostic {
            is_error: true,
            header: "error[E0308]".into(),
            location: Some("src/foo.rs:20:5".into()),
            message: "mismatched types".into(),
            detail: None,
        };
        assert_eq!(d.path(), Some(Path::new("src/foo.rs")));
    }

    #[test]
    fn clippy_diagnostic_path_none_without_location() {
        let d = ClippyDiagnostic {
            is_error: false,
            header: "warning".into(),
            location: None,
            message: "something".into(),
            detail: None,
        };
        assert!(d.path().is_none());
    }

    #[test]
    fn clippy_diagnostic_format_one_matches_filter_output() {
        let d = ClippyDiagnostic {
            is_error: true,
            header: "error[E0308]".into(),
            location: Some("src/foo.rs:20:5".into()),
            message: "mismatched types".into(),
            detail: Some("expected `i32`, found `&str`".into()),
        };
        assert_eq!(
            d.format_one(),
            "error[E0308] src/foo.rs:20:5 mismatched types - expected `i32`, found `&str`"
        );
    }

    #[test]
    fn parse_clippy_empty_has_no_diagnostics() {
        let parsed = parse_clippy("");
        assert!(!parsed.parse_failed);
        assert!(parsed.diagnostics.is_empty());
    }
}
