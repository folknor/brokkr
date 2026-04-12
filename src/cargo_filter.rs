//! Filters raw cargo output into structured summaries.
//!
//! Instead of dumping hundreds of lines of `Compiling …` / `Checking …` noise
//! followed by buried errors, these filters extract the actionable information
//! and present it in a compact, structured format.

use std::collections::HashMap;

/// Filter cargo clippy output — group warnings by lint rule, show errors first.
///
/// Output format:
/// ```text
/// cargo clippy: 2 errors, 11 warnings
/// ═══════════════════════════════════════
///
/// Errors:
///   1. error[E0308]: mismatched types
///      --> src/foo.rs:10:5
///
///   2. error: casting `u64` to `usize` may truncate
///      --> src/commands/renumber_external.rs:2352:61
///
/// Warnings by lint:
///   clippy::same_item_push (3x)
///     src/foo.rs:10:5
///     src/bar.rs:20:3
///     ... +1 more
/// ```
pub fn filter_clippy(output: &str) -> String {
    let mut by_rule: HashMap<String, Vec<String>> = HashMap::new();
    let mut error_blocks: Vec<String> = Vec::new();
    let mut warning_count: usize = 0;
    let mut error_count: usize = 0;
    let mut compiled: usize = 0;

    let mut in_block = false;
    let mut current_block = Vec::new();
    let mut current_is_error = false;
    let mut current_rule = String::new();

    for line in output.lines() {
        let trimmed = line.trim_start();

        // Skip compilation noise.
        if trimmed.starts_with("Compiling")
            || trimmed.starts_with("Checking")
            || trimmed.starts_with("Downloading")
            || trimmed.starts_with("Downloaded")
            || trimmed.starts_with("Finished")
            || trimmed.starts_with("Locking")
            || trimmed.starts_with("Updating")
        {
            compiled += 1;
            continue;
        }

        // Skip summary lines: "warning: `crate` (lib) generated N warnings"
        if line.starts_with("warning:")
            && line.contains("generated")
            && line.contains("warning")
        {
            continue;
        }

        // Skip "error: aborting …" / "error: could not compile …"
        if (line.starts_with("error:") || line.starts_with("error["))
            && (line.contains("aborting due to") || line.contains("could not compile"))
        {
            continue;
        }

        // Detect new error/warning block.
        let is_error_start = line.starts_with("error:") || line.starts_with("error[");
        let is_warning_start = line.starts_with("warning:") || line.starts_with("warning[");

        if is_error_start || is_warning_start {
            // Flush previous block.
            flush_block(
                &mut in_block,
                &mut current_block,
                current_is_error,
                &current_rule,
                &mut error_blocks,
                &mut by_rule,
            );

            if is_error_start {
                error_count += 1;
                current_is_error = true;
            } else {
                warning_count += 1;
                current_is_error = false;
            }

            // Extract rule name from brackets: "warning: unused [unused_variables]"
            current_rule = extract_rule(line);

            in_block = true;
            current_block.push(line.to_string());
        } else if in_block {
            // Blank line after enough context ends the block.
            if line.trim().is_empty() && current_block.len() > 3 {
                flush_block(
                    &mut in_block,
                    &mut current_block,
                    current_is_error,
                    &current_rule,
                    &mut error_blocks,
                    &mut by_rule,
                );
            } else {
                current_block.push(line.to_string());
            }
        }
    }

    // Flush trailing block.
    flush_block(
        &mut in_block,
        &mut current_block,
        current_is_error,
        &current_rule,
        &mut error_blocks,
        &mut by_rule,
    );

    format_clippy_result(error_count, warning_count, compiled, &error_blocks, &by_rule)
}

/// Format parsed clippy results into a structured summary string.
fn format_clippy_result(
    error_count: usize,
    warning_count: usize,
    compiled: usize,
    error_blocks: &[String],
    by_rule: &HashMap<String, Vec<String>>,
) -> String {
    // No issues — clean pass.
    if error_count == 0 && warning_count == 0 {
        if compiled > 0 {
            return format!("cargo clippy: no issues ({compiled} crates checked)");
        }
        return "cargo clippy: no issues".into();
    }

    let mut result = format!("cargo clippy: {error_count} errors, {warning_count} warnings\n");
    result.push_str("═══════════════════════════════════════\n");

    // Errors first — these are what need fixing.
    if !error_blocks.is_empty() {
        result.push('\n');
        for (i, block) in error_blocks.iter().enumerate().take(15) {
            result.push_str(&format!("  {}. ", i + 1));
            result.push_str(block);
            result.push('\n');
        }
        if error_blocks.len() > 15 {
            result.push_str(&format!("\n  ... +{} more errors\n", error_blocks.len() - 15));
        }
    }

    // Warnings grouped by rule, sorted by frequency.
    if !by_rule.is_empty() {
        result.push_str("\nWarnings by lint:\n");
        let mut rule_counts: Vec<_> = by_rule.iter().collect();
        rule_counts.sort_by_key(|&(_, locs)| std::cmp::Reverse(locs.len()));

        for (rule, locations) in rule_counts.iter().take(15) {
            result.push_str(&format!("  {} ({}x)\n", rule, locations.len()));
            for loc in locations.iter().take(3) {
                result.push_str(&format!("    {loc}\n"));
            }
            if locations.len() > 3 {
                result.push_str(&format!("    ... +{} more\n", locations.len() - 3));
            }
        }
        if by_rule.len() > 15 {
            result.push_str(&format!("\n  ... +{} more lint rules\n", by_rule.len() - 15));
        }
    }

    result.trim_end().to_string()
}

/// Filter cargo test output — show failures and compact summary.
///
/// On success:
/// ```text
/// cargo test: 137 passed (4 suites, 1.45s)
/// ```
///
/// On failure:
/// ```text
/// cargo test: 2 failures
/// ═══════════════════════════════════════
///
///   1. tests::failing_test
///      thread 'tests::failing_test' panicked at src/lib.rs:15:9:
///      assertion `left == right` failed
///
///   2. tests::another_failing
///      thread 'tests::another_failing' panicked at src/lib.rs:20:9:
///      something went wrong
///
/// test result: FAILED. 14 passed; 2 failed; 0 ignored
/// ```
///
/// On compilation error (no test results at all):
/// Falls back to `filter_clippy` to show the build errors.
pub fn filter_test(stdout: &str, stderr: &str) -> String {
    // Check if this is actually a compilation failure (no test results in stdout).
    let has_test_result = stdout.lines().any(|l| l.starts_with("test result:"));
    let has_compile_error = stderr.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("error[") || (t.starts_with("error:") && !t.contains("test run failed"))
    });

    if !has_test_result && has_compile_error {
        // Compilation failed before tests could run. Use clippy filter on stderr
        // which handles build errors the same way.
        let filtered = filter_clippy(stderr);
        if filtered.starts_with("cargo clippy:") {
            return filtered.replacen("cargo clippy:", "cargo test:", 1);
        }
        return filtered;
    }

    // Parse test results and failures from stdout.
    //
    // Cargo test output has TWO "failures:" sections:
    //   1. Detail section with "---- test_name stdout ----" blocks
    //   2. Summary section with just indented test names
    // We only want the first (detail) section.
    let mut failures: Vec<String> = Vec::new();
    let mut summary_lines: Vec<String> = Vec::new();
    let mut in_failure_detail = false;
    let mut seen_failure_section = false;
    let mut current_failure = Vec::new();

    for line in stdout.lines() {
        // Skip compilation and running lines.
        if line.trim_start().starts_with("Compiling")
            || line.trim_start().starts_with("Downloading")
            || line.trim_start().starts_with("Finished")
            || line.starts_with("running ")
            || (line.starts_with("test ") && line.ends_with("... ok"))
        {
            continue;
        }

        if line == "failures:" {
            if !seen_failure_section {
                // First "failures:" — detail section with stdout/stderr blocks.
                in_failure_detail = true;
                seen_failure_section = true;
            } else {
                // Second "failures:" — just test name list, stop collecting.
                in_failure_detail = false;
                if !current_failure.is_empty() {
                    failures.push(current_failure.join("\n"));
                    current_failure.clear();
                }
            }
            continue;
        }

        if in_failure_detail {
            if line.starts_with("test result:") {
                in_failure_detail = false;
                summary_lines.push(line.to_string());
            } else if line.starts_with("    ") || line.starts_with("---- ") {
                current_failure.push(line.to_string());
            } else if line.trim().is_empty() && !current_failure.is_empty() {
                failures.push(current_failure.join("\n"));
                current_failure.clear();
            } else if !line.trim().is_empty() {
                current_failure.push(line.to_string());
            }
        }

        // Capture test result summaries (outside failure sections too).
        if !in_failure_detail && line.starts_with("test result:") {
            summary_lines.push(line.to_string());
        }
    }

    if !current_failure.is_empty() {
        failures.push(current_failure.join("\n"));
    }

    // All passed — aggregate into compact format.
    if failures.is_empty() && !summary_lines.is_empty() {
        if let Some(agg) = aggregate_test_results(&summary_lines) {
            return agg;
        }
        // Fallback: show raw summary lines.
        return summary_lines.join("\n");
    }

    // Failures present.
    let mut result = String::new();
    if !failures.is_empty() {
        result.push_str(&format!(
            "cargo test: {} failure{}\n",
            failures.len(),
            if failures.len() == 1 { "" } else { "s" }
        ));
        result.push_str("═══════════════════════════════════════\n\n");

        for (i, failure) in failures.iter().enumerate().take(10) {
            result.push_str(&format!("  {}. ", i + 1));
            // Indent continuation lines.
            for (j, fline) in failure.lines().enumerate() {
                if j == 0 {
                    result.push_str(fline.trim());
                    result.push('\n');
                } else {
                    result.push_str(&format!("     {}\n", fline.trim()));
                }
            }
            result.push('\n');
        }
        if failures.len() > 10 {
            result.push_str(&format!("  ... +{} more failures\n\n", failures.len() - 10));
        }
    }

    for line in &summary_lines {
        result.push_str(line);
        result.push('\n');
    }

    result.trim_end().to_string()
}

// --- helpers ---

/// Extract a lint rule name from brackets in a warning/error line.
/// "warning: unused variable [unused_variables]" → "unused_variables"
fn extract_rule(line: &str) -> String {
    if let Some(bracket_start) = line.rfind('[')
        && let Some(bracket_end) = line.rfind(']')
        && bracket_end > bracket_start
    {
        return line[bracket_start + 1..bracket_end].to_string();
    }
    // No bracket — use the message itself as the key.
    let prefix = if line.starts_with("error") {
        "error: "
    } else {
        "warning: "
    };
    line.strip_prefix(prefix)
        .unwrap_or(line)
        .to_string()
}

/// Extract a `-->` location from a block of lines.
fn extract_location(block: &[String]) -> Option<String> {
    for line in block {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--> ") {
            return Some(trimmed.trim_start_matches("--> ").to_string());
        }
    }
    None
}

/// Flush the current block into either error_blocks or by_rule.
fn flush_block(
    in_block: &mut bool,
    current_block: &mut Vec<String>,
    is_error: bool,
    rule: &str,
    error_blocks: &mut Vec<String>,
    by_rule: &mut HashMap<String, Vec<String>>,
) {
    if !*in_block || current_block.is_empty() {
        return;
    }

    if is_error {
        // For errors, keep the full block (header + location + context).
        error_blocks.push(current_block.join("\n"));
    } else {
        // For warnings, just track rule + location.
        let location = extract_location(current_block).unwrap_or_default();
        if !rule.is_empty() {
            by_rule
                .entry(rule.to_string())
                .or_default()
                .push(location);
        }
    }

    current_block.clear();
    *in_block = false;
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
        // "test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
        if !line.contains("test result: ok.") {
            // Has a non-ok result — don't aggregate, show raw.
            return None;
        }

        suites += 1;

        // Parse counts with simple string scanning.
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

/// Parse "N <label>" from a test result line. E.g. "15 passed" → 15.
fn parse_count(line: &str, label: &str) -> Option<usize> {
    let idx = line.find(label)?;
    // Walk backwards from idx to find the number.
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
        assert!(result.contains("no issues"), "got: {result}");
    }

    #[test]
    fn clippy_errors_before_warnings() {
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
        assert!(result.contains("1 errors, 1 warnings"), "got: {result}");
        // Errors section should appear.
        assert!(result.contains("E0308"), "got: {result}");
        // Warnings grouped by rule.
        assert!(result.contains("unused_variables (1x)"), "got: {result}");
        // Noise stripped.
        assert!(!result.contains("aborting"), "got: {result}");
        assert!(!result.contains("generated"), "got: {result}");
    }

    #[test]
    fn clippy_multiple_warnings_same_rule() {
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
        assert!(result.contains("unused_variables (3x)"), "got: {result}");
        assert!(
            result.contains("clippy::needless_return (1x)"),
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
        assert!(
            result.contains("cargo test: 15 passed (1 suite, 0.01s)"),
            "got: {result}"
        );
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
        assert!(
            result.contains("cargo test: 80 passed (2 suites, 0.75s)"),
            "got: {result}"
        );
    }

    #[test]
    fn test_failures_shown() {
        let stdout = "\
running 5 tests
test foo::test_a ... ok
test foo::test_b ... FAILED
test foo::test_c ... ok

failures:

---- foo::test_b stdout ----
thread 'foo::test_b' panicked at 'assert_eq!(1, 2)'

failures:
    foo::test_b

test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";
        let result = filter_test(stdout, "");
        assert!(result.contains("1 failure"), "got: {result}");
        assert!(result.contains("test_b"), "got: {result}");
        assert!(result.contains("test result:"), "got: {result}");
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
        assert!(result.contains("E0425"), "got: {result}");
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
