//! Sync-bench regression gate evaluation.
//!
//! See `docs/commands/ratatoskr-gate.md` for the design. This module is
//! pure: it pulls scalar values out of two JSON-encoded `gate_runs` rows
//! (current + baseline) according to the configured `MetricRule` set
//! and reports per-rule pass/fail. Storage and CLI plumbing live in
//! `src/db/gate.rs` and `src/ratatoskr/sync.rs`.

use std::fmt::Write;

use serde_json::{Map, Value};

use crate::config::{GateConfig, MetricRule};
use crate::db::gate::GateEntry;
use crate::error::DevError;

/// Outcome of evaluating one rule against one metric.
#[derive(Debug)]
pub struct RuleOutcome {
    pub metric: String,
    pub rule: String,
    pub pass: bool,
    pub detail: String,
}

/// Evaluate every rule under the given gate config against the current
/// run + baseline. Returns one `RuleOutcome` per (metric, rule) pair.
/// Metric lookup failures (missing key in JSON, bad scalar shape) are
/// hard errors per the design - no silent zero-treatment.
pub fn evaluate(
    gate: &GateConfig,
    current: &GateRun,
    baseline: &GateRun,
) -> Result<Vec<RuleOutcome>, DevError> {
    let mut out = Vec::new();
    for (metric, rule) in &gate.metrics {
        let current_v = lookup_scalar(current, metric)?;
        let baseline_v = lookup_scalar(baseline, metric)?;
        out.extend(evaluate_rule(metric, rule, &current_v, &baseline_v)?);
    }
    Ok(out)
}

/// Format a list of `RuleOutcome`s as one line each, prefixed with
/// `OK` or `FAIL`. Returns `(report, any_failed)`.
pub fn format_report(outcomes: &[RuleOutcome]) -> (String, bool) {
    let mut s = String::new();
    let mut any_failed = false;
    for o in outcomes {
        let tag = if o.pass { "OK  " } else { "FAIL" };
        if !o.pass {
            any_failed = true;
        }
        writeln!(s, "  [{tag}] {} :: {} ({})", o.metric, o.rule, o.detail)
            .expect("writing to a String never fails");
    }
    if s.ends_with('\n') {
        s.pop();
    }
    (s, any_failed)
}

// ---------------------------------------------------------------------------
// Scalar projection of a gate row
// ---------------------------------------------------------------------------

/// View of a gate row as scalar metrics (bare top-level fields plus
/// `sidecar.*` and `meta.*` JSON blobs). Constructed once per evaluation
/// from a `GateEntry` so the JSON is parsed once.
pub struct GateRun {
    elapsed_ms: i64,
    exit_code: i32,
    success: bool,
    sidecar: Map<String, Value>,
    meta: Map<String, Value>,
}

impl GateRun {
    pub fn from_entry(entry: &GateEntry) -> Result<Self, DevError> {
        Ok(Self {
            elapsed_ms: entry.elapsed_ms,
            exit_code: entry.exit_code,
            success: entry.success,
            sidecar: parse_obj(&entry.sidecar, "sidecar")?,
            meta: parse_obj(&entry.meta, "meta")?,
        })
    }

    /// Build a `GateRun` directly from in-memory data, skipping the
    /// JSON serialize+parse round-trip. Used by sync-bench for the
    /// just-completed run.
    pub fn from_parts(
        elapsed_ms: i64,
        exit_code: i32,
        success: bool,
        sidecar: Map<String, Value>,
        meta: Map<String, Value>,
    ) -> Self {
        Self {
            elapsed_ms,
            exit_code,
            success,
            sidecar,
            meta,
        }
    }
}

fn parse_obj(blob: &str, label: &str) -> Result<Map<String, Value>, DevError> {
    let v: Value = serde_json::from_str(blob)
        .map_err(|e| DevError::Config(format!("gate: parse {label} blob: {e}")))?;
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(DevError::Config(format!(
            "gate: {label} blob must be a JSON object"
        ))),
    }
}

/// One scalar metric value; either a number or a string. Bools collapse
/// to 0/1; nulls/arrays/objects are rejected at lookup time.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
    Num(f64),
    Text(String),
}

impl Scalar {
    fn as_num(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            Self::Text(_) => None,
        }
    }
    fn fmt(&self) -> String {
        match self {
            Self::Num(n) => format_num(*n),
            Self::Text(s) => format!("\"{s}\""),
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn lookup_scalar(run: &GateRun, key: &str) -> Result<Scalar, DevError> {
    if let Some(rest) = key.strip_prefix("sidecar.") {
        return lookup_in_blob(&run.sidecar, rest, "sidecar");
    }
    if let Some(rest) = key.strip_prefix("meta.") {
        return lookup_in_blob(&run.meta, rest, "meta");
    }
    match key {
        "elapsed_ms" => Ok(Scalar::Num(run.elapsed_ms as f64)),
        "exit_code" => Ok(Scalar::Num(f64::from(run.exit_code))),
        "success" => Ok(Scalar::Num(if run.success { 1.0 } else { 0.0 })),
        other => Err(DevError::Config(format!(
            "gate: unknown bare metric `{other}` (v1 set: elapsed_ms, exit_code, success). \
             For other metrics use `sidecar.<key>` or `meta.<key>`."
        ))),
    }
}

fn lookup_in_blob(map: &Map<String, Value>, key: &str, ns: &str) -> Result<Scalar, DevError> {
    let v = map.get(key).ok_or_else(|| {
        DevError::Config(format!(
            "gate: missing `{ns}.{key}` in run data - cannot evaluate rule"
        ))
    })?;
    json_to_scalar(v).ok_or_else(|| {
        DevError::Config(format!(
            "gate: `{ns}.{key}` is not a scalar (got {})",
            kind_of(v)
        ))
    })
}

fn json_to_scalar(v: &Value) -> Option<Scalar> {
    match v {
        Value::Number(n) => n.as_f64().map(Scalar::Num),
        Value::String(s) => Some(Scalar::Text(s.clone())),
        Value::Bool(b) => Some(Scalar::Num(if *b { 1.0 } else { 0.0 })),
        _ => None,
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn toml_to_scalar(v: &toml::Value) -> Option<Scalar> {
    match v {
        toml::Value::Integer(i) => Some(Scalar::Num(*i as f64)),
        toml::Value::Float(f) => Some(Scalar::Num(*f)),
        toml::Value::String(s) => Some(Scalar::Text(s.clone())),
        toml::Value::Boolean(b) => Some(Scalar::Num(if *b { 1.0 } else { 0.0 })),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rule application
// ---------------------------------------------------------------------------

fn evaluate_rule(
    metric: &str,
    rule: &MetricRule,
    current: &Scalar,
    baseline: &Scalar,
) -> Result<Vec<RuleOutcome>, DevError> {
    let mut out = Vec::new();

    if let Some(cap) = rule.max {
        out.push(check_num(metric, "max", current, |c| {
            (c <= cap, format!("current={} max={}", format_num(c), format_num(cap)))
        })?);
    }
    if let Some(floor) = rule.min {
        out.push(check_num(metric, "min", current, |c| {
            (c >= floor, format!("current={} min={}", format_num(c), format_num(floor)))
        })?);
    }
    if let Some(factor) = rule.max_relative {
        let b = require_num(metric, "max_relative", baseline, "baseline")?;
        let bound = b * factor;
        out.push(check_num(metric, "max_relative", current, |c| {
            (
                c <= bound,
                format!("current={} bound={} (baseline={} * {})", format_num(c), format_num(bound), format_num(b), format_num(factor)),
            )
        })?);
    }
    if let Some(factor) = rule.min_relative {
        let b = require_num(metric, "min_relative", baseline, "baseline")?;
        let bound = b * factor;
        out.push(check_num(metric, "min_relative", current, |c| {
            (
                c >= bound,
                format!("current={} bound={} (baseline={} * {})", format_num(c), format_num(bound), format_num(b), format_num(factor)),
            )
        })?);
    }
    if let Some(delta) = rule.max_delta {
        let b = require_num(metric, "max_delta", baseline, "baseline")?;
        out.push(check_num(metric, "max_delta", current, |c| {
            let d = c - b;
            (
                d <= delta,
                format!("current={} baseline={} delta={} max_delta={}", format_num(c), format_num(b), format_num(d), format_num(delta)),
            )
        })?);
    }
    if let Some(ref expected) = rule.equal {
        let want = toml_to_scalar(expected).ok_or_else(|| {
            DevError::Config(format!(
                "gate: metric `{metric}` rule `equal`: value must be int/float/string/bool"
            ))
        })?;
        let pass = current == &want;
        out.push(RuleOutcome {
            metric: metric.into(),
            rule: "equal".into(),
            pass,
            detail: format!("current={} expected={}", current.fmt(), want.fmt()),
        });
    }
    if rule.equal_to_baseline == Some(true) {
        let pass = current == baseline;
        out.push(RuleOutcome {
            metric: metric.into(),
            rule: "equal_to_baseline".into(),
            pass,
            detail: format!("current={} baseline={}", current.fmt(), baseline.fmt()),
        });
    }

    Ok(out)
}

fn check_num<F: FnOnce(f64) -> (bool, String)>(
    metric: &str,
    rule: &str,
    current: &Scalar,
    f: F,
) -> Result<RuleOutcome, DevError> {
    let c = require_num(metric, rule, current, "current")?;
    let (pass, detail) = f(c);
    Ok(RuleOutcome {
        metric: metric.into(),
        rule: rule.into(),
        pass,
        detail,
    })
}

fn require_num(metric: &str, rule: &str, s: &Scalar, side: &str) -> Result<f64, DevError> {
    s.as_num().ok_or_else(|| {
        DevError::Config(format!(
            "gate: metric `{metric}` rule `{rule}`: {side} value is not numeric"
        ))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
    use super::*;
    use std::collections::BTreeMap;

    fn run(elapsed: i64, sidecar_json: &str, meta_json: &str) -> GateRun {
        GateRun {
            elapsed_ms: elapsed,
            exit_code: 0,
            success: true,
            sidecar: serde_json::from_str(sidecar_json).unwrap(),
            meta: serde_json::from_str(meta_json).unwrap(),
        }
    }

    fn gate_with(metric: &str, rule: MetricRule) -> GateConfig {
        let mut metrics = BTreeMap::new();
        metrics.insert(metric.into(), rule);
        GateConfig {
            script: "x.lua".into(),
            baseline_label: None,
            baseline: BTreeMap::new(),
            metrics,
        }
    }

    #[test]
    fn max_pass_and_fail() {
        let cur = run(800, "{}", "{}");
        let base = run(700, "{}", "{}");
        let g = gate_with(
            "elapsed_ms",
            MetricRule { max: Some(1000.0), ..Default::default() },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].pass);

        let g = gate_with(
            "elapsed_ms",
            MetricRule { max: Some(500.0), ..Default::default() },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert!(!r[0].pass);
    }

    #[test]
    fn max_relative_against_baseline() {
        let cur = run(770, "{}", "{}");
        let base = run(700, "{}", "{}");
        let g = gate_with(
            "elapsed_ms",
            MetricRule { max_relative: Some(1.10), ..Default::default() },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert!(r[0].pass, "770 <= 700*1.10=770: pass on the boundary");

        let cur = run(771, "{}", "{}");
        let r = evaluate(&g, &cur, &base).unwrap();
        assert!(!r[0].pass);
    }

    #[test]
    fn meta_namespace_lookup() {
        let cur = run(0, "{}", r#"{"correct":1,"messages":1000}"#);
        let base = run(0, "{}", r#"{"correct":1,"messages":1000}"#);
        let g = gate_with(
            "meta.correct",
            MetricRule { equal: Some(toml::Value::Integer(1)), ..Default::default() },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert!(r[0].pass);
    }

    #[test]
    fn equal_to_baseline_strict() {
        let cur = run(0, "{}", r#"{"messages":999}"#);
        let base = run(0, "{}", r#"{"messages":1000}"#);
        let g = gate_with(
            "meta.messages",
            MetricRule { equal_to_baseline: Some(true), ..Default::default() },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert!(!r[0].pass);
    }

    #[test]
    fn missing_key_is_hard_error() {
        let cur = run(0, "{}", "{}");
        let base = run(0, "{}", "{}");
        let g = gate_with(
            "meta.never_set",
            MetricRule { max: Some(1.0), ..Default::default() },
        );
        let err = evaluate(&g, &cur, &base).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("missing"), "got: {msg}");
    }

    #[test]
    fn max_delta_zero_catches_drift() {
        let cur = run(0, "{}", r#"{"requests":13}"#);
        let base = run(0, "{}", r#"{"requests":12}"#);
        let g = gate_with(
            "meta.requests",
            MetricRule { max_delta: Some(0.0), ..Default::default() },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert!(!r[0].pass);
    }

    #[test]
    fn rules_stack_as_and() {
        // both rules pass: 720 <= 1000 and 720 <= 700 * 1.10 = 770
        let cur = run(720, "{}", "{}");
        let base = run(700, "{}", "{}");
        let g = gate_with(
            "elapsed_ms",
            MetricRule {
                max: Some(1000.0),
                max_relative: Some(1.10),
                ..Default::default()
            },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|o| o.pass));

        // one rule passes (max=1000), the other fails (771 > 770)
        let cur = run(771, "{}", "{}");
        let r = evaluate(&g, &cur, &base).unwrap();
        assert_eq!(r.len(), 2);
        assert!(r.iter().any(|o| !o.pass));
        assert!(r.iter().any(|o| o.pass));
    }
}
