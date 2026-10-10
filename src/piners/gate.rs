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
//!
//! The gate's verdict is also what reconciles it with the harness exit code:
//! [`breaks_all_pinned`] decides whether an exit of 1 is fully explained by
//! breaks the pins expect, which is what lets a pinned `compile_fail` pass.

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
    // Only a valid disposition (`report::valid_label`) can satisfy a pin: an
    // invalid line whose derived label happens to equal `expected` - a tier
    // posing as an outcome - is a deviation, shown as `invalid (...)`.
    let actual: BTreeMap<&str, String> = report
        .probes
        .iter()
        .map(|p| {
            let got = p
                .valid_disposition()
                .map_or_else(|| format!("invalid ({})", p.disposition()), str::to_owned);
            (p.probe.as_str(), got)
        })
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

/// The dispositions the harness signals with exit code 1 ("break(s)"). A
/// probe can pin any of them: a probe exercising an unimplemented feature
/// legitimately expects `compile_fail`, and one whose process the harness
/// saw die is `harness_abort`.
pub const BREAK_DISPOSITIONS: [&str; 3] = ["compile_fail", "runtime_fail", "harness_abort"];

/// Whether the report carries at least one break line (any probe).
pub fn report_has_break(report: &HarnessReport) -> bool {
    report
        .probes
        .iter()
        .any(|p| p.valid_disposition().is_some_and(|d| BREAK_DISPOSITIONS.contains(&d)))
}

/// Whether a harness exit of 1 is fully accounted for by pinned breaks: the
/// report carries at least one break line, and every break line belongs to a
/// selected probe whose pin expects exactly that break.
///
/// Exit 1 means "some probe broke", which is also what a probe pinned to
/// `compile_fail` does on every passing run. Without this, a pinned break
/// could never pass the gate - the exit code would fail the run the gate had
/// just accepted. An unpinned, mispinned or unselected break still leaves the
/// exit unexplained, so the exit code stays authoritative for those.
pub fn breaks_all_pinned(ids: &[String], registry: &Registry, report: &HarnessReport) -> bool {
    let selected: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut any = false;
    for p in &report.probes {
        let Some(disp) = p.valid_disposition().filter(|d| BREAK_DISPOSITIONS.contains(d)) else {
            continue;
        };
        any = true;
        let pinned = selected.contains(p.probe.as_str())
            && registry.pins.get(&p.probe).and_then(|pin| pin.expected.as_deref())
                == Some(disp);
        if !pinned {
            return false;
        }
    }
    any
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
        let mut p = Pin::new(
            FilePin {
                path: PathBuf::from("p.pine"),
                xxh128: "00".into(),
            },
            FilePin {
                path: PathBuf::from("p.csv"),
                xxh128: "11".into(),
            },
        );
        p.expected = expected.map(str::to_owned);
        p
    }

    fn registry(pins: &[(&str, Option<&str>)]) -> Registry {
        let mut map = BTreeMap::new();
        for (id, exp) in pins {
            map.insert((*id).to_owned(), pin(*exp));
        }
        Registry {
            pins: map,
            ..Registry::default()
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
    fn a_pinned_break_explains_exit_one() {
        let reg = registry(&[("a", Some("compile_fail")), ("b", Some("accepted"))]);
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"error\":\"x\"}\n{\"probe\":\"b\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"accepted\"}}",
        );
        let ids = ["a".to_owned(), "b".to_owned()];
        assert!(evaluate(&ids, &reg, &rep).is_empty());
        assert!(breaks_all_pinned(&ids, &reg, &rep));
        assert!(report_has_break(&rep));
    }

    #[test]
    fn an_unpinned_or_unselected_break_does_not_explain_exit_one() {
        // `a` expects a runtime_fail but compile-failed: mispinned.
        let reg = registry(&[("a", Some("runtime_fail")), ("c", Some("compile_fail"))]);
        let rep = report(r#"{"probe":"a","outcome":"compile_fail","error":"x"}"#);
        assert!(!breaks_all_pinned(&["a".to_owned()], &reg, &rep));
        // `c` is pinned to its break but was not selected.
        let rep = report(r#"{"probe":"c","outcome":"compile_fail","error":"x"}"#);
        assert!(!breaks_all_pinned(&["a".to_owned()], &reg, &rep));
        // No break line at all: exit 1 is unexplained.
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        assert!(!breaks_all_pinned(&["a".to_owned()], &reg, &rep));
        assert!(!report_has_break(&rep));
    }

    #[test]
    fn an_invalid_line_never_satisfies_its_pin() {
        let reg = registry(&[("a", Some("accepted")), ("b", Some("runtime_fail"))]);
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"accepted\"}\n{\"probe\":\"b\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"runtime_fail\"}}",
        );
        let ids = ["a".to_owned(), "b".to_owned()];
        let diffs = evaluate(&ids, &reg, &rep);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].actual.as_deref(), Some("invalid (accepted)"));
        // Nor does a posing break explain exit 1.
        assert!(!report_has_break(&rep));
        assert!(!breaks_all_pinned(&ids, &reg, &rep));
    }

    #[test]
    fn a_pinned_harness_abort_passes_and_explains_exit_one() {
        let reg = registry(&[("a", Some("harness_abort"))]);
        let rep = report(r#"{"probe":"a","outcome":"harness_abort","error":"killed by signal 11"}"#);
        let ids = ["a".to_owned()];
        assert!(evaluate(&ids, &reg, &rep).is_empty());
        assert!(report_has_break(&rep));
        assert!(breaks_all_pinned(&ids, &reg, &rep));
        // Unpinned, the crash is a deviation and leaves exit 1 unexplained.
        let unpinned = registry(&[("a", Some("accepted"))]);
        assert_eq!(evaluate(&ids, &unpinned, &rep).len(), 1);
        assert!(!breaks_all_pinned(&ids, &unpinned, &rep));
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
