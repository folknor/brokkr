//! Filters raw cargo output into one-line-per-diagnostic summaries.
//!
//! Instead of dumping hundreds of lines of cargo decoration (source excerpts,
//! pipe characters, help suggestions, diff hunks), each diagnostic becomes a
//! single line: `error[CODE] file:line:col message`

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
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

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
            // Flush previous block.
            if in_block && !current_block.is_empty() {
                let diag = format_diagnostic(&current_block);
                if current_is_error {
                    errors.push(diag);
                } else {
                    warnings.push(diag);
                }
                current_block.clear();
            }

            current_is_error = is_error_start;
            in_block = true;
            current_block.push(line.to_string());
        } else if in_block {
            if line.trim().is_empty() && current_block.len() > 3 {
                let diag = format_diagnostic(&current_block);
                if current_is_error {
                    errors.push(diag);
                } else {
                    warnings.push(diag);
                }
                current_block.clear();
                in_block = false;
            } else {
                current_block.push(line.to_string());
            }
        }
    }

    // Flush trailing block.
    if in_block && !current_block.is_empty() {
        let diag = format_diagnostic(&current_block);
        if current_is_error {
            errors.push(diag);
        } else {
            warnings.push(diag);
        }
    }

    if errors.is_empty() && warnings.is_empty() {
        // If the output had lines that look like errors/warnings but we extracted
        // nothing, the parser failed — fall back to raw output.
        let has_error_lines = output
            .lines()
            .any(|l| l.starts_with("error:") || l.starts_with("error["));
        let has_warning_lines = output.lines().any(|l| {
            l.starts_with("warning:") || l.starts_with("warning[")
        });
        if has_error_lines || has_warning_lines {
            return output.to_string();
        }
        return "cargo clippy: no issues".into();
    }

    let mut result = format!(
        "cargo clippy: {} errors, {} warnings\n",
        errors.len(),
        warnings.len()
    );
    for line in &errors {
        result.push_str("  ");
        result.push_str(line);
        result.push('\n');
    }
    for line in &warnings {
        result.push_str("  ");
        result.push_str(line);
        result.push('\n');
    }

    result.trim_end().to_string()
}

/// Filter cargo test output — one line per failure, compact summary on success.
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
    // Compilation failure — no test results, just build errors.
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

    // Parse test failures and result summaries from stdout.
    let mut failures: Vec<String> = Vec::new();
    let mut summary_lines: Vec<String> = Vec::new();
    let mut in_failure_detail = false;
    let mut seen_failure_section = false;
    let mut current_name = String::new();
    let mut current_panic_loc = String::new();
    let mut current_panic_msg = String::new();

    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Compiling")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Finished")
            || line.starts_with("running ")
            || (line.starts_with("test ") && line.ends_with("... ok"))
        {
            continue;
        }

        if line == "failures:" {
            if !seen_failure_section {
                in_failure_detail = true;
                seen_failure_section = true;
            } else {
                // Second "failures:" section — flush and stop collecting.
                in_failure_detail = false;
                flush_test_failure(
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

        if in_failure_detail {
            if line.starts_with("test result:") {
                in_failure_detail = false;
                flush_test_failure(
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
                // New failure block — flush previous.
                flush_test_failure(
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
                // "thread 'name' panicked at src/file.rs:10:5:" (Rust 2021+)
                // "thread 'name' panicked at 'msg', src/file.rs:10:5" (older)
                if let Some(idx) = line.find("panicked at ") {
                    let rest = &line[idx + "panicked at ".len()..];
                    let rest = rest.trim_end_matches(':');
                    // If it starts with a quote, it's the old format: 'msg', location
                    if rest.starts_with('\'') {
                        if let Some(end_quote) = rest[1..].find('\'') {
                            current_panic_msg = rest[1..1 + end_quote].to_string();
                            let after = rest[1 + end_quote + 1..].trim_start_matches(", ");
                            current_panic_loc = after.to_string();
                        }
                    } else {
                        // New format: location on this line, message on next
                        current_panic_loc = rest.to_string();
                    }
                }
            } else if current_panic_msg.is_empty()
                && !current_panic_loc.is_empty()
                && !line.trim().is_empty()
            {
                // First non-empty line after "panicked at location:" is the message.
                current_panic_msg = line.trim().to_string();
            }
        }

        if !in_failure_detail && line.starts_with("test result:") {
            summary_lines.push(line.to_string());
        }
    }

    flush_test_failure(
        &current_name,
        &current_panic_loc,
        &current_panic_msg,
        &mut failures,
    );

    // All passed — aggregate into compact format.
    if failures.is_empty() && !summary_lines.is_empty() {
        if let Some(agg) = aggregate_test_results(&summary_lines) {
            return agg;
        }
        return summary_lines.join("\n");
    }

    // Parser found no failures but stdout has FAILED lines — fall back to raw.
    if failures.is_empty() {
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
    }

    // Failures present.
    let mut result = format!(
        "cargo test: {} failure{}\n",
        failures.len(),
        if failures.len() == 1 { "" } else { "s" }
    );
    for line in &failures {
        result.push_str("  ");
        result.push_str(line);
        result.push('\n');
    }
    if !summary_lines.is_empty() {
        result.push('\n');
        for line in &summary_lines {
            result.push_str(line);
            result.push('\n');
        }
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

/// Parse a diagnostic header into (prefix, message).
///
/// `"error[E0308]: mismatched types"` → `("error[E0308]", "mismatched types")`
/// `"warning: unused variable: \`x\` [unused_variables]"` → `("warning[unused_variables]", "unused variable: \`x\`")`
fn parse_header(line: &str) -> (String, String) {
    // error[CODE]: message
    if line.starts_with("error[") || line.starts_with("warning[") {
        if let Some(bracket_end) = line.find(']') {
            let prefix = &line[..bracket_end + 1];
            let message = line[bracket_end + 1..]
                .trim_start_matches(':')
                .trim();
            return (prefix.to_string(), message.to_string());
        }
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
    if let Some(bracket_start) = rest.rfind('[') {
        if let Some(bracket_end) = rest.rfind(']') {
            if bracket_end > bracket_start {
                let rule = &rest[bracket_start + 1..bracket_end];
                let message = rest[..bracket_start].trim();
                return (format!("{level}[{rule}]"), message.to_string());
            }
        }
    }

    (level.to_string(), rest.to_string())
}

/// Format a diagnostic block into a single line.
///
/// `["error[E0425]: cannot find value ...", " --> src/foo.rs:10:5", ...]`
/// → `"error[E0425] src/foo.rs:10:5 cannot find value ..."`
fn format_diagnostic(block: &[String]) -> String {
    let (prefix, message) = parse_header(&block[0]);
    let location = extract_location(block).unwrap_or_default();

    if location.is_empty() {
        format!("{prefix} {message}")
    } else {
        format!("{prefix} {location} {message}")
    }
}

/// Flush a collected test failure into the failures vec as a one-liner.
fn flush_test_failure(
    name: &str,
    panic_loc: &str,
    panic_msg: &str,
    failures: &mut Vec<String>,
) {
    if name.is_empty() {
        return;
    }
    let mut line = format!("FAILED {name}");
    if !panic_loc.is_empty() {
        line.push(' ');
        line.push_str(panic_loc);
    }
    if !panic_msg.is_empty() {
        line.push(' ');
        line.push_str(panic_msg);
    }
    failures.push(line);
}

/// Aggregate multiple "test result: ok." lines into a compact summary.
fn aggregate_test_results(summary_lines: &[String]) -> Option<String> {
    let mut total_passed: usize = 0;
    let mut total_failed: usize = 0;
    let mut total_ignored: usize = 0;
    let mut total_filtered: usize = 0;
    let mut total_duration: f64 = 0.0;
    let mut suites: usize = 0;
    let mut has_duration = false;

    for line in summary_lines {
        if !line.contains("test result: ok.") {
            return None;
        }

        suites += 1;

        if let Some(n) = parse_count(line, "passed") {
            total_passed += n;
        }
        if let Some(n) = parse_count(line, "failed") {
            total_failed += n;
        }
        if let Some(n) = parse_count(line, "ignored") {
            total_ignored += n;
        }
        if let Some(n) = parse_count(line, "filtered out") {
            total_filtered += n;
        }
        if let Some(d) = parse_duration(line) {
            total_duration += d;
            has_duration = true;
        }
    }

    if suites == 0 {
        return None;
    }

    let mut parts = vec![format!("{total_passed} passed")];
    if total_failed > 0 {
        parts.push(format!("{total_failed} failed"));
    }
    if total_ignored > 0 {
        parts.push(format!("{total_ignored} ignored"));
    }
    if total_filtered > 0 {
        parts.push(format!("{total_filtered} filtered out"));
    }

    let counts = parts.join(", ");
    let suite_text = if suites == 1 {
        "1 suite".to_string()
    } else {
        format!("{suites} suites")
    };

    if has_duration {
        Some(format!(
            "cargo test: {counts} ({suite_text}, {total_duration:.2}s)"
        ))
    } else {
        Some(format!("cargo test: {counts} ({suite_text})"))
    }
}

/// Parse "N <label>" from a test result line.
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

#[cfg(test)]
mod tests {
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
        // Error line: one-liner with code, location, message.
        assert!(
            result.contains("error[E0308] src/foo.rs:20:5 mismatched types"),
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
        // Each warning is its own line — no grouping.
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
