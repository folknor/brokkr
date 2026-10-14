//! Sync-bench regression gate evaluation.
//!
//! See `docs/commands/ratatoskr-gate.md` for the design. This module compiles
//! configured rules into typed metric selectors and predicates, pulls scalar
//! values out of two JSON-encoded `gate_runs` rows (current + baseline), and
//! reports per-rule pass/fail. Storage and CLI plumbing live in
//! `src/ratatoskr/gate_store.rs` and `src/ratatoskr/sync.rs`.

use std::fmt::{self, Write};

use serde_json::{Map, Value};

use crate::config::{GateConfig, MetricRule};
use crate::error::DevError;
use crate::ratatoskr::gate_store::GateEntry;

/// Outcome of evaluating one rule against one metric.
#[derive(Debug)]
pub struct RuleOutcome {
    pub metric: String,
    pub rule: String,
    pub pass: bool,
    pub detail: String,
}

/// The validated, executable form of a gate's configured rules.
///
/// Build this before compiling or running the harness. It validates every
/// metric selector and scalar `equal` value, and guarantees that each
/// metric has at least one predicate. Execution and rebase warnings then
/// consume this same representation.
#[derive(Debug)]
pub struct GateRules {
    metrics: Vec<MetricPredicates>,
}

#[derive(Debug)]
struct MetricPredicates {
    name: String,
    key: MetricKey,
    predicates: Vec<Predicate>,
}

#[derive(Debug)]
enum MetricKey {
    ElapsedMs,
    ExitCode,
    Success,
    Sidecar(String),
    Meta(String),
}

#[derive(Clone, Debug)]
enum Predicate {
    Max(f64),
    Min(f64),
    MaxRelative(f64),
    MinRelative(f64),
    MaxDelta(f64),
    Equal(Scalar),
    EqualToBaseline,
}

impl GateRules {
    pub fn compile(gate_name: &str, gate: &GateConfig) -> Result<Self, DevError> {
        let mut metrics = Vec::with_capacity(gate.metrics.len());
        for (name, rule) in &gate.metrics {
            let key = MetricKey::parse(name)
                .map_err(|reason| DevError::Config(format!("gate `{gate_name}`: {reason}")))?;
            let predicates = compile_predicates(gate_name, name, rule)?;
            if predicates.is_empty() {
                return Err(DevError::Config(format!(
                    "gate `{gate_name}`: no predicate set on metric `{name}` - a rule like that \
                     checks nothing and passes. Give it at least one of max, min, max_relative, \
                     min_relative, max_delta, equal, or equal_to_baseline = true."
                )));
            }
            metrics.push(MetricPredicates {
                name: name.clone(),
                key,
                predicates,
            });
        }
        Ok(Self { metrics })
    }

    pub fn evaluate(
        &self,
        current: &GateRun,
        baseline: &GateRun,
    ) -> Result<Vec<RuleOutcome>, DevError> {
        if self.metrics.is_empty() {
            return Err(DevError::Config(
                "gate has no metric rules to evaluate".into(),
            ));
        }
        let mut outcomes = Vec::new();
        for metric in &self.metrics {
            let current_value = lookup_scalar(current, &metric.key)?;
            let baseline_value = lookup_scalar(baseline, &metric.key)?;
            for predicate in &metric.predicates {
                outcomes.push(predicate.evaluate(&metric.name, &current_value, &baseline_value)?);
            }
        }
        Ok(outcomes)
    }

    pub fn no_drift_metrics(&self) -> Vec<&str> {
        self.metrics
            .iter()
            .filter(|metric| metric.predicates.iter().any(Predicate::admits_no_drift))
            .map(|metric| metric.name.as_str())
            .collect()
    }
}

impl MetricKey {
    fn parse(name: &str) -> Result<Self, String> {
        if let Some(key) = name.strip_prefix("sidecar.") {
            return if key.is_empty() {
                Err("metric `sidecar.` has an empty key".into())
            } else {
                Ok(Self::Sidecar(key.to_owned()))
            };
        }
        if let Some(key) = name.strip_prefix("meta.") {
            return if key.is_empty() {
                Err("metric `meta.` has an empty key".into())
            } else {
                Ok(Self::Meta(key.to_owned()))
            };
        }
        match name {
            "elapsed_ms" => Ok(Self::ElapsedMs),
            "exit_code" => Ok(Self::ExitCode),
            "success" => Ok(Self::Success),
            other => Err(format!(
                "unknown metric `{other}` (bare metrics are elapsed_ms, exit_code, success; \
                 other values use `sidecar.<key>` or `meta.<key>`)."
            )),
        }
    }
}

impl Predicate {
    fn name(&self) -> &'static str {
        match self {
            Self::Max(_) => "max",
            Self::Min(_) => "min",
            Self::MaxRelative(_) => "max_relative",
            Self::MinRelative(_) => "min_relative",
            Self::MaxDelta(_) => "max_delta",
            Self::Equal(_) => "equal",
            Self::EqualToBaseline => "equal_to_baseline",
        }
    }

    fn admits_no_drift(&self) -> bool {
        match self {
            Self::MaxDelta(delta) => *delta == 0.0,
            Self::EqualToBaseline => true,
            _ => false,
        }
    }

    fn evaluate(
        &self,
        metric: &str,
        current: &Scalar,
        baseline: &Scalar,
    ) -> Result<RuleOutcome, DevError> {
        let rule = self.name();
        match self {
            Self::Max(cap) => check_num(metric, rule, current, |value| {
                (
                    value <= *cap,
                    format!("current={} max={}", format_num(value), format_num(*cap)),
                )
            }),
            Self::Min(floor) => check_num(metric, rule, current, |value| {
                (
                    value >= *floor,
                    format!("current={} min={}", format_num(value), format_num(*floor)),
                )
            }),
            Self::MaxRelative(factor) => {
                let base = require_num(metric, rule, baseline, "baseline")?;
                let bound = base * *factor;
                check_num(metric, rule, current, |value| {
                    (
                        value <= bound,
                        format!(
                            "current={} bound={} (baseline={} * {})",
                            format_num(value),
                            format_num(bound),
                            format_num(base),
                            format_num(*factor)
                        ),
                    )
                })
            }
            Self::MinRelative(factor) => {
                let base = require_num(metric, rule, baseline, "baseline")?;
                let bound = base * *factor;
                check_num(metric, rule, current, |value| {
                    (
                        value >= bound,
                        format!(
                            "current={} bound={} (baseline={} * {})",
                            format_num(value),
                            format_num(bound),
                            format_num(base),
                            format_num(*factor)
                        ),
                    )
                })
            }
            Self::MaxDelta(delta) => {
                let base = require_num(metric, rule, baseline, "baseline")?;
                check_num(metric, rule, current, |value| {
                    let actual_delta = value - base;
                    (
                        actual_delta <= *delta,
                        format!(
                            "current={} baseline={} delta={} max_delta={}",
                            format_num(value),
                            format_num(base),
                            format_num(actual_delta),
                            format_num(*delta)
                        ),
                    )
                })
            }
            Self::Equal(expected) => {
                let pass = current == expected;
                Ok(RuleOutcome {
                    metric: metric.into(),
                    rule: rule.into(),
                    pass,
                    detail: format!("current={} expected={}", current.fmt(), expected.fmt()),
                })
            }
            Self::EqualToBaseline => {
                let pass = current == baseline;
                Ok(RuleOutcome {
                    metric: metric.into(),
                    rule: rule.into(),
                    pass,
                    detail: format!("current={} baseline={}", current.fmt(), baseline.fmt()),
                })
            }
        }
    }
}

fn compile_predicates(
    gate_name: &str,
    metric: &str,
    rule: &MetricRule,
) -> Result<Vec<Predicate>, DevError> {
    let mut predicates = Vec::new();
    if let Some(value) = rule.max {
        predicates.push(Predicate::Max(value));
    }
    if let Some(value) = rule.min {
        predicates.push(Predicate::Min(value));
    }
    if let Some(value) = rule.max_relative {
        predicates.push(Predicate::MaxRelative(value));
    }
    if let Some(value) = rule.min_relative {
        predicates.push(Predicate::MinRelative(value));
    }
    if let Some(value) = rule.max_delta {
        predicates.push(Predicate::MaxDelta(value));
    }
    if let Some(value) = &rule.equal {
        let value = toml_to_scalar(value).ok_or_else(|| {
            DevError::Config(format!(
                "gate `{gate_name}`: metric `{metric}` rule `equal`: value must be int/float/string/bool"
            ))
        })?;
        predicates.push(Predicate::Equal(value));
    }
    // False adds no check. If it is the only configured value, GateRules::compile
    // rejects the resulting empty predicate list.
    if rule.equal_to_baseline == Some(true) {
        predicates.push(Predicate::EqualToBaseline);
    }
    Ok(predicates)
}

/// Validate a configured gate before build work begins.
pub fn validate_rules(gate_name: &str, gate: &GateConfig) -> Result<(), DevError> {
    GateRules::compile(gate_name, gate).map(|_| ())
}

/// Evaluate a configured gate directly - the tests' shorthand. Runtime callers
/// compile once with [`GateRules::compile`] and call [`GateRules::evaluate`].
#[cfg(test)]
pub fn evaluate(
    gate: &GateConfig,
    current: &GateRun,
    baseline: &GateRun,
) -> Result<Vec<RuleOutcome>, DevError> {
    GateRules::compile("<gate>", gate)?.evaluate(current, baseline)
}

/// A canonical cargo feature set stored in a gate identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalFeatures(String);

impl CanonicalFeatures {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for CanonicalFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("default features")
        } else {
            write!(f, "features `{}`", self.0)
        }
    }
}

/// The fields that identify whether two gate runs measured the same setup.
/// `features = None` is reserved for pre-v2 database rows where that field
/// was never recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateIdentity {
    gate: String,
    /// Current rows use a project-relative path for in-project scripts;
    /// pre-existing rows may still contain a canonical absolute path.
    script: String,
    fixture: String,
    profile: crate::build::CargoProfile,
    features: Option<CanonicalFeatures>,
}

impl GateIdentity {
    pub fn new(
        gate: impl Into<String>,
        script: impl Into<String>,
        fixture: impl Into<String>,
        profile: crate::build::CargoProfile,
        features: Option<CanonicalFeatures>,
    ) -> Self {
        Self {
            gate: gate.into(),
            script: script.into(),
            fixture: fixture.into(),
            profile,
            features,
        }
    }

    /// Return the first identity field that differs. Call this on the
    /// baseline identity with the current identity as `other`; a missing
    /// baseline feature value is the compatibility path for pre-v2 rows.
    pub fn mismatch(&self, other: &Self) -> Option<GateIdentityMismatch> {
        if self.gate != other.gate {
            return Some(GateIdentityMismatch::Gate {
                baseline: self.gate.clone(),
                current: other.gate.clone(),
            });
        }
        if !same_script_identity(&self.script, &other.script) {
            return Some(GateIdentityMismatch::Script {
                baseline: self.script.clone(),
                current: other.script.clone(),
            });
        }
        if self.fixture != other.fixture {
            return Some(GateIdentityMismatch::Fixture {
                baseline: self.fixture.clone(),
                current: other.fixture.clone(),
            });
        }
        if self.profile != other.profile {
            return Some(GateIdentityMismatch::Profile {
                baseline: self.profile,
                current: other.profile,
            });
        }
        if let Some(baseline) = &self.features {
            match &other.features {
                Some(current) if baseline != current => {
                    return Some(GateIdentityMismatch::Features {
                        baseline: baseline.clone(),
                        current: current.clone(),
                    });
                }
                None => return Some(GateIdentityMismatch::FeaturesUnrecorded),
                _ => {}
            }
        }
        None
    }

    pub fn features_unrecorded(&self) -> bool {
        self.features.is_none()
    }
}

/// Older gate rows stored canonical absolute script paths. A moved checkout
/// compares those rows with the current project-relative path by components,
/// preserving the identity of the script within this project's gate store.
fn same_script_identity(baseline: &str, current: &str) -> bool {
    if baseline == current {
        return true;
    }
    let baseline = std::path::Path::new(baseline);
    let current = std::path::Path::new(current);
    baseline.is_absolute() && current.is_relative() && baseline.ends_with(current)
}

/// Why a baseline and a current run cannot be compared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateIdentityMismatch {
    Gate {
        baseline: String,
        current: String,
    },
    Script {
        baseline: String,
        current: String,
    },
    Fixture {
        baseline: String,
        current: String,
    },
    Profile {
        baseline: crate::build::CargoProfile,
        current: crate::build::CargoProfile,
    },
    Features {
        baseline: CanonicalFeatures,
        current: CanonicalFeatures,
    },
    FeaturesUnrecorded,
}

impl fmt::Display for GateIdentityMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gate { baseline, current } => {
                write!(
                    f,
                    "baseline row's gate is `{baseline}`, current gate is `{current}`"
                )
            }
            Self::Script { baseline, current } => {
                write!(f, "baseline script `{baseline}` != current `{current}`")
            }
            Self::Fixture { baseline, current } => {
                write!(f, "baseline fixture `{baseline}` != current `{current}`")
            }
            Self::Profile { baseline, current } => write!(
                f,
                "baseline was built `{}` but this run is `{}`. Re-run with the \
                 baseline's profile (--debug / --release), or pin a baseline \
                 recorded under this one.",
                baseline.as_str(),
                current.as_str()
            ),
            Self::Features { baseline, current } => write!(
                f,
                "baseline was built with {baseline} but this run with {current}. \
                 Build with the baseline's `[ratatoskr.harness] features`, or \
                 pin a baseline recorded under these."
            ),
            Self::FeaturesUnrecorded => {
                f.write_str("current run's cargo features were not recorded")
            }
        }
    }
}

/// The complete caller-facing result of a gate comparison.
pub enum GateVerdict {
    Incomparable(GateIdentityMismatch),
    Evaluated(Vec<RuleOutcome>),
}

impl GateVerdict {
    pub fn from_outcomes(outcomes: Vec<RuleOutcome>) -> Self {
        Self::Evaluated(outcomes)
    }

    pub fn incomparable(mismatch: GateIdentityMismatch) -> Self {
        Self::Incomparable(mismatch)
    }

    pub fn mismatch(&self) -> Option<&GateIdentityMismatch> {
        match self {
            Self::Incomparable(mismatch) => Some(mismatch),
            Self::Evaluated(_) => None,
        }
    }

    pub fn outcomes(&self) -> &[RuleOutcome] {
        match self {
            Self::Incomparable(_) => &[],
            Self::Evaluated(outcomes) => outcomes,
        }
    }

    pub fn passed(&self) -> bool {
        match self {
            Self::Incomparable(_) => false,
            Self::Evaluated(outcomes) => {
                !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.pass)
            }
        }
    }

    pub fn format_report(&self) -> String {
        let mut report = String::new();
        if let Self::Incomparable(mismatch) = self {
            return format!("  [INCOMPARABLE] {mismatch}");
        }
        for outcome in self.outcomes() {
            let tag = if outcome.pass { "OK  " } else { "FAIL" };
            writeln!(
                report,
                "  [{tag}] {} :: {} ({})",
                outcome.metric, outcome.rule, outcome.detail
            )
            .expect("writing to a String never fails");
        }
        if report.ends_with('\n') {
            report.pop();
        }
        report
    }
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
    /// JSON serialize+parse round-trip. Used by the sync bench for the
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

fn lookup_scalar(run: &GateRun, key: &MetricKey) -> Result<Scalar, DevError> {
    match key {
        MetricKey::ElapsedMs => Ok(Scalar::Num(run.elapsed_ms as f64)),
        MetricKey::ExitCode => Ok(Scalar::Num(f64::from(run.exit_code))),
        MetricKey::Success => Ok(Scalar::Num(if run.success { 1.0 } else { 0.0 })),
        MetricKey::Sidecar(name) => lookup_in_blob(&run.sidecar, name, "sidecar"),
        MetricKey::Meta(name) => lookup_in_blob(&run.meta, name, "meta"),
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

pub(crate) fn kind_of(v: &Value) -> &'static str {
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

    #[test]
    fn legacy_absolute_script_identity_survives_a_checkout_move() {
        let baseline = GateIdentity::new(
            "gate",
            "/old/checkout/crates/app/tests/sync/run.lua",
            "fixture",
            crate::build::CargoProfile::Release,
            Some(CanonicalFeatures::new("")),
        );
        let current = GateIdentity::new(
            "gate",
            "crates/app/tests/sync/run.lua",
            "fixture",
            crate::build::CargoProfile::Release,
            Some(CanonicalFeatures::new("")),
        );

        assert!(baseline.mismatch(&current).is_none());
    }

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
            MetricRule {
                max: Some(1000.0),
                ..Default::default()
            },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].pass);

        let g = gate_with(
            "elapsed_ms",
            MetricRule {
                max: Some(500.0),
                ..Default::default()
            },
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
            MetricRule {
                max_relative: Some(1.10),
                ..Default::default()
            },
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
            MetricRule {
                equal: Some(toml::Value::Integer(1)),
                ..Default::default()
            },
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
            MetricRule {
                equal_to_baseline: Some(true),
                ..Default::default()
            },
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
            MetricRule {
                max: Some(1.0),
                ..Default::default()
            },
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
            MetricRule {
                max_delta: Some(0.0),
                ..Default::default()
            },
        );
        let r = evaluate(&g, &cur, &base).unwrap();
        assert!(!r[0].pass);
        let rules = GateRules::compile("g", &g).unwrap();
        assert_eq!(rules.no_drift_metrics(), vec!["meta.requests"]);
    }

    #[test]
    fn invalid_metric_keys_and_equal_values_fail_during_compilation() {
        let g = gate_with(
            "elapsed",
            MetricRule {
                max: Some(1.0),
                ..Default::default()
            },
        );
        assert!(
            GateRules::compile("g", &g)
                .unwrap_err()
                .to_string()
                .contains("unknown metric")
        );

        let g = gate_with(
            "meta.correct",
            MetricRule {
                equal: Some(toml::Value::Array(Vec::new())),
                ..Default::default()
            },
        );
        assert!(
            GateRules::compile("g", &g)
                .unwrap_err()
                .to_string()
                .contains("must be int/float/string/bool")
        );
    }

    #[test]
    fn verdict_owns_pass_state_and_report() {
        let verdict = GateVerdict::from_outcomes(vec![RuleOutcome {
            metric: "elapsed_ms".into(),
            rule: "max".into(),
            pass: false,
            detail: "current=12 max=10".into(),
        }]);
        assert!(!verdict.passed());
        assert!(verdict.format_report().contains("[FAIL] elapsed_ms :: max"));
    }

    /// A metric table with no predicate used to produce zero outcomes and
    /// a PASSED report.
    #[test]
    fn predicate_free_rule_is_refused() {
        let cur = run(0, "{}", "{}");
        let base = run(0, "{}", "{}");
        let g = gate_with("elapsed_ms", MetricRule::default());
        let err = validate_rules("g", &g).unwrap_err();
        assert!(err.to_string().contains("elapsed_ms"), "got: {err}");
        assert!(evaluate(&g, &cur, &base).is_err());
    }

    #[test]
    fn equal_to_baseline_false_alone_is_refused() {
        let g = gate_with(
            "meta.messages",
            MetricRule {
                equal_to_baseline: Some(false),
                ..Default::default()
            },
        );
        assert!(validate_rules("g", &g).is_err());
        let g = gate_with(
            "meta.messages",
            MetricRule {
                equal_to_baseline: Some(true),
                ..Default::default()
            },
        );
        validate_rules("g", &g).unwrap();
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
