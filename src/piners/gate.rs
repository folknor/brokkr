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
//! Beside the disposition, each pin may carry its per-leg displayed-label
//! mismatch counts (`labels`), and [`GateVerdict`] ratchets them: a count
//! that moves either way fails, as a tier move does, until a bless adopts
//! it. The census needs no per-probe judgement of whether a probe's labels
//! should agree - a probe whose labels legitimately differ (counter-built
//! ids, value-bearing comments) just pins its count. A pin without `labels`
//! is ungated for labels (counted as unpinned) until a bless stamps it, so
//! adopting the ratchet changes no existing verdict. `ambiguous` moves with
//! the pairing and is reported, never gated.
//!
//! The gate's verdict is also what reconciles it with the harness exit code:
//! [`breaks_all_pinned`] decides whether an exit of 1 is fully explained by
//! breaks the pins expect, which is what lets a pinned `compile_fail` pass.

use std::collections::{BTreeMap, HashSet};

use crate::output;
use crate::piners::registry::{LabelPin, PinnedDisposition, Registry};
use crate::piners::report::{HarnessReport, LabelCensus, ProbeLine, ProbeOutcome};

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
    // Only a valid record (`report::record_label`) can satisfy a pin: an
    // invalid line whose derived label happens to equal `expected` - a tier
    // posing as an outcome - is a deviation, shown as `invalid (...)`. So is
    // a clean line for a probe any earlier occurrence of which was invalid:
    // repeats collapse to the last, but an invalid occurrence never
    // satisfies a pin. The stored `gate_ok` is this verdict, so it carries
    // the taint to `corpus-results` too.
    let tainted = report.tainted();
    // Per probe: what the line shows, and the evaluated disposition it can
    // satisfy a pin with - none for an invalid, tainted or unevaluated line.
    let actual: BTreeMap<&str, (String, Option<PinnedDisposition>)> = report
        .probes
        .iter()
        .map(|p| {
            let got = match p.outcome() {
                None => (format!("invalid ({})", p.disposition()), None),
                Some(_) if tainted.contains(p.probe.as_str()) => (
                    format!(
                        "invalid (an occurrence was invalid; last {})",
                        p.disposition()
                    ),
                    None,
                ),
                Some(outcome) => (outcome.label().to_owned(), outcome.evaluated()),
            };
            (p.probe.as_str(), got)
        })
        .collect();

    let mut diffs = Vec::new();
    for id in ids {
        let expected = registry.pins.get(id).and_then(|p| p.expected);
        let got = actual.get(id.as_str());
        let satisfied = matches!((expected, got), (Some(e), Some((_, Some(g)))) if e == *g);
        if !satisfied {
            diffs.push(GateDiff {
                probe: id.clone(),
                expected: expected.map(|d| d.as_str().to_owned()),
                actual: got.map(|(shown, _)| shown.clone()),
            });
        }
    }
    diffs
}

/// A displayed-label leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    Entry,
    Exit,
}

impl Leg {
    pub const fn as_str(self) -> &'static str {
        match self {
            Leg::Entry => "entry",
            Leg::Exit => "exit",
        }
    }
}

/// One way a probe's labels fail their ratchet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelDeviation {
    /// The leg's mismatch count differs from its pin. A rise is a
    /// regression; a fall is an improvement to adopt by re-blessing, failed
    /// like a tier improvement so the ratchet tightens instead of leaving the
    /// old allowance for a later regression to hide in. `ambiguous` rides
    /// along for the reader, never compared.
    Moved {
        leg: Leg,
        pinned: u64,
        actual: u64,
        ambiguous: u64,
    },
    /// The pin has label counts, the disposition matches, and the line
    /// carries no census: a ratchet cannot pass on missing evidence.
    Missing { pinned: LabelPin },
}

impl LabelDeviation {
    fn to_json(&self) -> serde_json::Value {
        match self {
            LabelDeviation::Moved {
                leg,
                pinned,
                actual,
                ambiguous,
            } => serde_json::json!({
                "leg": leg.as_str(), "pinned": pinned, "actual": actual, "ambiguous": ambiguous
            }),
            LabelDeviation::Missing { pinned } => serde_json::json!({
                "missing": true, "pinned": { "entry": pinned.entry, "exit": pinned.exit }
            }),
        }
    }

    fn render(&self) -> String {
        match self {
            LabelDeviation::Moved {
                leg,
                pinned,
                actual,
                ambiguous,
            } => {
                let way = if actual > pinned {
                    "rose"
                } else {
                    "fell (improvement - re-bless to adopt)"
                };
                format!(
                    "{} label mismatches {way} {pinned} -> {actual} ({ambiguous} ambiguous, not gated)",
                    leg.as_str()
                )
            }
            LabelDeviation::Missing { pinned } => format!(
                "labels pinned entry={} exit={}, but the line carries no label census",
                pinned.entry, pinned.exit
            ),
        }
    }
}

/// The label ratchet's view of one selected probe, stored with its run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LabelAssessment {
    /// The pin's label counts at run time; `None` = not yet stamped.
    pub pinned: Option<LabelPin>,
    /// The deviations found, `None` when the probe was not label-checked (no
    /// valid untainted line).
    pub deviations: Option<Vec<LabelDeviation>>,
}

impl LabelAssessment {
    /// The pinned counts as stored JSON.
    pub fn pinned_json(&self) -> Option<String> {
        self.pinned
            .map(|p| serde_json::json!({ "entry": p.entry, "exit": p.exit }).to_string())
    }

    /// The deviations as stored JSON: `[]` when checked and clean.
    pub fn deviations_json(&self) -> Option<String> {
        self.deviations.as_ref().map(|ds| {
            serde_json::Value::Array(ds.iter().map(LabelDeviation::to_json).collect()).to_string()
        })
    }
}

/// How many label-checked probes are ratcheted, how many await their first
/// stamp, and the ambiguous legs across them (reported, never gated).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LabelCoverage {
    pub ratcheted: usize,
    pub unpinned: usize,
    pub ambiguous: u64,
}

/// The whole gate verdict: disposition deviations (the only kind that can
/// mean "no disposition emitted", so the only source of `gate_miss`), label
/// deviations, and every selected probe's label assessment.
#[derive(Debug, Default)]
pub struct GateVerdict {
    pub dispositions: Vec<GateDiff>,
    pub labels: Vec<(String, LabelDeviation)>,
    pub assessments: BTreeMap<String, LabelAssessment>,
    pub coverage: LabelCoverage,
}

impl From<Vec<GateDiff>> for GateVerdict {
    fn from(dispositions: Vec<GateDiff>) -> Self {
        Self {
            dispositions,
            ..Self::default()
        }
    }
}

impl GateVerdict {
    /// The full gate: dispositions, then the label ratchet.
    pub fn evaluate(ids: &[String], registry: &Registry, report: &HarnessReport) -> Self {
        let dispositions = evaluate(ids, registry, report);
        let deviating: HashSet<&str> = dispositions.iter().map(|d| d.probe.as_str()).collect();
        let tainted = report.tainted();
        let lines: BTreeMap<&str, &ProbeLine> = report
            .probes
            .iter()
            .map(|p| (p.probe.as_str(), p))
            .collect();
        let mut labels = Vec::new();
        let mut assessments = BTreeMap::new();
        let mut coverage = LabelCoverage::default();
        for id in ids {
            let pinned = registry.pins.get(id).and_then(|p| p.labels);
            let line = lines
                .get(id.as_str())
                .filter(|p| p.outcome().is_some() && !tainted.contains(id.as_str()));
            let deviations = line.map(|p| {
                let census = p.census;
                let parity = p
                    .outcome()
                    .and_then(ProbeOutcome::evaluated)
                    .is_some_and(PinnedDisposition::is_parity);
                // Unpinned counts every parity probe awaiting its first stamp,
                // census or not, so a harness predating the census still
                // shows up as ungated; a pinned probe missing its census is
                // missing evidence, in neither bucket.
                match (pinned.is_some(), census.is_some()) {
                    (true, true) => coverage.ratcheted += 1,
                    (false, _) if parity => coverage.unpinned += 1,
                    _ => {}
                }
                if let Some(c) = census {
                    coverage.ambiguous = coverage
                        .ambiguous
                        .saturating_add(c.entry.ambiguous)
                        .saturating_add(c.exit.ambiguous);
                }
                check_labels(pinned, census, deviating.contains(id.as_str()))
            });
            if let Some(ds) = &deviations {
                labels.extend(ds.iter().map(|d| (id.clone(), d.clone())));
            }
            assessments.insert(id.clone(), LabelAssessment { pinned, deviations });
        }
        Self {
            dispositions,
            labels,
            assessments,
            coverage,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.dispositions.is_empty() && self.labels.is_empty()
    }

    /// Every deviating probe, either kind.
    pub fn deviating(&self) -> HashSet<&str> {
        self.dispositions
            .iter()
            .map(|d| d.probe.as_str())
            .chain(self.labels.iter().map(|(p, _)| p.as_str()))
            .collect()
    }

    /// The deviation count for the verdict line, one per probe.
    pub fn deviating_count(&self) -> usize {
        self.deviating().len()
    }

    /// Render every deviation, then the label coverage, to the `[corpus]`
    /// log.
    pub fn render(&self) {
        render_diffs(&self.dispositions);
        for (probe, d) in &self.labels {
            output::corpus_msg(&format!("gate: {probe}: {}", d.render()));
        }
    }

    /// The coverage line, when any parity probe was label-checked.
    pub fn coverage_line(&self) -> Option<String> {
        let c = &self.coverage;
        (c.ratcheted + c.unpinned > 0).then(|| {
            let mut line = format!("labels: {} ratcheted", output::count(c.ratcheted, "probe"));
            if c.unpinned > 0 {
                line.push_str(&format!(
                    ", {} unpinned (labels ungated until blessed)",
                    c.unpinned
                ));
            }
            line.push_str(&format!(", {} ambiguous legs (not gated)", c.ambiguous));
            line
        })
    }
}

/// The label check for one probe with a valid untainted line.
fn check_labels(
    pinned: Option<LabelPin>,
    census: Option<LabelCensus>,
    disposition_deviates: bool,
) -> Vec<LabelDeviation> {
    let Some(pin) = pinned else {
        return Vec::new(); // never stamped: ungated, counted as unpinned
    };
    let Some(c) = census else {
        // A disposition that already deviates says enough; otherwise the
        // missing evidence is itself the failure.
        return if disposition_deviates {
            Vec::new()
        } else {
            vec![LabelDeviation::Missing { pinned: pin }]
        };
    };
    [
        (Leg::Entry, pin.entry, c.entry),
        (Leg::Exit, pin.exit, c.exit),
    ]
    .into_iter()
    .filter(|(_, pinned, leg)| leg.mismatched != *pinned)
    .map(|(leg, pinned, census)| LabelDeviation::Moved {
        leg,
        pinned,
        actual: census.mismatched,
        ambiguous: census.ambiguous,
    })
    .collect()
}

/// Whether the report carries at least one break line (any probe).
pub fn report_has_break(report: &HarnessReport) -> bool {
    let tainted = report.tainted();
    report.probes.iter().any(|p| {
        !tainted.contains(p.probe.as_str()) && p.outcome().is_some_and(ProbeOutcome::is_break)
    })
}

/// Whether a harness exit of 1 is fully accounted for by pinned breaks: the
/// report carries at least one break line, and every break line belongs to a
/// selected probe whose pin expects exactly that break.
///
/// Exit 1 means "some probe broke", which is also what a probe pinned to
/// `compile_fail` does on every passing run. Without this, a pinned break
/// could never pass the gate - the exit code would fail the run the gate had
/// just accepted. An unpinned, mispinned or unselected break still leaves the
/// exit unexplained, so the exit code stays authoritative for those, and an
/// `unevaluated` line, a break no pin can hold, always does.
pub fn breaks_all_pinned(ids: &[String], registry: &Registry, report: &HarnessReport) -> bool {
    let selected: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let tainted = report.tainted();
    let mut any = false;
    for p in &report.probes {
        let Some(outcome) = p.outcome().filter(|outcome| outcome.is_break()) else {
            continue;
        };
        any = true;
        // A probe with an invalid occurrence explains nothing, and neither
        // does an unevaluated one: it has no disposition a pin could hold.
        let pinned = outcome.evaluated().is_some_and(|disp| {
            !tainted.contains(p.probe.as_str())
                && selected.contains(p.probe.as_str())
                && registry.pins.get(&p.probe).and_then(|pin| pin.expected) == Some(disp)
        });
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
                format!(
                    "{}: not blessed (got {a}) - run `brokkr corpus --bless`",
                    d.probe
                )
            }
            (Some(e), None) => {
                format!(
                    "{}: expected {e}, but harness emitted no disposition",
                    d.probe
                )
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
        p.expected = expected.map(|label| {
            PinnedDisposition::from_label(label).expect("test disposition is recognized")
        });
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
    fn a_contradictory_disposition_field_never_satisfies_a_pin_or_explains_a_break() {
        // Pinned to the field's claim, and to the derived label: neither passes.
        let rep = report(r#"{"probe":"a","outcome":"compile_fail","disposition":"accepted"}"#);
        let ids = ["a".to_owned()];
        for pin in ["accepted", "compile_fail"] {
            let reg = registry(&[("a", Some(pin))]);
            assert_eq!(evaluate(&ids, &reg, &rep).len(), 1, "{pin}");
            assert!(!breaks_all_pinned(&ids, &reg, &rep), "{pin}");
        }
        assert!(!report_has_break(&rep));
    }

    #[test]
    fn a_contradictory_then_clean_repeat_never_satisfies_its_pin() {
        let mut rep = report(
            "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"disposition\":\"accepted\"}\n{\"probe\":\"a\",\"outcome\":\"compile_fail\"}",
        );
        rep.take_duplicates();
        let ids = ["a".to_owned()];
        let reg = registry(&[("a", Some("compile_fail"))]);
        let diffs = evaluate(&ids, &reg, &rep);
        assert_eq!(diffs.len(), 1);
        assert!(
            diffs[0]
                .actual
                .as_deref()
                .unwrap()
                .starts_with("invalid (an occurrence was invalid")
        );
        assert!(!report_has_break(&rep));
        assert!(!breaks_all_pinned(&ids, &reg, &rep));
        // The stored verdict follows: ingest marks the row gate_ok = 0.
        let db_path = crate::test_scratch::scratch_path("piners_gate", "tainted_runs.db");
        let db = crate::piners::corpus_db::CorpusDb::open(&db_path).unwrap();
        let run = crate::piners::corpus_db::RunRecord {
            run_id: None,
            started_at: None,
            commit_sha: None,
            dirty: None,
            selector: "{}",
            context: None,
            gated: true,
            verdict: &crate::piners::run_model::RunVerdict::Fail("test gate deviation".into()),
            harness_exit_code: Some(1),
            stderr: "",
            wall_ms: None,
            protocol_violations: Some(2),
            run_error: None,
        };
        let expected: BTreeMap<String, Option<String>> =
            [("a".to_owned(), Some("compile_fail".to_owned()))]
                .into_iter()
                .collect();
        let rec = crate::piners::integrity::Reconciliation::reconcile(&ids, &rep, Vec::new());
        let run_id = db
            .record_run(&run, &rep, &rec, &expected, &GateVerdict::from(diffs))
            .unwrap();
        let stored = db.disposition_for_probe(run_id, "a").unwrap().unwrap();
        assert!(!stored.gate_ok);
        // The clean last line is stored, unscored, with the occurrence that
        // tainted it.
        assert_eq!(stored.scored, Some(false));
        assert!(
            stored.defect.as_deref().unwrap().starts_with("line 1: "),
            "{:?}",
            stored.defect
        );
    }

    #[test]
    fn a_pinned_harness_abort_passes_and_explains_exit_one() {
        let reg = registry(&[("a", Some("harness_abort"))]);
        let rep =
            report(r#"{"probe":"a","outcome":"harness_abort","error":"killed by signal 11"}"#);
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
    fn an_unevaluated_line_never_satisfies_a_pin_or_explains_exit_one() {
        let rep = report(
            r#"{"probe":"a","outcome":"unevaluated","reason":"invalid_evidence","error":"bad row"}"#,
        );
        let ids = ["a".to_owned()];
        // No pin can name it, so every pin deviates, a pinned break included.
        for expected in ["accepted", "runtime_fail"] {
            let reg = registry(&[("a", Some(expected))]);
            let diffs = evaluate(&ids, &reg, &rep);
            assert_eq!(diffs.len(), 1, "{expected}");
            assert_eq!(diffs[0].actual.as_deref(), Some("unevaluated"));
            assert!(report_has_break(&rep));
            assert!(!breaks_all_pinned(&ids, &reg, &rep), "{expected}");
        }
    }

    fn labelled(id: &str, expected: &str, labels: Option<(u64, u64)>) -> Registry {
        let mut reg = registry(&[(id, Some(expected))]);
        if let Some(p) = reg.pins.get_mut(id) {
            p.labels = labels.map(|(entry, exit)| LabelPin { entry, exit });
        }
        reg
    }

    fn census_line(id: &str, tier: &str, entry: u64, exit: u64, ambiguous: u64) -> String {
        format!(
            "{{\"probe\":\"{id}\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"{tier}\"}},\
             \"labels\":{{\"entry\":{{\"compared\":10,\"mismatched\":{entry},\"ambiguous\":{ambiguous},\
             \"unavailable\":0}},\"exit\":{{\"compared\":10,\"mismatched\":{exit},\"ambiguous\":0,\
             \"unavailable\":0}}}}}}"
        )
    }

    #[test]
    fn a_label_count_that_rises_or_falls_fails_and_equal_passes() {
        let ids = ["a".to_owned()];
        let reg = labelled("a", "byte_exact", Some((0, 2)));
        let same = report(&census_line("a", "byte_exact", 0, 2, 5));
        let v = GateVerdict::evaluate(&ids, &reg, &same);
        assert!(v.is_empty(), "ambiguous moves are never gated");
        assert_eq!(v.coverage.ratcheted, 1);
        // Entry rose, exit fell: both legs reported, each its own way.
        let moved = report(&census_line("a", "byte_exact", 3, 0, 0));
        let v = GateVerdict::evaluate(&ids, &reg, &moved);
        assert!(v.dispositions.is_empty());
        assert_eq!(v.labels.len(), 2);
        assert_eq!(v.deviating_count(), 1);
        let rendered: Vec<String> = v.labels.iter().map(|(_, d)| d.render()).collect();
        assert!(rendered[0].contains("rose 0 -> 3"));
        assert!(rendered[1].contains("fell"));
        assert_eq!(
            v.assessments["a"].deviations_json().unwrap(),
            r#"[{"actual":3,"ambiguous":0,"leg":"entry","pinned":0},{"actual":0,"ambiguous":0,"leg":"exit","pinned":2}]"#
        );
    }

    #[test]
    fn an_unpinned_probe_is_counted_not_gated() {
        let ids = ["a".to_owned()];
        let reg = labelled("a", "accepted", None);
        let v = GateVerdict::evaluate(&ids, &reg, &report(&census_line("a", "accepted", 4, 4, 1)));
        assert!(v.is_empty());
        assert_eq!(
            v.coverage,
            LabelCoverage {
                ratcheted: 0,
                unpinned: 1,
                ambiguous: 1
            }
        );
        assert_eq!(v.assessments["a"].deviations_json().as_deref(), Some("[]"));
        assert_eq!(v.assessments["a"].pinned_json(), None);
    }

    #[test]
    fn a_parity_probe_from_a_harness_predating_the_census_counts_as_unpinned() {
        let ids = ["a".to_owned(), "b".to_owned()];
        let mut reg = labelled("a", "accepted", None);
        reg.pins.insert(
            "b".to_owned(),
            labelled("b", "compile_fail", None).pins["b"].clone(),
        );
        let rep = report(
            "{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"accepted\"}}\n\
             {\"probe\":\"b\",\"outcome\":\"compile_fail\",\"error\":\"x\"}",
        );
        let v = GateVerdict::evaluate(&ids, &reg, &rep);
        assert!(v.is_empty());
        // The break has no labels to pin; only the parity probe awaits one.
        assert_eq!(v.coverage.unpinned, 1);
        assert!(v.coverage_line().unwrap().contains("1 unpinned"));
    }

    #[test]
    fn a_pinned_probe_without_a_census_fails_unless_its_disposition_already_does() {
        let ids = ["a".to_owned()];
        // A parity line that lost its census.
        let reg = labelled("a", "accepted", Some((0, 0)));
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        let v = GateVerdict::evaluate(&ids, &reg, &rep);
        assert!(matches!(v.labels[0].1, LabelDeviation::Missing { .. }));
        assert_eq!(v.coverage, LabelCoverage::default()); // missing, not unpinned
        // A non-parity line under a labels pin whose disposition matches.
        let reg = labelled("a", "compile_fail", Some((0, 0)));
        let rep = report(r#"{"probe":"a","outcome":"compile_fail","error":"x"}"#);
        assert_eq!(GateVerdict::evaluate(&ids, &reg, &rep).labels.len(), 1);
        // A disposition deviation already says enough.
        let reg = labelled("a", "accepted", Some((0, 0)));
        let v = GateVerdict::evaluate(&ids, &reg, &rep);
        assert_eq!((v.dispositions.len(), v.labels.len()), (1, 0));
    }

    #[test]
    fn an_invalid_line_is_not_label_checked_and_no_disposition_is_a_gate_miss_only() {
        let ids = ["a".to_owned(), "b".to_owned()];
        let mut reg = labelled("a", "accepted", Some((0, 0)));
        reg.pins.insert(
            "b".to_owned(),
            labelled("b", "accepted", Some((0, 0))).pins["b"].clone(),
        );
        // `a` carries a census whose counts cannot describe one population;
        // `b` emitted nothing.
        let rep = report(&census_line("a", "accepted", 11, 0, 0));
        let v = GateVerdict::evaluate(&ids, &reg, &rep);
        assert!(v.labels.is_empty());
        assert_eq!(v.dispositions.len(), 2);
        assert_eq!(v.assessments["a"].deviations, None);
        assert_eq!(v.assessments["b"].deviations, None);
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
