//! Parse and render the harness's NDJSON output.
//!
//! The harness emits one JSON object per line: a per-probe disposition
//! line for each probe, then a single summary line (`"summary": true`).
//! brokkr is deliberately tolerant of unknown fields here - the harness
//! owns the schema and will grow it (extra tiers, deltas, provenance)
//! ahead of brokkr learning to render them, the same forward-compat
//! posture as the cargo JSON parser.

use serde::Deserialize;

use crate::output;

/// Acceptance detail, present only when `outcome == "parity"`.
#[derive(Debug, Clone, Deserialize)]
pub struct Acceptance {
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub failing: Vec<String>,
}

/// A per-probe disposition line.
#[derive(Debug, Clone, Deserialize)]
pub struct ProbeLine {
    pub probe: String,
    pub outcome: String,
    #[serde(default)]
    pub matched: u64,
    #[serde(default)]
    pub ours_only: u64,
    #[serde(default)]
    pub tv_only: u64,
    #[serde(default)]
    pub count_tier: Option<String>,
    #[serde(default)]
    pub acceptance: Option<Acceptance>,
    /// Error string carried by a `*_fail` outcome.
    #[serde(default)]
    pub error: Option<String>,
}

/// The trailing summary line.
#[derive(Debug, Clone, Deserialize)]
pub struct SummaryLine {
    #[allow(dead_code)] // discriminator only; presence is what matters
    pub summary: bool,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub parity: u64,
    #[serde(default)]
    pub byte_exact: u64,
    #[serde(default)]
    pub accepted: u64,
    #[serde(default)]
    pub actionable_drift: u64,
    #[serde(default)]
    pub count_divergent: u64,
    #[serde(default)]
    pub compile_fail: u64,
    #[serde(default)]
    pub runtime_fail: u64,
}

/// Everything parsed out of one harness run.
#[derive(Debug, Default)]
pub struct HarnessReport {
    pub probes: Vec<ProbeLine>,
    pub summary: Option<SummaryLine>,
}

/// Parse NDJSON harness stdout. Blank lines are skipped; a line that
/// fails to parse as either shape is surfaced as a warning but does not
/// abort - the run's exit status and the summary line are the source of
/// truth, and a forward-compat field we cannot model should not sink the
/// report.
pub fn parse(stdout: &[u8]) -> HarnessReport {
    let text = String::from_utf8_lossy(stdout);
    let mut report = HarnessReport::default();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Branch on the presence of the `summary` discriminator so the
        // two shapes never fight over a shared field.
        let is_summary = serde_json::from_str::<serde_json::Value>(trimmed)
            .ok()
            .and_then(|v| v.get("summary").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);

        if is_summary {
            match serde_json::from_str::<SummaryLine>(trimmed) {
                Ok(s) => report.summary = Some(s),
                Err(e) => output::corpus_msg(&format!("warning: unparsable summary line: {e}")),
            }
        } else {
            match serde_json::from_str::<ProbeLine>(trimmed) {
                Ok(p) => report.probes.push(p),
                Err(e) => output::corpus_msg(&format!("warning: unparsable probe line: {e}")),
            }
        }
    }

    report
}

/// Render the per-probe lines and the summary to the `[corpus]` log.
pub fn render(report: &HarnessReport) {
    for p in &report.probes {
        output::corpus_msg(&format_probe(p));
    }
    if let Some(s) = &report.summary {
        output::corpus_msg(&format_summary(s));
    }
}

fn format_probe(p: &ProbeLine) -> String {
    if let Some(err) = &p.error {
        return format!("{}: {} - {err}", p.probe, p.outcome);
    }
    let mut line = format!(
        "{}: {}",
        p.probe, p.outcome
    );
    if let Some(a) = &p.acceptance {
        line.push_str(&format!(" [{}", a.tier));
        if !a.profile.is_empty() {
            line.push_str(&format!("/{}", a.profile));
        }
        line.push(']');
        if !a.failing.is_empty() {
            line.push_str(&format!(" failing={}", a.failing.join(",")));
        }
    } else if let Some(ct) = &p.count_tier {
        line.push_str(&format!(" [{ct}]"));
    }
    line.push_str(&format!(
        " matched={} ours_only={} tv_only={}",
        p.matched, p.ours_only, p.tv_only
    ));
    line
}

fn format_summary(s: &SummaryLine) -> String {
    format!(
        "summary: {} total, {} parity (byte_exact={}, accepted={}, actionable_drift={}, count_divergent={}), compile_fail={}, runtime_fail={}",
        s.total,
        s.parity,
        s.byte_exact,
        s.accepted,
        s.actionable_drift,
        s.count_divergent,
        s.compile_fail,
        s.runtime_fail,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn parses_probe_and_summary_lines() {
        let nd = br#"{"probe":"p1","outcome":"parity","matched":42,"ours_only":0,"tv_only":0,"count_tier":"exact","acceptance":{"tier":"accepted","profile":"production","p90":{"exit":0.18},"failing":["exit_price"]}}
{"probe":"p2","outcome":"compile_fail","error":"unexpected token"}
{"summary":true,"total":2,"parity":1,"byte_exact":0,"accepted":1,"actionable_drift":0,"count_divergent":0,"compile_fail":1,"runtime_fail":0}
"#;
        let r = parse(nd);
        assert_eq!(r.probes.len(), 2);
        assert_eq!(r.probes[0].matched, 42);
        assert_eq!(
            r.probes[0].acceptance.as_ref().unwrap().failing,
            vec!["exit_price".to_owned()]
        );
        assert_eq!(r.probes[1].error.as_deref(), Some("unexpected token"));
        let s = r.summary.unwrap();
        assert_eq!(s.total, 2);
        assert_eq!(s.compile_fail, 1);
    }

    #[test]
    fn tolerates_unknown_fields_and_blank_lines() {
        let nd = br#"
{"probe":"p1","outcome":"parity","matched":1,"future_field":{"x":1},"tv_only":0}

{"summary":true,"total":1,"parity":1,"brand_new_count":99}
"#;
        let r = parse(nd);
        assert_eq!(r.probes.len(), 1);
        assert_eq!(r.probes[0].probe, "p1");
        assert!(r.summary.is_some());
    }
}
