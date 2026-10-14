//! Filters raw cargo output into one-line-per-diagnostic summaries.
//!
//! Instead of dumping hundreds of lines of cargo decoration (source excerpts,
//! pipe characters, help suggestions, diff hunks), each diagnostic becomes a
//! single line: `error[CODE] file:line:col message`
//!
//! Libtest output belongs to the test execution path in `test_runner::output`.
//! This module only reads rustc-shaped text for commands that still receive
//! rendered diagnostics.
//!
//! Both the cargo JSON reader and this text reader produce the shared
//! [`Diagnostic`] model.

#[cfg(test)]
use std::path::Path;

use crate::diagnostic::{Diagnostic, OwnerIndex};
use crate::rustc_text::{self, BlockKind, Level};

/// Parsed clippy output.
///
/// `parse_failed` means the parsed list cannot explain the failure, and each
/// producer says when: the legacy text parser here sets it when cargo emitted
/// a well-formed `error:`/`warning:` header was present but no finding was
/// extracted, and callers print the raw output instead of the list; `check`'s
/// JSON path sets it when a
/// failed run left no error-level diagnostic, and prints the captured streams
/// after the list (`check_cmd::phase::clippy_parse_from_events`).
pub struct ClippyParse {
    pub diagnostics: Vec<Diagnostic>,
    pub parse_failed: bool,
}

/// Parse cargo clippy text output into structured diagnostics.
pub fn parse_clippy(output: &str) -> ClippyParse {
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut saw_header = false;
    let mut kept: Vec<&str> = Vec::new();
    for line in output.lines() {
        let kind = rustc_text::classify_line(line);
        if matches!(
            kind,
            BlockKind::Finding | BlockKind::FailureSummary | BlockKind::CargoStatus
        ) {
            saw_header = true;
        }
        if !matches!(
            kind,
            BlockKind::FailureSummary | BlockKind::CargoStatus | BlockKind::Progress
        ) {
            kept.push(line);
        }
    }
    let filtered_output = kept.join("\n");
    for block in rustc_text::rustc_blocks(&filtered_output) {
        if rustc_text::classify_block(&block) != BlockKind::Finding {
            continue;
        }
        let lines = block.text.lines().map(str::to_owned).collect::<Vec<_>>();
        diagnostics.push(parse_block(&lines, block.level));
    }

    // Errors first, then warnings; each half keeps discovery order.
    let (errors, warnings): (Vec<_>, Vec<_>) =
        diagnostics.into_iter().partition(Diagnostic::is_error);
    let mut sorted = errors;
    sorted.extend(warnings);

    let parse_failed = sorted.is_empty() && saw_header;

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
    filter_clippy_with_owners(output, None)
}

/// Like [`filter_clippy`], resolving warning ownership from cargo metadata.
pub fn filter_clippy_with_owners(output: &str, owners: Option<&OwnerIndex>) -> String {
    format_clippy_for(output, "cargo clippy", owners)
        .unwrap_or_else(|| "cargo clippy: no issues".into())
}

/// Format diagnostics for a cargo command. `None` means there was no finding;
/// parse failures retain the raw stream so the caller never loses an
/// unexplained build failure.
fn format_clippy_for(output: &str, tool: &str, owners: Option<&OwnerIndex>) -> Option<String> {
    let mut parsed = parse_clippy(output);
    if parsed.parse_failed {
        return Some(output.to_string());
    }
    if let Some(owners) = owners {
        for diagnostic in &mut parsed.diagnostics {
            diagnostic.classify_owner(owners);
        }
        parsed.diagnostics.retain(|d| !d.is_dependency_warning());
    }
    if parsed.diagnostics.is_empty() {
        return None;
    }

    let errors = parsed.diagnostics.iter().filter(|d| d.is_error()).count();
    let warnings = parsed.diagnostics.len() - errors;

    let mut result = format!(
        "{tool}: {}, {}\n",
        crate::output::count(errors, "error"),
        crate::output::count(warnings, "warning")
    );
    for d in &parsed.diagnostics {
        result.push_str("  ");
        result.push_str(&d.format_one());
        result.push('\n');
    }
    Some(result.trim_end().to_string())
}

/// Format a `cargo test` run that died in the build phase: the clippy-style
/// one-line diagnostics labeled as `cargo test` - the command that actually
/// ran - plus [`linker_failure_detail`] when applicable. Shared by
/// [`crate::test_runner::filter_test`]'s compile-error branch and `brokkr test`'s BUILD FAILED
/// reporter, so both paths label the failure identically.
pub(crate) fn filter_test_build_failure(stderr: &str) -> String {
    let mut filtered = format_clippy_for(stderr, "cargo test", None)
        .unwrap_or_else(|| "cargo test: build failed without a parsed diagnostic".into());
    if let Some(extra) = linker_failure_detail(stderr) {
        filtered.push('\n');
        filtered.push_str(&extra);
    }
    filtered
}

/// Extra detail for a `linking with ...` build failure. The one-line
/// condensation keeps `` error: linking with `clang` failed `` and drops the
/// `= note:` block - which is where the linker put the actual cause. Pull
/// the undefined-symbol lines back out, and when they carry the
/// stale-incremental signature - a symbol in the failing crate's own
/// namespace, referenced by an `.rcgu.o` incremental codegen unit - name
/// the fix: `brokkr clean --cargo <pkg>`. A genuinely missing symbol from
/// another crate matches neither the namespace nor the rcgu test, so the
/// hint stays off the ordinary link-error path.
fn linker_failure_detail(stderr: &str) -> Option<String> {
    if !stderr.contains("linking with ") {
        return None;
    }
    let undefined: Vec<&str> = stderr
        .lines()
        .map(|l| {
            let t = l.trim_start();
            t.strip_prefix("= note: ").unwrap_or(t)
        })
        .filter(|t| {
            let lower = t.to_lowercase();
            lower.contains("undefined symbol") || lower.contains("undefined reference")
        })
        .collect();
    if undefined.is_empty() {
        return None;
    }

    let mut out: Vec<String> = undefined.iter().map(|t| format!("  linker: {t}")).collect();

    // `error: could not compile `pkg` (lib test)` names the crate whose
    // build cache is suspect; its symbols are `pkg`-with-underscores paths.
    let package = stderr.lines().find_map(|l| {
        let rest = l.trim_start().strip_prefix("error: could not compile `")?;
        let (pkg, _) = rest.split_once('`')?;
        Some(pkg.to_string())
    });
    if let Some(pkg) = package {
        let ident = format!("{}::", pkg.replace('-', "_"));
        if stderr.contains(".rcgu.o") && undefined.iter().any(|t| t.contains(&ident)) {
            out.push(format!(
                "  stale incremental cache suspected (phantom symbol in an \
                 incremental codegen unit) - run: brokkr clean --cargo {pkg}"
            ));
        }
    }
    Some(out.join("\n"))
}

/// Parse a diagnostic block into structured fields.
///
/// `["error[E0425]: cannot find value ...", " --> src/foo.rs:10:5", ...]`
/// → `Diagnostic { level: Error, code: Some(LintName::parse("E0425")), site: ..., ... }`
fn parse_block(block: &[String], level: Level) -> Diagnostic {
    let (mut header, message) = parse_header(&block[0]);
    // If the header didn't carry a [rule], try to recover one from a
    // `= note: \`#[warn(rule)]\`` line in the block. Real cargo/clippy
    // emits the rule there, not in the header.
    if (header == "warning" || header == "error")
        && let Some(rule) = extract_rule_from_notes(block)
    {
        header = format!("{header}[{rule}]");
    }
    let mut diagnostic = Diagnostic::new(level, code_from_header(&header), message);
    diagnostic.site = extract_location(block).as_deref().and_then(parse_site);
    diagnostic.detail = extract_detail(block);
    diagnostic
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
/// `"error[E0308]: mismatched types"` becomes `("error[E0308]", "mismatched types")`;
/// `` "warning: unused variable: `x` [unused_variables]" `` becomes
/// `` ("warning[unused_variables]", "unused variable: `x`") ``.
fn parse_header(line: &str) -> (String, String) {
    // error[CODE]: message
    if (line.starts_with("error[") || line.starts_with("warning["))
        && let Some(bracket_end) = line.find(']')
    {
        let prefix = &line[..bracket_end + 1];
        let message = line[bracket_end + 1..].trim_start_matches(':').trim();
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
        && bracket_end > bracket_start
    {
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

fn code_from_header(header: &str) -> Option<String> {
    let start = header.find('[')? + 1;
    let end = header[start..].find(']')? + start;
    (end > start).then(|| header[start..end].to_owned())
}

fn parse_site(location: &str) -> Option<crate::diagnostic::Site> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    let mut parts = location.rsplitn(3, ':');
    let parsed = match (parts.next(), parts.next(), parts.next()) {
        (Some(column), Some(line), Some(file)) => match (line.parse(), column.parse()) {
            (Ok(line), Ok(column)) => Some((file, Some(line), Some(column))),
            _ => None,
        },
        _ => None,
    };
    let (file, line, column) = parsed.unwrap_or((location, None, None));
    Some(crate::diagnostic::Site {
        file: std::path::PathBuf::from(file),
        line,
        column,
    })
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
    use crate::test_runner::filter_test;

    /// Filter with ownership resolved against a workspace at `/home/me/proj`
    /// whose one member is the root package.
    fn filter_clippy_in_tree(output: &str) -> String {
        let root = Path::new("/home/me/proj");
        let manifests = std::collections::HashMap::from([(
            root.join("Cargo.toml"),
            "path+file:///home/me/proj#proj@0.1.0".to_owned(),
        )]);
        let owners =
            OwnerIndex::from_metadata_at(root, root, &std::collections::HashMap::new(), &manifests);
        filter_clippy_with_owners(output, Some(&owners))
    }

    #[test]
    fn foreign_manifest_warnings_are_dropped() {
        let output = "\
warning: /elsewhere/iced/Cargo.toml: `lints.clippy.map-entry` is deprecated
warning: /home/me/proj/Cargo.toml: `lints.clippy.map-entry` is deprecated
warning: unused variable: `x`
 --> src/a.rs:1:9
";
        let filtered = filter_clippy_in_tree(output);
        assert!(!filtered.contains("/elsewhere/"));
        assert!(filtered.contains("/home/me/proj/Cargo.toml"));
        assert!(filtered.contains("unused variable"));
        assert!(filtered.starts_with("cargo clippy: 0 errors, 2 warnings"));
    }

    #[test]
    fn foreign_manifest_warnings_only_dropped_when_scoped() {
        let output =
            "warning: /elsewhere/iced/Cargo.toml: `lints.clippy.map-entry` is deprecated\n";
        assert!(filter_clippy(output).contains("/elsewhere/"));
        assert_eq!(filter_clippy_in_tree(output), "cargo clippy: no issues");
    }

    #[test]
    fn linker_failure_surfaces_symbol_and_stale_hint() {
        // The wild-linker stale-incremental shape: a phantom symbol in the
        // failing crate's own namespace, referenced by an .rcgu.o.
        let stderr = "\
error: linking with `clang` failed: exit status: 1
  |
  = note: some arguments are omitted. use `--verbose` to show all linker arguments
  = note: wild: error: Undefined symbol broadarrow_web_bridge::tests::sample_identity, referenced by
              /home/folk/Programs/broadarrow/target/debug/deps/broadarrow_web_bridge-49b43a2d64958f14.ba4nwrzbiwcfuixpjcdn6vpzx.02tc9cg.rcgu.o
          clang: error: linker command failed with exit code 255 (use -v to see invocation)

error: could not compile `broadarrow-web-bridge` (lib test) due to 1 previous error
";
        let result = filter_test("", stderr);
        assert!(
            result.contains("Undefined symbol broadarrow_web_bridge::tests::sample_identity"),
            "got: {result}"
        );
        assert!(
            result.contains("brokkr clean --cargo broadarrow-web-bridge"),
            "got: {result}"
        );
    }

    #[test]
    fn linker_failure_foreign_symbol_gets_no_stale_hint() {
        // A genuinely missing symbol from another crate: surface the linker
        // note, but the stale-incremental hint must not fire.
        let stderr = "\
error: linking with `cc` failed: exit status: 1
  |
  = note: /usr/bin/ld: main.o: undefined reference to `sqlite3_open'

error: could not compile `mycrate` (bin \"mycrate\") due to 1 previous error
";
        let result = filter_test("", stderr);
        assert!(
            result.contains("undefined reference to `sqlite3_open'"),
            "got: {result}"
        );
        assert!(!result.contains("brokkr clean"), "got: {result}");
    }

    #[test]
    fn clippy_clean() {
        let output = "    Checking pbfhogg v0.1.0\n    Finished dev target(s) in 1.53s\n";
        let result = filter_clippy(output);
        assert_eq!(result, "cargo clippy: no issues");
    }

    #[test]
    fn cargo_test_build_failure_keeps_its_tool_label() {
        let stderr = "error[E0308]: mismatched types\n --> src/foo.rs:20:5\n  |\n20|     \"hello\"\n  |     ^^^^^^^ expected `i32`, found `&str`\n  |\nerror: aborting due to 1 previous error\n";
        let result = filter_test_build_failure(stderr);
        assert!(
            result.starts_with("cargo test: 1 error, 0 warnings"),
            "got: {result}"
        );
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
        assert!(
            result.starts_with("cargo clippy: 1 error, 1 warning"),
            "got: {result}"
        );
        // Error line: one-liner with code, location, message, and type detail.
        assert!(
            result.contains(
                "error[E0308] src/foo.rs:20:5 mismatched types - expected `i32`, found `&str`"
            ),
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
        assert!(
            error_pos < warning_pos,
            "errors should come before warnings: {result}"
        );
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
        assert_eq!(
            result.matches("warning[unused_variables]").count(),
            3,
            "got: {result}"
        );
        assert!(
            result.contains("warning[clippy::needless_return]"),
            "got: {result}"
        );
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
        assert!(result.contains("1 error, 0 warnings"), "got: {result}");
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
        assert!(parsed.diagnostics[0].is_error());
        assert_eq!(parsed.diagnostics[0].code.as_deref(), Some("E0308"));
        assert_eq!(
            parsed.diagnostics[0].location().as_deref(),
            Some("src/foo.rs:20:5")
        );
        assert_eq!(parsed.diagnostics[0].message, "mismatched types");
        assert!(!parsed.diagnostics[1].is_error());
        assert_eq!(
            parsed.diagnostics[1].code.as_deref(),
            Some("unused_variables")
        );
    }

    #[test]
    fn clippy_diagnostic_path_extracts_file() {
        let mut d = Diagnostic::new(
            Level::Error,
            Some("E0308".into()),
            "mismatched types".into(),
        );
        d.site = Some(crate::diagnostic::Site {
            file: "src/foo.rs".into(),
            line: Some(20),
            column: Some(5),
        });
        assert_eq!(d.path(), Some(Path::new("src/foo.rs")));
    }

    #[test]
    fn clippy_diagnostic_path_none_without_location() {
        let d = Diagnostic::new(Level::Warning, None, "something".into());
        assert!(d.path().is_none());
    }

    #[test]
    fn clippy_diagnostic_format_one_matches_filter_output() {
        let mut d = Diagnostic::new(
            Level::Error,
            Some("E0308".into()),
            "mismatched types".into(),
        );
        d.site = Some(crate::diagnostic::Site {
            file: "src/foo.rs".into(),
            line: Some(20),
            column: Some(5),
        });
        d.detail = Some("expected `i32`, found `&str`".into());
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
