//! JSON event model and parser for `brokkr check --json`.
//!
//! Parses cargo's `--message-format=json` output into structured diagnostic
//! events and emits them as NDJSON (one JSON object per line on stdout).

use serde::Serialize;

// --- Event model ---

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckEvent {
    Diagnostic(DiagnosticEvent),
    DependencyViolation(DependencyViolationEvent),
    TestFailure(TestFailureEvent),
    TestHung(TestHungEvent),
    TestTiming(TestTimingEvent),
    DiagnosticSummary(DiagnosticSummaryEvent),
    DependencySummary(DependencySummaryEvent),
    TestSummary(TestSummaryEvent),
    Gremlin(GremlinEvent),
    GremlinSummary(GremlinSummaryEvent),
    Style(StyleEvent),
    StyleSummary(StyleSummaryEvent),
    Header(HeaderEvent),
    HeaderSummary(HeaderSummaryEvent),
    Textlint(TextlintEvent),
    TextlintSummary(TextlintSummaryEvent),
    Manifest(ManifestEvent),
    ManifestSummary(ManifestSummaryEvent),
}

/// One `[manifest]` structural violation.
#[derive(Serialize)]
pub struct ManifestEvent {
    pub file: String,
    pub rule: &'static str,
}

/// Closing tally for the `[manifest]` phase.
#[derive(Serialize)]
pub struct ManifestSummaryEvent {
    pub status: &'static str,
    pub violations: usize,
}

/// One `[header]` missing/stale-header violation.
#[derive(Serialize)]
pub struct HeaderEvent {
    pub file: String,
}

/// Closing tally for the `[header]` phase.
#[derive(Serialize)]
pub struct HeaderSummaryEvent {
    pub status: &'static str,
    pub violations: usize,
}

/// One `[[textlint]]` violation.
#[derive(Serialize)]
pub struct TextlintEvent {
    pub file: String,
    pub line: usize,
    pub rule: String,
}

/// Closing tally for the `[[textlint]]` phase.
#[derive(Serialize)]
pub struct TextlintSummaryEvent {
    pub status: &'static str,
    pub violations: usize,
}

/// One `[style]` blank-line violation.
#[derive(Serialize)]
pub struct StyleEvent {
    pub file: String,
    pub line: usize,
    pub keyword: &'static str,
}

/// Closing tally for the `[style]` phase.
#[derive(Serialize)]
pub struct StyleSummaryEvent {
    pub status: &'static str,
    pub violations: usize,
}

/// Per-test wall-clock timing observed by the libtest watchdog tracker.
/// Emitted in `--json` mode when `--timings` is set, one per completed
/// test (in observation order, not sorted - the consumer sorts).
#[derive(Serialize)]
pub struct TestTimingEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep: Option<String>,
    pub name: String,
    pub elapsed_seconds: f64,
}

#[derive(Serialize)]
pub struct DiagnosticEvent {
    pub tool: &'static str,
    /// Which sweeps this diagnostic appeared in (e.g.
    /// `["all-features"]`, `["consumer"]`, or both for clippy;
    /// user-defined names like `["all", "consumer"]` for profile-driven
    /// runs). Empty for single-sweep / unsweept events.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sweeps: Vec<String>,
    pub level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u64>,
    /// Inline label on the primary span, e.g. "expected `i32`, found `&str`".
    /// Captured from cargo's JSON `spans[].label` so the text renderer can
    /// surface the same one-line detail it used to scrape from the rendered
    /// source annotation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_label: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered: Option<String>,
}

#[derive(Serialize)]
pub struct DependencyViolationEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    pub from: String,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub optional: bool,
}

#[derive(Serialize)]
pub struct DependencySummaryEvent {
    pub status: &'static str,
    pub rules: usize,
    pub packages: usize,
    pub violations: usize,
}

#[derive(Serialize)]
pub struct ChildDiagnostic {
    pub level: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u64>,
}

#[derive(Serialize)]
pub struct TestFailureEvent {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Serialize)]
pub struct TestHungEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep: Option<String>,
    pub name: String,
    pub elapsed_seconds: f64,
    pub snapshot_dir: String,
    pub cargo_pid: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_pids: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wchan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_error: Option<String>,
}

#[derive(Serialize)]
pub struct DiagnosticSummaryEvent {
    pub tool: &'static str,
    /// Which sweep this summary belongs to. `None` for tools / runs with
    /// no sweep distinction (single-sweep clippy, single-shot test).
    /// Set for multi-sweep clippy and profile-driven tests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep: Option<String>,
    pub status: &'static str,
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Serialize)]
pub struct TestSummaryEvent {
    pub status: &'static str,
    /// Profile sweep label when running under `[test.profiles.*]`;
    /// `None` for single-shot test runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep: Option<String>,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub filtered_out: usize,
    pub suites: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
}

#[derive(Serialize)]
pub struct GremlinEvent {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub codepoint: String,
    pub name: &'static str,
}

#[derive(Serialize)]
pub struct GremlinSummaryEvent {
    pub status: &'static str,
    pub found: usize,
}

// --- Parsing ---

/// Parse cargo `--message-format=json` stdout into diagnostic events.
///
/// Each line is a JSON object from cargo. Only lines with
/// `"reason": "compiler-message"` are extracted; everything else is skipped.
///
/// `sweep` tags every emitted Diagnostic with the sweep label (e.g.
/// `"all-features"`, `"consumer"`, or a profile-defined sweep name).
/// `None` for non-clippy tools and single-sweep runs - the field stays
/// empty in that case.
#[allow(clippy::too_many_lines)] // JSON walk - splitting just shuffles match arms
pub fn parse_cargo_diagnostics(
    stdout: &str,
    tool: &'static str,
    sweep: Option<&str>,
) -> Vec<CheckEvent> {
    let mut events = Vec::new();

    for line in stdout.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(|v| v.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = val.get("message") else {
            continue;
        };

        let level = msg
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string);

        let message = msg
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Whether this compiler-message carries any spans. rustc's summary
        // and meta-noise messages ("N warnings emitted", "generated N
        // warnings", "aborting due to N previous errors") always arrive with
        // an empty `spans` array and a null `code`.
        let spans_empty = msg
            .get("spans")
            .and_then(|s| s.as_array())
            .is_none_or(Vec::is_empty);

        // Skip rustc/cargo summary + meta-noise diagnostics. The old text
        // scraper filtered these (`cargo_filter::is_meta_noise`); the JSON
        // path must do the same or the phantom, location-less messages inflate
        // the lint count (3 lints reported as "4 errors"). A real lint/error
        // always carries a primary span or a diagnostic code, so gating on
        // `spans_empty && code.is_none()` cannot drop a genuine finding.
        if is_summary_noise(&message, spans_empty, code.is_some()) {
            continue;
        }

        // Primary span for file/line/column.
        let primary_span = msg
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|spans| spans.iter().find(|s| s.get("is_primary") == Some(&serde_json::Value::Bool(true))));

        let (file, line_start, col_start, line_end, col_end, primary_label) = match primary_span {
            Some(span) => (
                span.get("file_name").and_then(|v| v.as_str()).map(std::string::ToString::to_string),
                span.get("line_start").and_then(serde_json::Value::as_u64),
                span.get("column_start").and_then(serde_json::Value::as_u64),
                span.get("line_end").and_then(serde_json::Value::as_u64),
                span.get("column_end").and_then(serde_json::Value::as_u64),
                span.get("label")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(std::string::ToString::to_string),
            ),
            None => (None, None, None, None, None, None),
        };

        // Child diagnostics.
        let children = msg
            .get("children")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|child| {
                        let child_level = child.get("level")?.as_str()?.to_string();
                        let child_msg = child.get("message")?.as_str()?.to_string();
                        if child_msg.is_empty() {
                            return None;
                        }
                        let child_span = child
                            .get("spans")
                            .and_then(|s| s.as_array())
                            .and_then(|spans| spans.first());
                        let (child_file, child_line, child_col) = match child_span {
                            Some(s) => (
                                s.get("file_name").and_then(|v| v.as_str()).map(std::string::ToString::to_string),
                                s.get("line_start").and_then(serde_json::Value::as_u64),
                                s.get("column_start").and_then(serde_json::Value::as_u64),
                            ),
                            None => (None, None, None),
                        };
                        Some(ChildDiagnostic {
                            level: child_level,
                            message: child_msg,
                            file: child_file,
                            line: child_line,
                            column: child_col,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let rendered = msg
            .get("rendered")
            .and_then(|v| v.as_str())
            .map(|s| s.trim_end().to_string());

        events.push(CheckEvent::Diagnostic(DiagnosticEvent {
            tool,
            sweeps: sweep.map(|s| vec![s.to_owned()]).unwrap_or_default(),
            level,
            code,
            message,
            file,
            line: line_start,
            column: col_start,
            end_line: line_end,
            end_column: col_end,
            primary_label,
            children,
            rendered,
        }));
    }

    events
}

/// Classify a compiler-message as a rustc/cargo summary or meta-noise line
/// that should not become a real `Diagnostic`.
///
/// Ported from the text-scraper's `cargo_filter::is_meta_noise`. These
/// messages arrive with no primary span and no diagnostic `code`, so the
/// caller gates on `spans_empty && !has_code` before matching message shape -
/// a genuine lint/error always carries a span or a code and is never dropped.
/// Matching is on message *shape* (substring patterns), never exact strings,
/// so `1 warning emitted` / `12 warnings emitted` / localized crate names all
/// fall through.
fn is_summary_noise(message: &str, spans_empty: bool, has_code: bool) -> bool {
    if !spans_empty || has_code {
        return false;
    }
    // "N warning(s) emitted"
    if message.contains("emitted") && message.contains("warning") {
        return true;
    }
    // "`crate` (lib) generated N warning(s)"
    if message.contains("generated") && message.contains("warning") {
        return true;
    }
    // "aborting due to N previous error(s)"
    if message.contains("aborting due to") {
        return true;
    }
    // "could not compile `crate` ..."
    if message.contains("could not compile") {
        return true;
    }
    // "build failed, waiting for other jobs to finish..."
    if message.contains("build failed") {
        return true;
    }
    false
}

// --- Emitter ---

/// Emit a single NDJSON event to stdout.
pub fn emit(event: &CheckEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        println!("{json}");
    }
}

/// Emit a synthetic error event when JSON parsing produces nothing but cargo failed.
///
/// Includes both stdout and stderr so the consumer can debug.
pub fn emit_parse_error(tool: &'static str, stdout: &str, stderr: &str) {
    let mut message = String::new();
    if !stderr.is_empty() {
        message.push_str(stderr);
    }
    if !stdout.is_empty() {
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(stdout);
    }
    if message.is_empty() {
        message = "cargo exited with non-zero status but produced no output".into();
    }

    emit(&CheckEvent::Diagnostic(DiagnosticEvent {
        tool,
        sweeps: Vec::new(),
        level: "error".into(),
        code: None,
        message,
        file: None,
        line: None,
        column: None,
        end_line: None,
        end_column: None,
        primary_label: None,
        children: Vec::new(),
        rendered: None,
    }));
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

    fn sample_compiler_message(level: &str, code: &str, message: &str, file: &str, line: u64) -> String {
        format!(
            r#"{{"reason":"compiler-message","message":{{"rendered":"rendered text","level":"{level}","code":{{"code":"{code}"}},"message":"{message}","spans":[{{"file_name":"{file}","line_start":{line},"column_start":5,"line_end":{line},"column_end":10,"is_primary":true}}],"children":[{{"level":"help","message":"try this","spans":[]}}]}}}}"#
        )
    }

    #[test]
    fn parse_captures_primary_span_label() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"file_name":"src/foo.rs","line_start":20,"column_start":5,"line_end":20,"column_end":10,"is_primary":true,"label":"expected `i32`, found `&str`"}],"children":[],"rendered":"rendered"}}"#;
        let events = parse_cargo_diagnostics(input, "clippy", None);
        assert_eq!(events.len(), 1);
        let CheckEvent::Diagnostic(d) = &events[0] else {
            panic!("expected Diagnostic event");
        };
        assert_eq!(d.primary_label.as_deref(), Some("expected `i32`, found `&str`"));
    }

    #[test]
    fn parse_omits_empty_primary_label() {
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":{"code":"unused_variables"},"message":"unused","spans":[{"file_name":"src/a.rs","line_start":1,"column_start":1,"line_end":1,"column_end":2,"is_primary":true,"label":""}],"children":[],"rendered":"rendered"}}"#;
        let events = parse_cargo_diagnostics(input, "clippy", None);
        let CheckEvent::Diagnostic(d) = &events[0] else {
            panic!("expected Diagnostic event");
        };
        assert!(d.primary_label.is_none());
    }

    #[test]
    fn parse_single_error() {
        let input = sample_compiler_message("error", "E0425", "cannot find value", "src/main.rs", 10);
        let events = parse_cargo_diagnostics(&input, "clippy", None);
        assert_eq!(events.len(), 1);
        if let CheckEvent::Diagnostic(d) = &events[0] {
            assert_eq!(d.tool, "clippy");
            assert_eq!(d.level, "error");
            assert_eq!(d.code.as_deref(), Some("E0425"));
            assert_eq!(d.message, "cannot find value");
            assert_eq!(d.file.as_deref(), Some("src/main.rs"));
            assert_eq!(d.line, Some(10));
            assert_eq!(d.column, Some(5));
            assert_eq!(d.children.len(), 1);
            assert_eq!(d.children[0].level, "help");
            assert_eq!(d.children[0].message, "try this");
        } else {
            panic!("expected Diagnostic event");
        }
    }

    #[test]
    fn skips_non_compiler_messages() {
        let input = r#"{"reason":"compiler-artifact","target":{"name":"foo"}}"#;
        let events = parse_cargo_diagnostics(input, "clippy", None);
        assert!(events.is_empty());
    }

    #[test]
    fn skips_aborting_errors() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":null,"message":"aborting due to 3 previous errors","spans":[],"children":[],"rendered":"error: aborting"}}"#;
        let events = parse_cargo_diagnostics(input, "clippy", None);
        assert!(events.is_empty());
    }

    #[test]
    fn real_clippy_warning_produces_one_diagnostic() {
        // A genuine lint: has both spans and a code -> must survive.
        let input = sample_compiler_message("warning", "clippy::needless_return", "unneeded return statement", "src/a.rs", 7);
        let events = parse_cargo_diagnostics(&input, "clippy", None);
        assert_eq!(events.len(), 1);
        let CheckEvent::Diagnostic(d) = &events[0] else {
            panic!("expected Diagnostic event");
        };
        assert_eq!(d.code.as_deref(), Some("clippy::needless_return"));
        assert_eq!(d.file.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn skips_warnings_emitted_summary() {
        // rustc's "N warnings emitted": empty spans, null code -> zero diagnostics.
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"2 warnings emitted","spans":[],"children":[],"rendered":"warning: 2 warnings emitted"}}"#;
        let events = parse_cargo_diagnostics(input, "clippy", None);
        assert!(events.is_empty(), "expected the summary line to be filtered, got {} event(s)", events.len());
    }

    #[test]
    fn skips_generated_warnings_summary() {
        // cargo's per-crate roll-up: "`crate` (lib) generated N warnings".
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"`brokkr` (lib) generated 3 warnings","spans":[],"children":[],"rendered":"warning: `brokkr` (lib) generated 3 warnings"}}"#;
        let events = parse_cargo_diagnostics(input, "clippy", None);
        assert!(events.is_empty(), "expected the generated-warnings roll-up to be filtered, got {} event(s)", events.len());
    }

    #[test]
    fn multiple_diagnostics() {
        let mut input = sample_compiler_message("error", "E0308", "mismatched types", "src/a.rs", 1);
        input.push('\n');
        input.push_str(&sample_compiler_message("warning", "unused_variables", "unused var", "src/b.rs", 2));
        let events = parse_cargo_diagnostics(&input, "test", None);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn parse_with_sweep_tags_diagnostics() {
        let input = sample_compiler_message("warning", "needless_pass_by_value", "x", "src/a.rs", 1);
        let events = parse_cargo_diagnostics(&input, "clippy", Some("consumer"));
        assert_eq!(events.len(), 1);
        if let CheckEvent::Diagnostic(d) = &events[0] {
            assert_eq!(d.sweeps, vec!["consumer"]);
        } else {
            panic!("expected Diagnostic event");
        }
    }

    #[test]
    fn emit_produces_valid_json() {
        let event = CheckEvent::DiagnosticSummary(DiagnosticSummaryEvent {
            tool: "clippy",
            sweep: None,
            status: "failed",
            errors: 2,
            warnings: 3,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"diagnostic_summary""#));
        assert!(json.contains(r#""tool":"clippy""#));
        assert!(json.contains(r#""errors":2"#));
    }

    #[test]
    fn test_summary_serialization() {
        let event = CheckEvent::TestSummary(TestSummaryEvent {
            status: "failed",
            sweep: None,
            passed: 10,
            failed: 1,
            ignored: 0,
            filtered_out: 5,
            suites: 2,
            duration_seconds: Some(1.45),
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"test_summary""#));
        assert!(json.contains(r#""passed":10"#));
        assert!(json.contains(r#""filtered_out":5"#));
        assert!(json.contains(r#""duration_seconds":1.45"#));
    }

    #[test]
    fn test_summary_with_sweep_label() {
        let event = CheckEvent::TestSummary(TestSummaryEvent {
            status: "ok",
            sweep: Some("consumer".into()),
            passed: 1,
            failed: 0,
            ignored: 0,
            filtered_out: 0,
            suites: 1,
            duration_seconds: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""sweep":"consumer""#));
    }

    #[test]
    fn test_failure_serialization() {
        let event = CheckEvent::TestFailure(TestFailureEvent {
            name: "foo::test_bar".into(),
            location: Some("src/lib.rs:15:9".into()),
            message: Some("assertion failed".into()),
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"test_failure""#));
        assert!(json.contains(r#""name":"foo::test_bar""#));
    }

    #[test]
    fn test_summary_omits_none_duration() {
        let event = CheckEvent::TestSummary(TestSummaryEvent {
            status: "ok",
            sweep: None,
            passed: 5,
            failed: 0,
            ignored: 0,
            filtered_out: 0,
            suites: 1,
            duration_seconds: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("duration_seconds"));
        assert!(!json.contains("sweep"));
    }

    #[test]
    fn gremlin_event_serialization() {
        let event = CheckEvent::Gremlin(GremlinEvent {
            file: "src/foo.rs".into(),
            line: 10,
            column: 5,
            codepoint: "U+200B".into(),
            name: "ZERO WIDTH SPACE",
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"gremlin""#));
        assert!(json.contains(r#""file":"src/foo.rs""#));
        assert!(json.contains(r#""codepoint":"U+200B""#));
        assert!(json.contains(r#""name":"ZERO WIDTH SPACE""#));
    }

    #[test]
    fn gremlin_summary_serialization() {
        let event = CheckEvent::GremlinSummary(GremlinSummaryEvent {
            status: "failed",
            found: 3,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"gremlin_summary""#));
        assert!(json.contains(r#""status":"failed""#));
        assert!(json.contains(r#""found":3"#));
    }

    #[test]
    fn parse_error_fallback_message() {
        // Test the fallback message construction when both streams are empty.
        let event = CheckEvent::Diagnostic(DiagnosticEvent {
            tool: "clippy",
            sweeps: Vec::new(),
            level: "error".into(),
            code: None,
            message: "cargo exited with non-zero status but produced no output".into(),
            file: None,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            primary_label: None,
            children: Vec::new(),
            rendered: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("produced no output"));
        assert!(json.contains(r#""type":"diagnostic""#));
    }
}
