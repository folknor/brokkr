//! Parser for cargo's `--message-format=json` output.
//!
//! Turns cargo/clippy/rustc JSON diagnostics into structured [`DiagnosticEvent`]
//! values that the `check` text renderer formats. This is how brokkr *reads*
//! cargo - there is no machine-readable output mode of brokkr's own.

// --- Diagnostic model ---

/// A parsed compiler/clippy diagnostic. Field set mirrors the parts of cargo's
/// JSON the text renderer consumes.
pub struct DiagnosticEvent {
    pub level: String,
    pub code: Option<String>,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
    /// Inline label on the primary span, e.g. "expected `i32`, found `&str`".
    pub primary_label: Option<String>,
    pub children: Vec<ChildDiagnostic>,
    pub rendered: Option<String>,
}

pub struct ChildDiagnostic {
    pub message: String,
}

// --- Parsing ---

/// Parse cargo `--message-format=json` stdout into diagnostics.
///
/// Each line is a JSON object from cargo. Only lines with
/// `"reason": "compiler-message"` are extracted; everything else is skipped.
///
#[allow(clippy::too_many_lines)] // JSON walk - splitting just shuffles match arms
pub fn parse_cargo_diagnostics(stdout: &str) -> Vec<DiagnosticEvent> {
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

        let (file, line_start, col_start, primary_label) = match primary_span {
            Some(span) => (
                span.get("file_name").and_then(|v| v.as_str()).map(std::string::ToString::to_string),
                span.get("line_start").and_then(serde_json::Value::as_u64),
                span.get("column_start").and_then(serde_json::Value::as_u64),
                span.get("label")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(std::string::ToString::to_string),
            ),
            None => (None, None, None, None),
        };

        // Child diagnostics - only the message is consumed (the text renderer
        // scrapes the "expected/found" detail line from it).
        let children = msg
            .get("children")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|child| {
                        let child_msg = child.get("message")?.as_str()?.to_string();
                        if child_msg.is_empty() {
                            return None;
                        }
                        Some(ChildDiagnostic { message: child_msg })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let rendered = msg
            .get("rendered")
            .and_then(|v| v.as_str())
            .map(|s| s.trim_end().to_string());

        events.push(DiagnosticEvent {
            level,
            code,
            message,
            file,
            line: line_start,
            column: col_start,
            primary_label,
            children,
            rendered,
        });
    }

    events
}

/// Classify a compiler-message as a rustc/cargo summary or meta-noise line
/// that should not become a real diagnostic.
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
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
        let events = parse_cargo_diagnostics(input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].primary_label.as_deref(), Some("expected `i32`, found `&str`"));
    }

    #[test]
    fn parse_omits_empty_primary_label() {
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":{"code":"unused_variables"},"message":"unused","spans":[{"file_name":"src/a.rs","line_start":1,"column_start":1,"line_end":1,"column_end":2,"is_primary":true,"label":""}],"children":[],"rendered":"rendered"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events[0].primary_label.is_none());
    }

    #[test]
    fn parse_single_error() {
        let input = sample_compiler_message("error", "E0425", "cannot find value", "src/main.rs", 10);
        let events = parse_cargo_diagnostics(&input);
        assert_eq!(events.len(), 1);
        let d = &events[0];
        assert_eq!(d.level, "error");
        assert_eq!(d.code.as_deref(), Some("E0425"));
        assert_eq!(d.message, "cannot find value");
        assert_eq!(d.file.as_deref(), Some("src/main.rs"));
        assert_eq!(d.line, Some(10));
        assert_eq!(d.column, Some(5));
        assert_eq!(d.children.len(), 1);
        assert_eq!(d.children[0].message, "try this");
    }

    #[test]
    fn skips_non_compiler_messages() {
        let input = r#"{"reason":"compiler-artifact","target":{"name":"foo"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events.is_empty());
    }

    #[test]
    fn skips_aborting_errors() {
        let input = r#"{"reason":"compiler-message","message":{"level":"error","code":null,"message":"aborting due to 3 previous errors","spans":[],"children":[],"rendered":"error: aborting"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events.is_empty());
    }

    #[test]
    fn real_clippy_warning_produces_one_diagnostic() {
        // A genuine lint: has both spans and a code -> must survive.
        let input = sample_compiler_message("warning", "clippy::needless_return", "unneeded return statement", "src/a.rs", 7);
        let events = parse_cargo_diagnostics(&input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].code.as_deref(), Some("clippy::needless_return"));
        assert_eq!(events[0].file.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn skips_warnings_emitted_summary() {
        // rustc's "N warnings emitted": empty spans, null code -> zero diagnostics.
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"2 warnings emitted","spans":[],"children":[],"rendered":"warning: 2 warnings emitted"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events.is_empty(), "expected the summary line to be filtered, got {} event(s)", events.len());
    }

    #[test]
    fn skips_generated_warnings_summary() {
        // cargo's per-crate roll-up: "`crate` (lib) generated N warnings".
        let input = r#"{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"`brokkr` (lib) generated 3 warnings","spans":[],"children":[],"rendered":"warning: `brokkr` (lib) generated 3 warnings"}}"#;
        let events = parse_cargo_diagnostics(input);
        assert!(events.is_empty(), "expected the generated-warnings roll-up to be filtered, got {} event(s)", events.len());
    }

    #[test]
    fn multiple_diagnostics() {
        let mut input = sample_compiler_message("error", "E0308", "mismatched types", "src/a.rs", 1);
        input.push('\n');
        input.push_str(&sample_compiler_message("warning", "unused_variables", "unused var", "src/b.rs", 2));
        let events = parse_cargo_diagnostics(&input);
        assert_eq!(events.len(), 2);
    }
}
