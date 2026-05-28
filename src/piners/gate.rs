//! The per-probe expected-disposition gate.
//!
//! Replaces the old aggregate floors (the `≥132 exact` style thresholds),
//! which let a regression on one probe hide behind an improvement on
//! another. Instead each probe pins an `expected` disposition in `pins.toml`
//! and brokkr fails the run on *any* deviation - a regression
//! (`accepted -> count_divergent`) and a surprise improvement
//! (`actionable_drift -> accepted`) alike, since both mean the pinned
//! contract is stale and a human should re-bless.
//!
//! Two non-deviation conditions are also violations: a probe with no
//! `expected` yet (freshly reseeded, never blessed - "must bless") and a
//! selected probe the harness emitted no disposition for.

use std::collections::BTreeMap;

use crate::output;
use crate::piners::registry::Registry;
use crate::piners::report::HarnessReport;

/// One probe whose actual disposition does not satisfy its pinned `expected`.
#[derive(Debug)]
pub struct GateDiff {
    pub probe: String,
    /// Pinned expectation; `None` = never blessed.
    pub expected: Option<String>,
    /// Actual disposition this run; `None` = harness emitted no line.
    pub actual: Option<String>,
}

/// Compare each selected probe's actual disposition to its pinned `expected`.
/// Returns the violations; an empty vec means the gate passed.
pub fn evaluate(ids: &[String], registry: &Registry, report: &HarnessReport) -> Vec<GateDiff> {
    let actual: BTreeMap<&str, String> = report
        .probes
        .iter()
        .map(|p| (p.probe.as_str(), p.disposition()))
        .collect();

    let mut diffs = Vec::new();
    for id in ids {
        let expected = registry.pins.get(id).and_then(|p| p.expected.clone());
        let got = actual.get(id.as_str()).cloned();
        let satisfied = matches!((&expected, &got), (Some(e), Some(g)) if e == g);
        if !satisfied {
            diffs.push(GateDiff {
                probe: id.clone(),
                expected,
                actual: got,
            });
        }
    }
    diffs
}

/// Render the gate diffs to the `[corpus]` log, one line per probe.
pub fn render_diffs(diffs: &[GateDiff]) {
    for d in diffs {
        let detail = match (&d.expected, &d.actual) {
            (None, Some(a)) => {
                format!("{}: not blessed (got {a}) - run `brokkr corpus --bless`", d.probe)
            }
            (Some(e), None) => {
                format!("{}: expected {e}, but harness emitted no disposition", d.probe)
            }
            (Some(e), Some(a)) => format!("{}: expected {e}, got {a}", d.probe),
            (None, None) => format!("{}: not blessed and no disposition emitted", d.probe),
        };
        output::corpus_msg(&format!("gate: {detail}"));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::registry::{FilePin, Pin};
    use std::path::PathBuf;

    fn pin(expected: Option<&str>) -> Pin {
        Pin {
            expected: expected.map(str::to_owned),
            pine: FilePin {
                path: PathBuf::from("p.pine"),
                xxh128: "00".into(),
            },
            csv: FilePin {
                path: PathBuf::from("p.csv"),
                xxh128: "11".into(),
            },
        }
    }

    fn registry(pins: &[(&str, Option<&str>)]) -> Registry {
        let mut map = BTreeMap::new();
        for (id, exp) in pins {
            map.insert((*id).to_owned(), pin(*exp));
        }
        Registry {
            pins: map,
            keywords: BTreeMap::new(),
        }
    }

    fn report(lines: &str) -> HarnessReport {
        crate::piners::report::parse(lines.as_bytes())
    }

    #[test]
    fn passes_when_actual_matches_expected() {
        let reg = registry(&[("a", Some("accepted"))]);
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        assert!(evaluate(&["a".to_owned()], &reg, &rep).is_empty());
    }

    #[test]
    fn flags_regression_and_surprise_improvement() {
        let reg = registry(&[("a", Some("accepted")), ("b", Some("actionable_drift"))]);
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"count_divergent\"}}\n{\"probe\":\"b\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"accepted\"}}",
        );
        let diffs = evaluate(&["a".to_owned(), "b".to_owned()], &reg, &rep);
        assert_eq!(diffs.len(), 2); // both directions fail
    }

    #[test]
    fn missing_expected_is_a_violation() {
        let reg = registry(&[("a", None)]);
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        let diffs = evaluate(&["a".to_owned()], &reg, &rep);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].expected, None);
        assert_eq!(diffs[0].actual.as_deref(), Some("accepted"));
    }

    #[test]
    fn missing_actual_is_a_violation() {
        let reg = registry(&[("a", Some("accepted"))]);
        let rep = report(""); // harness emitted nothing
        let diffs = evaluate(&["a".to_owned()], &reg, &rep);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].actual, None);
    }
}
