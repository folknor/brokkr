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
    TestFailure(TestFailureEvent),
    DiagnosticSummary(DiagnosticSummaryEvent),
    TestSummary(TestSummaryEvent),
}

#[derive(Serialize)]
pub struct DiagnosticEvent {
    pub tool: &'static str,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered: Option<String>,
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
pub struct DiagnosticSummaryEvent {
    pub tool: &'static str,
    pub status: &'static str,
    pub errors: usize,
    pub warnings: usize,
}

#[derive(Serialize)]
pub struct TestSummaryEvent {
    pub status: &'static str,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub filtered_out: usize,
    pub suites: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
}

// --- Parsing ---

/// Parse cargo `--message-format=json` stdout into diagnostic events.
///
/// Each line is a JSON object from cargo. Only lines with
/// `"reason": "compiler-message"` are extracted; everything else is skipped.
#[allow(clippy::too_many_lines)] // JSON walk — splitting just shuffles match arms
pub fn parse_cargo_diagnostics(stdout: &str, tool: &'static str) -> Vec<CheckEvent> {
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

        // Skip "aborting due to N previous errors" meta-diagnostics.
        if level == "error" {
            let text = msg
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if text.contains("aborting due to") || text.contains("could not compile") {
                continue;
            }
        }

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

        // Primary span for file/line/column.
        let primary_span = msg
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|spans| spans.iter().find(|s| s.get("is_primary") == Some(&serde_json::Value::Bool(true))));

        let (file, line_start, col_start, line_end, col_end) = match primary_span {
            Some(span) => (
                span.get("file_name").and_then(|v| v.as_str()).map(std::string::ToString::to_string),
                span.get("line_start").and_then(serde_json::Value::as_u64),
                span.get("column_start").and_then(serde_json::Value::as_u64),
                span.get("line_end").and_then(serde_json::Value::as_u64),
                span.get("column_end").and_then(serde_json::Value::as_u64),
            ),
            None => (None, None, None, None, None),
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
            level,
            code,
            message,
            file,
            line: line_start,
            column: col_start,
            end_line: line_end,
            end_column: col_end,
            children,
            rendered,
        }));
    }

    events
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
        level: "error".into(),
        code: None,
        message,
        file: None,
        line: None,
        column: None,
        end_line: None,
        end_column: None,
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
    fn parse_single_error() {
        let input = sample_compiler_message("error", "E0425", "cannot find value", "src/main.rs", 10);
        let events = parse_cargo_diagnostics(&input, "clippy");
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
        let events = parse_cargo_diagnostics(input, "clippy");
        assert!(events.is_empty());
    }

    #[test]
    fn skips_aborting_errors() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":null,"message":"aborting due to 3 previous errors","spans":[],"children":[],"rendered":"error: aborting"}}"#;
        let events = parse_cargo_diagnostics(input, "clippy");
        assert!(events.is_empty());
    }

    #[test]
    fn multiple_diagnostics() {
        let mut input = sample_compiler_message("error", "E0308", "mismatched types", "src/a.rs", 1);
        input.push('\n');
        input.push_str(&sample_compiler_message("warning", "unused_variables", "unused var", "src/b.rs", 2));
        let events = parse_cargo_diagnostics(&input, "test");
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn emit_produces_valid_json() {
        let event = CheckEvent::DiagnosticSummary(DiagnosticSummaryEvent {
            tool: "clippy",
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
            passed: 5,
            failed: 0,
            ignored: 0,
            filtered_out: 0,
            suites: 1,
            duration_seconds: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("duration_seconds"));
    }

    #[test]
    fn parse_error_fallback_message() {
        // Test the fallback message construction when both streams are empty.
        let event = CheckEvent::Diagnostic(DiagnosticEvent {
            tool: "clippy",
            level: "error".into(),
            code: None,
            message: "cargo exited with non-zero status but produced no output".into(),
            file: None,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            children: Vec::new(),
            rendered: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("produced no output"));
        assert!(json.contains(r#""type":"diagnostic""#));
    }
}
