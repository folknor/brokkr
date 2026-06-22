//! Parse the two validators' JSON into a normalized [`DiagSet`].
//!
//! Both schemas are parsed tolerantly (unknown fields ignored) and reduced
//! to `(line, col, severity)` keys under a [`Scope`] (which severities and
//! which stages count). piners `hint` diagnostics are always dropped
//! (pine-lint has no counterpart). A parse failure returns `Err(reason)` -
//! the caller turns that into a `piners_error` / `lint_error` disposition
//! rather than aborting the whole run.
//!
//! **Stage scope.** The prototype (`pine-tools/scripts/compare-piners.mjs`)
//! found that comparing *type/semantic* diagnostics across the two tools is
//! mostly noise, so the default is syntax/parser-only. piners tags every
//! diagnostic with a `stage` (`lex`/`parse`/`type`/`semantic`); pine-lint is
//! expected to grow a `stage` field too, and until it does we fall back to a
//! message heuristic ([`is_syntax_message`]) ported from the prototype.

use serde::Deserialize;

use super::{DiagKey, DiagSet, Severity};

/// Which diagnostics count toward the diff.
#[derive(Debug, Clone, Copy)]
pub struct Scope {
    /// Include `warning`-severity diagnostics (else errors only).
    pub include_warnings: bool,
    /// Restrict to parser/syntax-stage diagnostics (else all stages).
    pub syntax_only: bool,
}

impl Scope {
    /// True if a diagnostic of `severity` passes the severity filter.
    fn keeps(self, severity: Severity) -> bool {
        self.include_warnings || severity == Severity::Error
    }
}

// ---------- piners (`<bin> validate <file> --format json`) ----------

#[derive(Debug, Deserialize)]
struct PinersReport {
    #[serde(default)]
    diagnostics: Vec<PinersDiag>,
}

#[derive(Debug, Deserialize)]
struct PinersDiag {
    severity: String,
    #[serde(default)]
    stage: Option<String>,
    line: usize,
    #[serde(default)]
    column: Option<usize>,
}

/// Parse piners' `Report` JSON under `scope`. Hint-severity diagnostics are
/// dropped; unknown severities are ignored (forward-compat). Exit code is not
/// the signal - this only looks at stdout - so a non-zero exit with valid
/// JSON still parses cleanly.
pub fn parse_piners(stdout: &[u8], scope: Scope) -> Result<DiagSet, String> {
    let report: PinersReport = serde_json::from_slice(stdout)
        .map_err(|e| format!("piners validate emitted unparsable JSON: {e}"))?;
    let mut set = DiagSet::new();
    for d in &report.diagnostics {
        let Some(sev) = severity_from_piners(&d.severity) else {
            continue;
        };
        if !scope.keeps(sev) {
            continue;
        }
        if scope.syntax_only && !is_piners_syntax_stage(d.stage.as_deref()) {
            continue;
        }
        set.insert(DiagKey {
            line: d.line,
            col: d.column,
            severity: sev,
        });
    }
    Ok(set)
}

fn severity_from_piners(s: &str) -> Option<Severity> {
    match s {
        "error" => Some(Severity::Error),
        "warning" => Some(Severity::Warning),
        // "hint" and any future variant are informational, not gated.
        _ => None,
    }
}

/// piners' parser/lexer stages are the syntax tier; `type`/`semantic` are not.
/// A missing stage is treated as syntax (fail-open: better a false agreement
/// than dropping a real syntax diagnostic).
fn is_piners_syntax_stage(stage: Option<&str>) -> bool {
    matches!(stage, None | Some("lex") | Some("parse"))
}

// ---------- pine-lint (offline and `--tv`, same schema) ----------

#[derive(Debug, Deserialize)]
struct PineLintResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Option<PineLintResult>,
}

#[derive(Debug, Default, Deserialize)]
struct PineLintResult {
    // `errors`/`warnings` are *absent* (not `[]`) on a clean snippet;
    // `default` folds absent -> empty. `null_as_empty` also tolerates an
    // explicit `null` from a future schema tweak.
    #[serde(default, deserialize_with = "null_as_empty")]
    errors: Vec<PineLintDiag>,
    #[serde(default, deserialize_with = "null_as_empty")]
    warnings: Vec<PineLintDiag>,
}

#[derive(Debug, Deserialize)]
struct PineLintDiag {
    start: PineLintPos,
    /// Stage, once pine-lint emits it (`syntax`/`type`/`analysis`). Until
    /// then, [`is_syntax_message`] classifies from the message text.
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct PineLintPos {
    line: usize,
    #[serde(default)]
    column: Option<usize>,
}

fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

/// Parse pine-lint's JSON (the same shape offline and under `--tv`) under
/// `scope`. `result.errors[]` -> `error`, `result.warnings[]` -> `warning`. A
/// `success: false` response (transport failure, mostly a `--tv` concern) or a
/// missing `result` is an `Err` - we cannot claim the snippet is clean.
pub fn parse_pine_lint(stdout: &[u8], scope: Scope) -> Result<DiagSet, String> {
    let resp: PineLintResponse = serde_json::from_slice(stdout)
        .map_err(|e| format!("pine-lint emitted unparsable JSON: {e}"))?;
    if !resp.success {
        return Err(format!(
            "pine-lint reported failure: {}",
            resp.error.as_deref().unwrap_or("no error message")
        ));
    }
    let result = resp
        .result
        .ok_or_else(|| "pine-lint returned success=true but no result field".to_owned())?;
    let mut set = DiagSet::new();
    for (diags, severity) in [
        (&result.errors, Severity::Error),
        (&result.warnings, Severity::Warning),
    ] {
        if !scope.keeps(severity) {
            continue;
        }
        for d in diags {
            if scope.syntax_only && !is_pine_lint_syntax(d) {
                continue;
            }
            set.insert(DiagKey {
                line: d.start.line,
                col: d.start.column,
                severity,
            });
        }
    }
    Ok(set)
}

/// Is this pine-lint diagnostic a syntax-stage one? Prefer the explicit
/// `stage` field; fall back to the message heuristic until pine-lint emits it.
fn is_pine_lint_syntax(d: &PineLintDiag) -> bool {
    match d.stage.as_deref() {
        Some(stage) => stage.eq_ignore_ascii_case("syntax"),
        None => is_syntax_message(&d.message),
    }
}

/// Heuristic ported from `compare-piners.mjs` `isSyntaxMessage`: a parser-tier
/// message reads like one of these. Used only when pine-lint omits `stage`.
fn is_syntax_message(message: &str) -> bool {
    const NEEDLES: [&str; 7] = [
        "syntax",
        "unexpected token",
        "mismatched input",
        "missing closing",
        "no viable alternative",
        "extraneous input",
        "end of line without line continuation",
    ];
    let lower = message.to_ascii_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const ALL: Scope = Scope {
        include_warnings: true,
        syntax_only: false,
    };
    const ERR_SYNTAX: Scope = Scope {
        include_warnings: false,
        syntax_only: true,
    };

    fn key(line: usize, col: usize, sev: Severity) -> DiagKey {
        DiagKey {
            line,
            col: Some(col),
            severity: sev,
        }
    }

    #[test]
    fn piners_clean_report_is_empty() {
        assert!(parse_piners(br#"{"ok":true,"diagnostics":[]}"#, ALL).unwrap().is_empty());
    }

    #[test]
    fn piners_maps_error_and_warning_and_drops_hint() {
        let json = br#"{"ok":false,"diagnostics":[
            {"severity":"error","stage":"type","code":"PINE1","line":3,"column":5,"message":"x"},
            {"severity":"warning","stage":"semantic","line":7,"column":1,"message":"y"},
            {"severity":"hint","stage":"parse","line":9,"column":2,"message":"z"}
        ]}"#;
        let set = parse_piners(json, ALL).unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&key(3, 5, Severity::Error)));
        assert!(set.contains(&key(7, 1, Severity::Warning)));
    }

    #[test]
    fn piners_syntax_scope_drops_type_and_warnings() {
        let json = br#"{"ok":false,"diagnostics":[
            {"severity":"error","stage":"parse","line":3,"column":5,"message":"x"},
            {"severity":"error","stage":"type","line":4,"column":1,"message":"y"},
            {"severity":"warning","stage":"lex","line":5,"column":1,"message":"z"}
        ]}"#;
        let set = parse_piners(json, ERR_SYNTAX).unwrap();
        // Only the parse-stage error survives: type is wrong stage, the warning
        // is wrong severity.
        assert_eq!(set.len(), 1);
        assert!(set.contains(&key(3, 5, Severity::Error)));
    }

    #[test]
    fn piners_unparsable_is_err() {
        assert!(parse_piners(b"not json", ALL).is_err());
        assert!(parse_piners(b"", ALL).is_err());
    }

    #[test]
    fn pine_lint_clean_absent_arrays_is_empty() {
        let json = br#"{"success":true,"result":{"variables":[],"functions":[]}}"#;
        assert!(parse_pine_lint(json, ALL).unwrap().is_empty());
    }

    #[test]
    fn pine_lint_maps_errors_and_warnings() {
        let json = br#"{"success":true,"result":{
            "errors":[{"start":{"line":3,"column":18},"message":"e"}],
            "warnings":[{"start":{"line":2,"column":8},"message":"w"}]
        }}"#;
        let set = parse_pine_lint(json, ALL).unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&key(3, 18, Severity::Error)));
        assert!(set.contains(&key(2, 8, Severity::Warning)));
    }

    #[test]
    fn pine_lint_syntax_scope_uses_stage_field_then_message() {
        // One has an explicit stage; one relies on the message heuristic; one
        // is type-stage prose that must be dropped under syntax_only.
        let json = br#"{"success":true,"result":{"errors":[
            {"start":{"line":1,"column":1},"stage":"syntax","message":"whatever"},
            {"start":{"line":2,"column":1},"message":"unexpected token foo"},
            {"start":{"line":3,"column":1},"message":"type mismatch: expected int"}
        ]}}"#;
        let set = parse_pine_lint(json, ERR_SYNTAX).unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&key(1, 1, Severity::Error)));
        assert!(set.contains(&key(2, 1, Severity::Error)));
    }

    #[test]
    fn pine_lint_null_arrays_tolerated() {
        let json = br#"{"success":true,"result":{"errors":null,"warnings":null}}"#;
        assert!(parse_pine_lint(json, ALL).unwrap().is_empty());
    }

    #[test]
    fn pine_lint_failure_is_err() {
        assert!(parse_pine_lint(br#"{"success":false,"error":"rate limited"}"#, ALL).is_err());
        assert!(parse_pine_lint(br#"{"success":true}"#, ALL).is_err());
        assert!(parse_pine_lint(b"<html>503</html>", ALL).is_err());
    }
}
