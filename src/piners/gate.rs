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
//! Beside the disposition, each pin may carry per-leg mismatch counts for
//! the label, performance and excursion censuses, and [`GateVerdict`]
//! ratchets them (`ratchet.rs`): a count that moves either way fails, as a
//! tier move does, until a bless adopts it. A census needs no per-probe
//! judgement of whether a probe's labels or excursions should agree - a
//! probe whose values legitimately differ just pins its count. An unpinned
//! leg is ungated until a bless stamps it, so adopting a ratchet changes no
//! existing verdict. None of them moves the tier.
//!
//! The gate's verdict is also what reconciles it with the harness exit code:
//! [`breaks_all_pinned`] decides whether an exit of 1 is fully explained by
//! breaks the pins expect, which is what lets a pinned `compile_fail` pass.

use std::collections::{BTreeMap, HashSet};

use crate::output;
use crate::piners::ratchet::{self, Assessment, Coverage, Deviation, RatchetKind};
use crate::piners::registry::{Pin, PinnedDisposition, Registry, Verdict};
use crate::piners::report::{HarnessReport, ProbeLine, ProbeOutcome};

/// One probe whose actual disposition does not satisfy its pinned `expected`.
#[derive(Debug)]
pub struct GateDiff {
    pub probe: String,
    /// Pinned expectation; `None` = never blessed.
    pub expected: Option<Verdict>,
    /// Actual disposition this run, an abort with its death and a failure
    /// with its cause; `None` =
    /// harness emitted no line.
    pub actual: Option<String>,
}

/// Whether `outcome` satisfies the pin `expected`: the one comparison the
/// gate and the exit-one acceptance share, plain [`Verdict`] equality, so a
/// pinned abort holds only its own death, and a pinned failure only its own
/// cause, on both paths.
fn holds(expected: Option<Verdict>, outcome: Option<Verdict>) -> bool {
    matches!((expected, outcome), (Some(e), Some(g)) if e == g)
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
    let actual: BTreeMap<&str, (String, Option<Verdict>)> = report
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
                Some(outcome) => (outcome.shown(), outcome.evaluated()),
            };
            (p.probe.as_str(), got)
        })
        .collect();

    let mut diffs = Vec::new();
    for id in ids {
        let expected = registry.pins.get(id).and_then(Pin::expectation);
        let got = actual.get(id.as_str());
        if !holds(expected, got.and_then(|(_, g)| *g)) {
            diffs.push(GateDiff {
                probe: id.clone(),
                expected,
                actual: got.map(|(shown, _)| shown.clone()),
            });
        }
    }
    diffs
}

/// One ratchet kind's verdict over the selection.
#[derive(Debug, Default)]
pub struct RatchetVerdict {
    /// Every deviation, with its probe, in selection then leg order.
    pub deviations: Vec<(String, Deviation)>,
    /// Every selected probe's assessment, stored with the run.
    pub assessments: BTreeMap<String, Assessment>,
    pub coverage: Coverage,
}

/// The whole gate verdict: disposition deviations (the only kind that can
/// mean "no disposition emitted", so the only source of `gate_miss`), and
/// one verdict per ratchet kind (`ratchet.rs`), every kind present once
/// [`GateVerdict::evaluate`] ran.
#[derive(Debug, Default)]
pub struct GateVerdict {
    pub dispositions: Vec<GateDiff>,
    pub ratchets: BTreeMap<RatchetKind, RatchetVerdict>,
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
    /// The full gate: dispositions, then every ratchet. Each ratchet is told
    /// whether the disposition check alone deviated - never whether another
    /// ratchet did - and that excuses only a missing leg, never a moved one.
    pub fn evaluate(ids: &[String], registry: &Registry, report: &HarnessReport) -> Self {
        let dispositions = evaluate(ids, registry, report);
        let deviating: HashSet<&str> = dispositions.iter().map(|d| d.probe.as_str()).collect();
        let tainted = report.tainted();
        let lines: BTreeMap<&str, &ProbeLine> = report
            .probes
            .iter()
            .map(|p| (p.probe.as_str(), p))
            .collect();
        let mut ratchets: BTreeMap<RatchetKind, RatchetVerdict> = RatchetKind::ALL
            .into_iter()
            .map(|k| (k, RatchetVerdict::default()))
            .collect();
        for id in ids {
            // Only a valid, untainted line is checked.
            let line = lines
                .get(id.as_str())
                .filter(|p| p.outcome().is_some() && !tainted.contains(id.as_str()));
            let parity = line
                .and_then(|p| p.outcome())
                .and_then(ProbeOutcome::disposition)
                .is_some_and(PinnedDisposition::is_parity);
            let pin = registry.pins.get(id);
            for (kind, verdict) in &mut ratchets {
                let pinned = pin.and_then(|p| p.ratchet(*kind));
                let deviations = line.map(|p| {
                    let census = p.census(*kind);
                    verdict.coverage.record(*kind, parity, pinned, census);
                    ratchet::check(*kind, pinned, census, deviating.contains(id.as_str()))
                });
                if let Some(ds) = &deviations {
                    verdict
                        .deviations
                        .extend(ds.iter().map(|d| (id.clone(), d.clone())));
                }
                verdict.assessments.insert(
                    id.clone(),
                    Assessment {
                        pinned: pinned.cloned(),
                        deviations,
                    },
                );
            }
        }
        Self {
            dispositions,
            ratchets,
        }
    }

    /// One kind's verdict, empty when the gate never ran it.
    pub fn ratchet(&self, kind: RatchetKind) -> Option<&RatchetVerdict> {
        self.ratchets.get(&kind)
    }

    fn ratchet_deviations(&self) -> impl Iterator<Item = &(String, Deviation)> {
        self.ratchets.values().flat_map(|v| v.deviations.iter())
    }

    pub fn is_empty(&self) -> bool {
        self.dispositions.is_empty() && self.ratchet_deviations().next().is_none()
    }

    /// Every deviating probe, any kind.
    pub fn deviating(&self) -> HashSet<&str> {
        self.dispositions
            .iter()
            .map(|d| d.probe.as_str())
            .chain(self.ratchet_deviations().map(|(p, _)| p.as_str()))
            .collect()
    }

    /// The deviation count for the verdict line, one per probe.
    pub fn deviating_count(&self) -> usize {
        self.deviating().len()
    }

    /// Render every deviation to the `[corpus]` log.
    pub fn render(&self) {
        render_diffs(&self.dispositions);
        for (probe, d) in self.ratchet_deviations() {
            output::corpus_msg(&format!("gate: {probe}: {}", d.render()));
        }
    }

    /// The coverage lines, one per kind with anything to count.
    pub fn coverage_lines(&self) -> Vec<String> {
        self.ratchets
            .iter()
            .filter_map(|(kind, v)| v.coverage.line(*kind))
            .collect()
    }
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
        let pinned = !tainted.contains(p.probe.as_str())
            && selected.contains(p.probe.as_str())
            && holds(
                registry.pins.get(&p.probe).and_then(Pin::expectation),
                outcome.evaluated(),
            );
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
    use crate::piners::ratchet::LegPin;
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
        // A failure pin holds the cause the fixture lines carry.
        if matches!(
            p.expected,
            Some(PinnedDisposition::CompileFail | PinnedDisposition::RuntimeFail)
        ) {
            p.cause = Some(crate::piners::registry::Cause::Script);
        }
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
            "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}\n{\"probe\":\"b\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"accepted\"}}",
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
        let rep = report(r#"{"probe":"a","outcome":"compile_fail","cause":"script","error":"x"}"#);
        assert!(!breaks_all_pinned(&["a".to_owned()], &reg, &rep));
        // `c` is pinned to its break but was not selected.
        let rep = report(r#"{"probe":"c","outcome":"compile_fail","cause":"script","error":"x"}"#);
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
        let rep = report(r#"{"probe":"a","outcome":"compile_fail","cause":"script","disposition":"accepted"}"#);
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
            "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"disposition\":\"accepted\"}\n{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"cause\":\"script\"}",
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
        let expected: BTreeMap<String, crate::piners::registry::Expectation> = [(
            "a".to_owned(),
            crate::piners::registry::Expectation {
                verdict: Some(Verdict::CompileFail(crate::piners::registry::Cause::Script)),
                attribution: None,
            },
        )]
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
        let mut reg = registry(&[("a", Some("accepted"))]);
        if let Some(p) = reg.pins.get_mut("a") {
            p.set_expectation(
                Verdict::HarnessAbort(crate::piners::registry::Death::Signal(11)),
                Some("engine recursion".to_owned()),
            );
        }
        let abort = |n: i32| {
            report(&format!(
                r#"{{"probe":"a","outcome":"harness_abort","error":"killed by signal {n}","death":{{"kind":"signal","number":{n}}}}}"#
            ))
        };
        let rep = abort(11);
        let ids = ["a".to_owned()];
        assert!(evaluate(&ids, &reg, &rep).is_empty());
        assert!(report_has_break(&rep));
        assert!(breaks_all_pinned(&ids, &reg, &rep));
        // The same label on another death deviates and leaves exit 1
        // unexplained, on both paths.
        let moved = abort(9);
        let diffs = evaluate(&ids, &reg, &moved);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].actual.as_deref(), Some("harness_abort (signal 9)"));
        assert_eq!(
            diffs[0].expected.map(|v| v.to_string()).as_deref(),
            Some("harness_abort (signal 11)")
        );
        assert!(!breaks_all_pinned(&ids, &reg, &moved));
        // Unpinned, the crash is a deviation and leaves exit 1 unexplained.
        let unpinned = registry(&[("a", Some("accepted"))]);
        assert_eq!(evaluate(&ids, &unpinned, &rep).len(), 1);
        assert!(!breaks_all_pinned(&ids, &unpinned, &rep));
    }

    #[test]
    fn a_pinned_failure_holds_its_cause_on_both_paths() {
        let reg = registry(&[("a", Some("runtime_fail"))]);
        let fail = |cause: &str| {
            report(&format!(
                r#"{{"probe":"a","outcome":"runtime_fail","cause":"{cause}","error":"e"}}"#
            ))
        };
        let ids = ["a".to_owned()];
        let rep = fail("script");
        assert!(evaluate(&ids, &reg, &rep).is_empty());
        assert!(breaks_all_pinned(&ids, &reg, &rep));
        // A typed stop that turns into a panic deviates and leaves exit 1
        // unexplained.
        let panic = fail("panic");
        let diffs = evaluate(&ids, &reg, &panic);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].actual.as_deref(), Some("runtime_fail (panic)"));
        assert_eq!(
            diffs[0].expected.map(|v| v.to_string()).as_deref(),
            Some("runtime_fail (script)")
        );
        assert!(!breaks_all_pinned(&ids, &reg, &panic));
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
            p.labels = labels.map(|(entry, exit)| LegPin::from([("entry", entry), ("exit", exit)]));
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

    fn labels(v: &GateVerdict) -> &RatchetVerdict {
        v.ratchet(RatchetKind::Labels).unwrap()
    }

    #[test]
    fn a_label_count_that_rises_or_falls_fails_and_equal_passes() {
        let ids = ["a".to_owned()];
        let reg = labelled("a", "byte_exact", Some((0, 2)));
        let same = report(&census_line("a", "byte_exact", 0, 2, 5));
        let v = GateVerdict::evaluate(&ids, &reg, &same);
        assert!(v.is_empty(), "ambiguous moves are never gated");
        assert_eq!(labels(&v).coverage.ratcheted, 1);
        // Entry rose, exit fell: both legs reported, each its own way.
        let moved = report(&census_line("a", "byte_exact", 3, 0, 0));
        let v = GateVerdict::evaluate(&ids, &reg, &moved);
        assert!(v.dispositions.is_empty());
        assert_eq!(labels(&v).deviations.len(), 2);
        assert_eq!(v.deviating_count(), 1);
        let rendered: Vec<String> = labels(&v)
            .deviations
            .iter()
            .map(|(_, d)| d.render())
            .collect();
        assert!(rendered[0].contains("labels entry mismatches rose 0 -> 3"));
        assert!(rendered[1].contains("fell"));
        assert_eq!(
            labels(&v).assessments["a"].deviations_json().unwrap(),
            r#"[{"actual":3,"ambiguous":0,"compared":10,"leg":"entry","pinned":0},{"actual":0,"ambiguous":0,"compared":10,"leg":"exit","pinned":2}]"#
        );
    }

    #[test]
    fn an_unpinned_probe_is_counted_not_gated() {
        let ids = ["a".to_owned()];
        let reg = labelled("a", "accepted", None);
        let v = GateVerdict::evaluate(&ids, &reg, &report(&census_line("a", "accepted", 4, 4, 1)));
        assert!(v.is_empty());
        assert_eq!(
            labels(&v).coverage,
            Coverage {
                ratcheted: 0,
                unpinned: 1,
                ambiguous: 1
            }
        );
        assert_eq!(
            labels(&v).assessments["a"].deviations_json().as_deref(),
            Some("[]")
        );
        assert_eq!(labels(&v).assessments["a"].pinned_json(), None);
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
             {\"probe\":\"b\",\"outcome\":\"compile_fail\",\"cause\":\"script\",\"error\":\"x\"}",
        );
        let v = GateVerdict::evaluate(&ids, &reg, &rep);
        assert!(v.is_empty());
        // The break has no labels to pin; only the parity probe awaits one.
        assert_eq!(labels(&v).coverage.unpinned, 1);
        assert!(v.coverage_lines()[0].contains("1 with an unpinned leg"));
    }

    #[test]
    fn a_pinned_probe_without_a_census_fails_unless_its_disposition_already_does() {
        let ids = ["a".to_owned()];
        // A parity line that lost its census: each pinned leg is missing.
        let reg = labelled("a", "accepted", Some((0, 0)));
        let rep = report(r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#);
        let v = GateVerdict::evaluate(&ids, &reg, &rep);
        assert_eq!(labels(&v).deviations.len(), 2);
        assert!(matches!(
            labels(&v).deviations[0].1,
            Deviation::Missing { leg: "entry", .. }
        ));
        assert_eq!(v.deviating_count(), 1);
        assert_eq!(labels(&v).coverage, Coverage::default()); // missing, not unpinned
        // A non-parity line under a labels pin whose disposition matches.
        let reg = labelled("a", "compile_fail", Some((0, 0)));
        let rep = report(r#"{"probe":"a","outcome":"compile_fail","cause":"script","error":"x"}"#);
        assert_eq!(
            labels(&GateVerdict::evaluate(&ids, &reg, &rep))
                .deviations
                .len(),
            2
        );
        // A disposition deviation already says enough.
        let reg = labelled("a", "accepted", Some((0, 0)));
        let v = GateVerdict::evaluate(&ids, &reg, &rep);
        assert_eq!((v.dispositions.len(), labels(&v).deviations.len()), (1, 0));
    }

    #[test]
    fn one_ratchets_deviation_never_excuses_another_ratchets_missing_leg() {
        // Labels moved and the performance pin's leg is missing, on a line
        // whose disposition matches: both fail, whatever order they run in.
        let ids = ["a".to_owned()];
        let mut reg = labelled("a", "accepted", Some((0, 0)));
        reg.pins.get_mut("a").unwrap().performance = Some(LegPin::from([("equity", 0)]));
        let v = GateVerdict::evaluate(&ids, &reg, &report(&census_line("a", "accepted", 1, 0, 0)));
        assert_eq!(labels(&v).deviations.len(), 1);
        let perf = v.ratchet(RatchetKind::Performance).unwrap();
        assert!(matches!(
            perf.deviations[0].1,
            Deviation::Missing { leg: "equity", .. }
        ));
        assert_eq!(v.deviating_count(), 1);
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
        assert!(labels(&v).deviations.is_empty());
        assert_eq!(v.dispositions.len(), 2);
        assert_eq!(labels(&v).assessments["a"].deviations, None);
        assert_eq!(labels(&v).assessments["b"].deviations, None);
    }

    #[test]
    fn a_moved_performance_leg_fails_the_gate_and_leaves_the_tier_alone() {
        let ids = ["a".to_owned()];
        let mut reg = registry(&[("a", Some("byte_exact"))]);
        reg.pins.get_mut("a").unwrap().performance = Some(LegPin::from([("equity", 0)]));
        let line = |equity: u64| {
            report(&format!(
                "{{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"byte_exact\"}},\
                 \"performance\":{{\"equity\":{{\"compared\":9,\"mismatched\":{equity},\
                 \"tolerance\":\"exact\"}},\"scalars\":{{\"compared\":4,\"mismatched\":2,\
                 \"tolerance\":\"exact\"}}}}}}"
            ))
        };
        fn perf(v: &GateVerdict) -> &RatchetVerdict {
            v.ratchet(RatchetKind::Performance).unwrap()
        }
        let v = GateVerdict::evaluate(&ids, &reg, &line(0));
        assert!(v.is_empty(), "an unpinned scalars leg is not gated");
        assert_eq!(perf(&v).coverage.unpinned, 1);
        let v = GateVerdict::evaluate(&ids, &reg, &line(3));
        assert!(v.dispositions.is_empty());
        assert_eq!(perf(&v).deviations.len(), 1);
        assert_eq!(v.deviating_count(), 1);
        assert_eq!(
            perf(&v).assessments["a"].deviations_json().unwrap(),
            r#"[{"actual":3,"compared":9,"leg":"equity","pinned":0,"tolerance":"exact"}]"#
        );
        // A census that cannot describe one population invalidates the line.
        let bad = report(
            "{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"byte_exact\"},\
             \"performance\":{}}",
        );
        assert!(bad.probes[0].record_defect().is_some());
    }

    #[test]
    fn a_moved_excursion_leg_fails_the_gate_and_an_unpinned_one_awaits_a_stamp() {
        let ids = ["a".to_owned()];
        let mut reg = registry(&[("a", Some("accepted"))]);
        let line = |run_up: u64| {
            report(&format!(
                "{{\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{{\"tier\":\"accepted\"}},\
                 \"excursions\":{{\"run_up\":{{\"compared\":9,\"mismatched\":{run_up},\
                 \"ambiguous\":1,\"unavailable\":0,\"tolerance\":\"float32\"}},\
                 \"drawdown\":{{\"compared\":9,\"mismatched\":0,\"ambiguous\":0,\
                 \"unavailable\":0,\"tolerance\":\"float32\"}}}}}}"
            ))
        };
        let v = GateVerdict::evaluate(&ids, &reg, &line(2));
        assert!(v.is_empty(), "an unpinned census is not gated");
        let lines = v.coverage_lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("excursions: 0 probes ratcheted, 1 with an unpinned leg")),
            "{lines:?}"
        );
        reg.pins.get_mut("a").unwrap().excursions = Some(LegPin::from([("run_up", 2)]));
        assert!(GateVerdict::evaluate(&ids, &reg, &line(2)).is_empty());
        let v = GateVerdict::evaluate(&ids, &reg, &line(1));
        let exc = v.ratchet(RatchetKind::Excursions).unwrap();
        assert_eq!(exc.deviations.len(), 1);
        assert!(exc.deviations[0].1.render().contains(
            "excursions run_up mismatches fell (improvement - re-bless to adopt) 2 -> 1"
        ));
        assert!(v.dispositions.is_empty());
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
