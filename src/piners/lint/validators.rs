//! Parse the two validators' JSON into a normalized [`DiagSet`].
//!
//! Both schemas are parsed tolerantly (unknown fields ignored) and reduced
//! to `(line, col, severity)` keys, error+warning only. piners `hint`
//! diagnostics are dropped (pine-lint has no counterpart). A parse failure
//! returns `Err(reason)` - the caller turns that into a `piners_error` /
//! `lint_error` disposition rather than aborting the whole run.

use serde::Deserialize;

use super::{DiagKey, DiagSet, Severity};

// ---------- piners (`<bin> validate <file> --format json`) ----------

#[derive(Debug, Deserialize)]
struct PinersReport {
    #[serde(default)]
    diagnostics: Vec<PinersDiag>,
}

#[derive(Debug, Deserialize)]
struct PinersDiag {
    severity: String,
    line: usize,
    #[serde(default)]
    column: Option<usize>,
}

/// Parse piners' `Report` JSON. Hint-severity diagnostics are dropped;
/// unknown severities are ignored (forward-compat). Exit code is not the
/// signal - this only looks at stdout - so a non-zero exit with valid JSON
/// still parses cleanly.
pub fn parse_piners(stdout: &[u8]) -> Result<DiagSet, String> {
    let report: PinersReport = serde_json::from_slice(stdout)
        .map_err(|e| format!("piners validate emitted unparsable JSON: {e}"))?;
    let mut set = DiagSet::new();
    for d in &report.diagnostics {
        if let Some(sev) = severity_from_piners(&d.severity) {
            set.insert(DiagKey {
                line: d.line,
                col: d.column,
                severity: sev,
            });
        }
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

/// Parse pine-lint's JSON (the same shape offline and under `--tv`).
/// `result.errors[]` -> `error`, `result.warnings[]` -> `warning`. A
/// `success: false` response (transport failure, mostly a `--tv` concern)
/// or a missing `result` is an `Err` - we cannot claim the snippet is clean.
pub fn parse_pine_lint(stdout: &[u8]) -> Result<DiagSet, String> {
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
    for d in &result.errors {
        set.insert(DiagKey {
            line: d.start.line,
            col: d.start.column,
            severity: Severity::Error,
        });
    }
    for d in &result.warnings {
        set.insert(DiagKey {
            line: d.start.line,
            col: d.start.column,
            severity: Severity::Warning,
        });
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn key(line: usize, col: usize, sev: Severity) -> DiagKey {
        DiagKey {
            line,
            col: Some(col),
            severity: sev,
        }
    }

    #[test]
    fn piners_clean_report_is_empty() {
        let set = parse_piners(br#"{"ok":true,"diagnostics":[]}"#).unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn piners_maps_error_and_warning_and_drops_hint() {
        let json = br#"{"ok":false,"diagnostics":[
            {"severity":"error","stage":"type","code":"PINE1","line":3,"column":5,"message":"x"},
            {"severity":"warning","line":7,"column":1,"message":"y"},
            {"severity":"hint","line":9,"column":2,"message":"z"}
        ]}"#;
        let set = parse_piners(json).unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&key(3, 5, Severity::Error)));
        assert!(set.contains(&key(7, 1, Severity::Warning)));
    }

    #[test]
    fn piners_unparsable_is_err() {
        assert!(parse_piners(b"not json").is_err());
        assert!(parse_piners(b"").is_err());
    }

    #[test]
    fn pine_lint_clean_absent_arrays_is_empty() {
        let json = br#"{"success":true,"result":{"variables":[],"functions":[]}}"#;
        let set = parse_pine_lint(json).unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn pine_lint_maps_errors_and_warnings() {
        let json = br#"{"success":true,"result":{
            "errors":[{"start":{"line":3,"column":18},"end":{"line":3,"column":19},"message":"e"}],
            "warnings":[{"start":{"line":2,"column":8},"message":"w"}]
        }}"#;
        let set = parse_pine_lint(json).unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains(&key(3, 18, Severity::Error)));
        assert!(set.contains(&key(2, 8, Severity::Warning)));
    }

    #[test]
    fn pine_lint_null_arrays_tolerated() {
        let json = br#"{"success":true,"result":{"errors":null,"warnings":null}}"#;
        assert!(parse_pine_lint(json).unwrap().is_empty());
    }

    #[test]
    fn pine_lint_failure_is_err() {
        assert!(parse_pine_lint(br#"{"success":false,"error":"rate limited"}"#).is_err());
        assert!(parse_pine_lint(br#"{"success":true}"#).is_err());
        assert!(parse_pine_lint(b"<html>503</html>").is_err());
    }
}
