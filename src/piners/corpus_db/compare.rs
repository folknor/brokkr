//! `corpus-results --compare A B`: two runs, probe by probe.
//!
//! The gate compares a probe's acceptance tier against its pin, so a probe
//! whose matched / ours-only / TradingView-only counts move while it stays in
//! one tier passes it unseen. This view reports exactly those moves: every
//! probe whose counts, `count_tier`, outcome or disposition differ between the
//! two runs, read B against A.
//!
//! What the comparison can and cannot claim:
//!
//! - A probe missing from one side is classified, not dropped: the run's
//!   stored selection (`selector.ids`) and `gate_miss` say whether it was not
//!   selected or was selected and emitted nothing. Legacy or malformed
//!   selectors stay explicitly unknown.
//! - Counts are projected as 0 when the harness line carried none (a compile
//!   failure has no trade counts). Such a side is shown as `no counts` rather
//!   than as a fall to zero, which would read as a divergence improvement.
//! - `count_tier` is compared NULL-safely: a tier appearing or vanishing is a
//!   move.
//! - The direction column is a heuristic for comparable parity executions,
//!   never a verdict: fewer matched trades can be a shorter window, and raw
//!   unmatched counts include boundary artifacts the label discounts. So it
//!   names the movement (`more divergent` / `less divergent` / `mixed`), and
//!   the two run headers carry what the store knows of each side's context
//!   (commit, profile, forwarded harness flags) for the reader to judge.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::CorpusDb;
use super::ingest::RunContext;
use super::query::RunRow;
use crate::error::DevError;

/// One probe's line in one run, as the comparison needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Side {
    /// Whether the run selected the probe. `NotSelected` is a report-only
    /// line (the harness emitted it unasked), which the gate never judged.
    pub selection: Selection,
    pub outcome: String,
    pub disposition: String,
    pub count_tier: Option<String>,
    /// Whether the harness line carried all three trade counts as numbers.
    /// The projections read 0 for an absent or non-numeric count, so without
    /// this a missing count would compare as a real zero.
    pub has_counts: bool,
    pub matched: i64,
    pub ours_only: i64,
    pub tv_only: i64,
}

/// What the stored run can establish about a probe's selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Selected,
    NotSelected,
    Unknown,
}

/// Where a probe stands in one run.
#[derive(Debug, Clone, PartialEq)]
pub enum Presence {
    Ran(Side),
    /// Selected, but the harness emitted no line for it.
    NoLine,
    /// Not part of that run's selection.
    NotSelected,
    /// The selector is legacy or malformed, so selection cannot be recovered.
    SelectionUnknown,
}

/// A probe that differs between the two runs (or, under `--full`, any probe).
#[derive(Debug)]
pub struct ProbeDelta {
    pub probe: String,
    pub a: Presence,
    pub b: Presence,
}

impl ProbeDelta {
    /// Did anything the comparison tracks change?
    pub fn moved(&self) -> bool {
        self.a != self.b
    }
}

/// The whole comparison.
pub struct Comparison {
    pub run_a: RunRow,
    pub run_b: RunRow,
    /// Every probe either run selected or emitted, by id.
    pub probes: Vec<ProbeDelta>,
}

impl CorpusDb {
    /// Load both runs and pair their probes. A run id with no run row is an
    /// error, never an empty comparison.
    pub fn compare_runs(&self, a: i64, b: i64) -> Result<Comparison, DevError> {
        let run_a = self.run(a)?.ok_or_else(|| no_run(a))?;
        let run_b = self.run(b)?.ok_or_else(|| no_run(b))?;
        let selected_a = selected_ids(&run_a.selector);
        let selected_b = selected_ids(&run_b.selector);
        let side_a = self.presence_map(&run_a, selected_a.as_deref())?;
        let side_b = self.presence_map(&run_b, selected_b.as_deref())?;
        let ids: BTreeSet<&String> = side_a.keys().chain(side_b.keys()).collect();
        let probes = ids
            .into_iter()
            .map(|id| ProbeDelta {
                probe: id.clone(),
                a: side_a
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| missing_presence(selected_a.as_deref(), id)),
                b: side_b
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| missing_presence(selected_b.as_deref(), id)),
            })
            .collect();
        Ok(Comparison {
            run_a,
            run_b,
            probes,
        })
    }

    /// Where each of `probes` stands in run `run_id`, by the same evidence
    /// the comparison reads: a stored disposition, then `gate_miss`, then
    /// the stored selector. An id with no run row is an error.
    pub fn presence_of(
        &self,
        run_id: i64,
        probes: &[String],
    ) -> Result<Vec<(String, Presence)>, DevError> {
        let run = self.run(run_id)?.ok_or_else(|| no_run(run_id))?;
        let selected = selected_ids(&run.selector);
        let map = self.presence_map(&run, selected.as_deref())?;
        Ok(probes
            .iter()
            .map(|p| {
                let presence = map
                    .get(p)
                    .cloned()
                    .unwrap_or_else(|| missing_presence(selected.as_deref(), p));
                (p.clone(), presence)
            })
            .collect())
    }

    /// Every probe the run selected or emitted, with what it emitted.
    fn presence_map(
        &self,
        run: &RunRow,
        selector_ids: Option<&[String]>,
    ) -> Result<BTreeMap<String, Presence>, DevError> {
        let mut out: BTreeMap<String, Presence> = BTreeMap::new();
        let mut selected: BTreeSet<String> = selector_ids.into_iter().flatten().cloned().collect();
        for miss in self.gate_misses_for_run(run.run_id)? {
            selected.insert(miss.probe);
        }
        for id in &selected {
            out.insert(id.clone(), Presence::NoLine);
        }
        let numeric = |path: &str| {
            format!("COALESCE(json_type(raw_json, '{path}') IN ('integer', 'real'), 0)")
        };
        let sql = format!(
            "SELECT probe, outcome, disposition, count_tier, matched, ours_only, tv_only, \
                    ({} AND {} AND {}) AS has_counts \
             FROM disposition WHERE run_id = ?1",
            numeric("$.matched"),
            numeric("$.ours_only"),
            numeric("$.tv_only")
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map([run.run_id], |r| {
            let probe = r.get::<_, String>("probe")?;
            Ok((
                probe.clone(),
                Side {
                    selection: if selected.contains(&probe) {
                        Selection::Selected
                    } else if selector_ids.is_some() {
                        Selection::NotSelected
                    } else {
                        Selection::Unknown
                    },
                    outcome: r.get::<_, Option<String>>("outcome")?.unwrap_or_default(),
                    disposition: r.get("disposition")?,
                    count_tier: r.get("count_tier")?,
                    has_counts: r.get::<_, i64>("has_counts")? != 0,
                    matched: r.get("matched")?,
                    ours_only: r.get("ours_only")?,
                    tv_only: r.get("tv_only")?,
                },
            ))
        })?;
        for row in rows {
            let (probe, side) = row?;
            out.insert(probe, Presence::Ran(side));
        }
        Ok(out)
    }
}

fn missing_presence(selector_ids: Option<&[String]>, probe: &str) -> Presence {
    let Some(ids) = selector_ids else {
        return Presence::SelectionUnknown;
    };
    if ids.iter().any(|id| id == probe) {
        Presence::NoLine
    } else {
        Presence::NotSelected
    }
}

fn no_run(id: i64) -> DevError {
    DevError::Config(format!(
        "corpus-results: no run {id} in runs.db (bare `brokkr corpus-results` lists the runs)"
    ))
}

/// The resolved ids a run's stored selector names; `None` for an
/// unparsable, malformed, or pre-`ids` selector.
fn selected_ids(selector: &str) -> Option<Vec<String>> {
    let value = serde_json::from_str::<Value>(selector).ok()?;
    let ids = value.get("ids")?.as_array()?;
    ids.iter()
        .map(|id| id.as_str().map(str::to_owned))
        .collect()
}

/// Which way a probe's counts moved, as a heuristic for comparable parity
/// executions. `None` when either side has no counts or nothing moved.
pub fn direction(a: &Side, b: &Side) -> Option<&'static str> {
    if !a.has_counts || !b.has_counts {
        return None;
    }
    // Each axis on its own: summing the two unmatched deltas would let one
    // side's rise cancel the other's fall.
    let deltas = [
        a.matched - b.matched,
        b.ours_only - a.ours_only,
        b.tv_only - a.tv_only,
    ];
    let worse = deltas.iter().any(|d| *d > 0);
    let better = deltas.iter().any(|d| *d < 0);
    match (worse, better) {
        (true, false) => Some("more divergent"),
        (false, true) => Some("less divergent"),
        (true, true) => Some("mixed"),
        (false, false) => None,
    }
}

/// The execution context a run row can establish. Legacy or malformed rows
/// remain unknown rather than receiving the parity command's dev default.
/// Equal contexts are necessary for comparable counts, not sufficient: two
/// commits can define the same custom profile differently.
pub fn context_of(run: &RunRow) -> Option<&RunContext> {
    run.context.as_ref()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::collections::BTreeMap;

    use super::*;
    use crate::piners::corpus_db::{RunContext, RunRecord, compare_report};
    use crate::piners::gate::{GateDiff, GateVerdict};
    use crate::piners::report::parse;

    fn record(db: &CorpusDb, ids: &[&str], nd: &str, misses: &[&str]) -> i64 {
        let selector = serde_json::json!({ "ids": ids, "debug": true }).to_string();
        record_with_selector(db, &selector, nd, misses)
    }

    fn record_with_selector(db: &CorpusDb, selector: &str, nd: &str, misses: &[&str]) -> i64 {
        let run = RunRecord {
            run_id: None,
            started_at: None,
            commit_sha: Some("0123456789abcdef0123456789abcdef01234567"),
            dirty: Some(false),
            selector,
            context: None,
            gated: true,
            verdict: &crate::piners::run_model::RunVerdict::Pass,
            harness_exit_code: Some(0),
            stderr: "",
            wall_ms: None,
            protocol_violations: Some(0),
            run_error: None,
        };
        let diffs: Vec<GateDiff> = misses
            .iter()
            .map(|p| GateDiff {
                probe: (*p).to_owned(),
                expected: Some("accepted".to_owned()),
                actual: None,
            })
            .collect();
        db.record_run(
            &run,
            &parse(nd.as_bytes()),
            &BTreeMap::new(),
            &GateVerdict::from(diffs),
        )
            .unwrap()
    }

    fn line(probe: &str, tier: &str, m: i64, o: i64, t: i64) -> String {
        format!(
            "{{\"probe\":\"{probe}\",\"outcome\":\"parity\",\"matched\":{m},\"ours_only\":{o},\
             \"tv_only\":{t},\"count_tier\":\"{tier}\",\"acceptance\":{{\"tier\":\"accepted\"}}}}\n"
        )
    }

    fn contextual_record(db: &CorpusDb, context: &RunContext) -> i64 {
        let selector = r#"{"ids":["p"],"debug":true,"harness_args":[]}"#;
        let run = RunRecord {
            run_id: None,
            started_at: None,
            commit_sha: None,
            dirty: None,
            selector,
            context: Some(context),
            gated: true,
            verdict: &crate::piners::run_model::RunVerdict::Pass,
            harness_exit_code: Some(0),
            stderr: "",
            wall_ms: None,
            protocol_violations: Some(0),
            run_error: None,
        };
        db.record_run(
            &run,
            &parse(line("p", "exact", 1, 0, 0).as_bytes()),
            &BTreeMap::new(),
            &GateVerdict::default(),
        )
        .unwrap()
    }

    #[test]
    fn compare_reads_the_typed_context_and_marks_legacy_context_unknown() {
        let db = CorpusDb::open_in_memory().unwrap();
        let corpus = RunContext {
            profile: crate::build::RustProfile::parse("corpus").unwrap(),
            harness_args: Vec::new(),
        };
        let release = RunContext {
            profile: crate::build::RustProfile::Release,
            harness_args: vec!["--scan".to_owned()],
        };
        let a = contextual_record(&db, &corpus);
        let b = contextual_record(&db, &release);
        let rendered = compare_report(&db.compare_runs(a, b).unwrap(), false);
        assert!(
            rendered.contains("A: corpus, B: release -- --scan"),
            "{rendered}"
        );
        // The same named profile on both sides is the same context.
        let c = contextual_record(&db, &corpus);
        let rendered = compare_report(&db.compare_runs(a, c).unwrap(), false);
        assert!(
            !rendered.contains("differ in execution context"),
            "{rendered}"
        );

        let legacy = record(&db, &["p"], &line("p", "exact", 1, 0, 0), &[]);
        let rendered = compare_report(&db.compare_runs(a, legacy).unwrap(), false);
        assert!(
            rendered.contains("context unknown for at least one run"),
            "{rendered}"
        );
    }

    #[test]
    fn a_count_move_inside_one_tier_is_reported() {
        let db = CorpusDb::open_in_memory().unwrap();
        let a = record(
            &db,
            &["p", "q"],
            &(line("p", "near", 100, 2, 1) + &line("q", "exact", 5, 0, 0)),
            &[],
        );
        let b = record(
            &db,
            &["p", "q"],
            &(line("p", "near", 97, 4, 1) + &line("q", "exact", 5, 0, 0)),
            &[],
        );
        let c = db.compare_runs(a, b).unwrap();
        let moved: Vec<&ProbeDelta> = c.probes.iter().filter(|d| d.moved()).collect();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].probe, "p");
        let (Presence::Ran(sa), Presence::Ran(sb)) = (&moved[0].a, &moved[0].b) else {
            panic!("both ran");
        };
        assert_eq!(direction(sa, sb), Some("more divergent"));
    }

    #[test]
    fn a_missing_side_is_classified_not_dropped() {
        let db = CorpusDb::open_in_memory().unwrap();
        let a = record(
            &db,
            &["p", "q"],
            &(line("p", "exact", 1, 0, 0) + &line("q", "exact", 1, 0, 0)),
            &[],
        );
        // q selected but emitted nothing; r new to B; p not selected in B.
        let b = record(&db, &["q", "r"], &line("r", "exact", 1, 0, 0), &["q"]);
        let c = db.compare_runs(a, b).unwrap();
        let by: BTreeMap<&str, &ProbeDelta> =
            c.probes.iter().map(|d| (d.probe.as_str(), d)).collect();
        assert_eq!(by["p"].b, Presence::NotSelected);
        assert_eq!(by["q"].b, Presence::NoLine);
        assert_eq!(by["r"].a, Presence::NotSelected);
    }

    #[test]
    fn legacy_and_malformed_selectors_remain_unknown() {
        let db = CorpusDb::open_in_memory().unwrap();
        let known = record(&db, &["p"], &line("p", "exact", 1, 0, 0), &[]);
        let legacy =
            record_with_selector(&db, r#"{"all":true}"#, &line("q", "exact", 1, 0, 0), &[]);
        let malformed = record_with_selector(&db, r#"{"ids":["p",false]}"#, "", &[]);

        let legacy_comparison = db.compare_runs(known, legacy).unwrap();
        let by: BTreeMap<&str, &ProbeDelta> = legacy_comparison
            .probes
            .iter()
            .map(|delta| (delta.probe.as_str(), delta))
            .collect();
        assert_eq!(by["p"].b, Presence::SelectionUnknown);
        let Presence::Ran(q) = &by["q"].b else {
            panic!("q was emitted")
        };
        assert_eq!(q.selection, Selection::Unknown);
        assert!(compare_report(&legacy_comparison, false).contains("selection unknown"));

        let malformed_comparison = db.compare_runs(known, malformed).unwrap();
        assert_eq!(malformed_comparison.probes[0].b, Presence::SelectionUnknown);
    }

    #[test]
    fn a_line_without_counts_is_not_a_fall_to_zero() {
        let db = CorpusDb::open_in_memory().unwrap();
        let a = record(&db, &["p"], &line("p", "near", 10, 3, 3), &[]);
        let b = record(
            &db,
            &["p"],
            "{\"probe\":\"p\",\"outcome\":\"compile_fail\",\"error\":\"boom\"}\n",
            &[],
        );
        let c = db.compare_runs(a, b).unwrap();
        let (Presence::Ran(sa), Presence::Ran(sb)) = (&c.probes[0].a, &c.probes[0].b) else {
            panic!("both ran");
        };
        assert!(!sb.has_counts);
        assert_eq!(direction(sa, sb), None);
        assert!(c.probes[0].moved());
    }

    #[test]
    fn opposing_unmatched_moves_are_mixed_not_cancelled() {
        let side = |o, t| Side {
            selection: Selection::Selected,
            outcome: "parity".into(),
            disposition: "accepted".into(),
            count_tier: None,
            has_counts: true,
            matched: 10,
            ours_only: o,
            tv_only: t,
        };
        assert_eq!(direction(&side(10, 10), &side(15, 5)), Some("mixed"));
        assert_eq!(direction(&side(10, 10), &side(16, 5)), Some("mixed"));
        assert_eq!(
            direction(&side(10, 10), &side(10, 12)),
            Some("more divergent")
        );
    }

    #[test]
    fn a_partial_count_triplet_is_no_counts_and_an_unselected_line_is_marked() {
        let db = CorpusDb::open_in_memory().unwrap();
        let a = record(&db, &["p"], &line("p", "near", 10, 0, 0), &[]);
        // B emits p with `matched` alone, plus q nobody asked for.
        let b = record(
            &db,
            &["p"],
            &("{\"probe\":\"p\",\"outcome\":\"parity\",\"matched\":10,\
               \"acceptance\":{\"tier\":\"accepted\"}}\n"
                .to_owned()
                + &line("q", "exact", 1, 0, 0)),
            &[],
        );
        let c = db.compare_runs(a, b).unwrap();
        let by: BTreeMap<&str, &ProbeDelta> =
            c.probes.iter().map(|d| (d.probe.as_str(), d)).collect();
        let Presence::Ran(pb) = &by["p"].b else {
            panic!("p ran")
        };
        assert!(!pb.has_counts);
        let Presence::Ran(qb) = &by["q"].b else {
            panic!("q ran")
        };
        assert_eq!(qb.selection, Selection::NotSelected);
    }

    #[test]
    fn a_tier_vanishing_is_a_move() {
        let db = CorpusDb::open_in_memory().unwrap();
        let a = record(&db, &["p"], &line("p", "near", 1, 0, 0), &[]);
        let b = record(
            &db,
            &["p"],
            "{\"probe\":\"p\",\"outcome\":\"parity\",\"matched\":1,\"ours_only\":0,\"tv_only\":0,\
             \"acceptance\":{\"tier\":\"accepted\"}}\n",
            &[],
        );
        assert!(db.compare_runs(a, b).unwrap().probes[0].moved());
    }

    #[test]
    fn an_unknown_run_is_an_error() {
        let db = CorpusDb::open_in_memory().unwrap();
        let a = record(&db, &["p"], &line("p", "near", 1, 0, 0), &[]);
        assert!(db.compare_runs(a, a + 7).is_err());
    }
}
