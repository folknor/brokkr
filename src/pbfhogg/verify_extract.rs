//! Verify: extract - pbfhogg extract vs osmium extract for each strategy.
//!
//! What a PASS establishes, per strategy: pbfhogg's output order (below),
//! the element content of everything it writes, and which elements it writes,
//! all against osmium. Content is pbfhogg diff's content equality
//! (coordinates, tags, refs, members; metadata left out). Neither tool
//! rewrites an element it extracts (osmium never clips ways or drops relation
//! members, and simple's dangling refs are absent elements, not edited ones),
//! so a content mismatch on a shared ID is a real disagreement.
//!
//! Membership is pbfhogg's stated contract against osmium (pbfhogg's
//! DEVIATIONS.md, "extract: per-strategy membership against osmium"):
//! complete_ways writes osmium's element set, smart writes the set of
//! `osmium extract -s smart -S types=multipolygon,boundary`, so for both any
//! element only one tool holds fails. Simple writes osmium's nodes and ways,
//! so an element of either kind only one tool holds fails, and osmium-only
//! relations fail; its relations differ by design, since osmium's simple
//! decides a relation on its first node or way member and pbfhogg's on any.
//! [`super::verify_extract_simple`] judges them against osmium's
//! complete_ways output, which also checks the content of the relations
//! osmium's simple omits.
//!
//! Input order. pbfhogg branches on `Sort.Type_then_ID` among the input
//! header's optional features (not the required ones), and so does this
//! check. With it, pbfhogg's output must be in strict
//! standard order. Without it (an unsorted snapshot), osmium, which refuses
//! out-of-order input, extracts from an osmium-sorted copy of the input
//! while pbfhogg extracts the input as it is, through its unsorted
//! fallbacks; pbfhogg's output must then not claim `Sort.Type_then_ID` (its
//! header contract: an extract keeps its input's order, claimed only where
//! the input claims it), and it is compared through an osmium-sorted copy,
//! whose strict order proves it repeats no ID. Which order pbfhogg keeps an
//! unsorted input's elements in is not promised and not checked.

use std::path::{Path, PathBuf};

use super::commands::{ExtractStrategy, PbfhoggCommand};
use super::inspect_report::{self, InspectCounts};
use super::verify::{Findings, SORT_TYPE_THEN_ID, Verdict, VerifyHarness};
use super::verify_element_diff::{self as element_diff, PerType};
use super::verify_extract_simple::{self as simple_relations, SimpleRelations};
use super::verify_reference::ToolRun;
use super::verify_sort;
use crate::datasets::Bbox;
use crate::error::DevError;
use crate::osc::{OscAction, OscKind};
use crate::output::{verify_msg, verify_summary};

/// The claim a passing extract check's summary line carries on an input
/// whose header declares `Sort.Type_then_ID`.
const CLAIM: &str = "output order, element content and membership against osmium: \
                     complete_ways and smart (-S types=multipolygon,boundary) as osmium's, \
                     simple as osmium's but for the relations osmium's first-member rule omits";

/// The claim on an input whose header does not declare it.
const CLAIM_UNSORTED: &str = "on unsorted input, against osmium on an osmium-sorted copy: no \
                              sorted claim and no repeated ID in pbfhogg's output, element \
                              content and membership: complete_ways and smart (-S \
                              types=multipolygon,boundary) as osmium's, simple as osmium's but \
                              for the relations osmium's first-member rule omits";

/// The relation types pbfhogg's smart completes, as osmium's `-S types=`
/// names them (pbfhogg's smart has no `-S` of its own).
const SMART_TYPES: &str = "types=multipolygon,boundary";

/// osmium's arguments selecting the strategy pbfhogg's `strategy` equals.
fn osmium_strategy(strategy: ExtractStrategy) -> Vec<&'static str> {
    let mut args = vec!["-s", strategy.osmium_name()];
    if strategy == ExtractStrategy::Smart {
        args.extend(["-S", SMART_TYPES]);
    }
    args
}

/// Cross-validate `pbfhogg extract` against `osmium extract` for
/// simple, complete-ways, and smart strategies.
#[allow(clippy::too_many_lines)]
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    bbox: &Bbox,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let outdir = harness.subdir("extract")?;

    // --- Input order: the header claim pbfhogg branches on ---
    // The report goes to the check's directory, never beside the input.
    let input_sorted =
        inspect_report::inspect_into(harness, pbf, &outdir.join("input.inspect.json"), false)?
            .optional_sorted();
    let osmium_input = if input_sorted {
        pbf.to_path_buf()
    } else {
        verify_msg(&format!(
            "  input header does not declare {SORT_TYPE_THEN_ID} as an optional feature: osmium \
             extracts an \
             osmium-sorted copy, pbfhogg the input as it is"
        ));
        // The sort's artifact name and argv are verify sort's, so one cached
        // run of the same input serves both checks.
        let dir = outdir.join("osmium-sorted-input");
        std::fs::create_dir_all(&dir)?;
        let sorted = dir.join(verify_sort::OSMIUM_SORTED);
        verify_sort::osmium_sort(harness, pbf, &sorted, &dir.join("osmium.stdout"))?;
        sorted
    };

    let mut findings = Findings::claiming(if input_sorted { CLAIM } else { CLAIM_UNSORTED });
    let mut membership = Vec::new();
    let mut simple: Option<SimpleArm> = None;
    let mut complete: Option<(PathBuf, u64)> = None;

    for strategy in ExtractStrategy::all() {
        let strategy_name = strategy.name();
        verify_msg(&format!("=== verify extract --{strategy_name} ==="));

        // --- pbfhogg extract ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{strategy_name}.osm.pbf"));
        let command = PbfhoggCommand::Extract {
            strategy: *strategy,
        };
        let pbfhogg_args = command
            .extract_args_for(pbf, bbox, &pbfhogg_out)?
            .arg_if(direct_io, "--direct-io");
        let captured = harness.run_pbfhogg(pbfhogg_args)?;
        harness.check_exit(&captured, "pbfhogg extract")?;

        // --- osmium extract ---
        let osmium_out = outdir.join(format!("osmium-{strategy_name}.osm.pbf"));
        let mut osmium_args = vec!["-b", bbox.as_str()];
        osmium_args.extend(osmium_strategy(*strategy));
        osmium_args.push("-o");
        let captured = harness.reference(
            &ToolRun::osmium("extract.1")
                .arg("extract")
                .input(&osmium_input)
                .args(&osmium_args)
                .output(&osmium_out)
                .arg("--overwrite"),
            &outdir.join(format!("osmium-{strategy_name}.stdout")),
        )?;
        harness.check_exit(&captured, "osmium extract")?;

        // --- pbfhogg's order, and the file it is compared through ---
        let label = format!("pbfhogg extract --{strategy_name}");
        let (compared, order_failure) = if input_sorted {
            (pbfhogg_out.clone(), format!("{label} output order"))
        } else {
            let claims = harness.has_sort_flag(&pbfhogg_out)?;
            let verdict = Verdict::from_pass(!claims);
            verify_msg(&format!(
                "  {label}: header {} {SORT_TYPE_THEN_ID} on unsorted input {}",
                if claims { "claims" } else { "does not claim" },
                if claims { "FAIL" } else { "PASS" }
            ));
            findings.record(
                &format!("{label} output claims {SORT_TYPE_THEN_ID} on unsorted input"),
                verdict,
            );
            let sorted = outdir.join(format!("pbfhogg-{strategy_name}.sorted.osm.pbf"));
            verify_sort::osmium_sort(
                harness,
                &pbfhogg_out,
                &sorted,
                &outdir.join(format!("pbfhogg-{strategy_name}.sorted.stdout")),
            )?;
            (sorted, format!("{label} output repeats an ID"))
        };
        let (ours_sorted, ours) = harness.check_sorted_counted(&label, &compared)?;
        findings.record(&order_failure, ours_sorted);
        let (theirs_sorted, theirs) = harness
            .check_sorted_counted(&format!("osmium extract --{strategy_name}"), &osmium_out)?;
        // A reference output with repeated or out-of-order IDs cannot be
        // compared by key: the diff could pair one copy and list the other
        // as osmium-only, hiding a conflicting content. That is a comparison
        // that could not be made, never a pass.
        if !theirs_sorted.is_pass() {
            return Err(DevError::Verify(format!(
                "osmium extract --{strategy_name} output is not in strict order, so it \
                 cannot be compared with pbfhogg's by ID"
            )));
        }

        // --- Element diff, classified as it streams ---
        let keep_relations = *strategy == ExtractStrategy::Simple;
        let mut report = ExtractDiffReport::default();
        element_diff::scan_osc_diff(
            harness,
            &compared,
            &osmium_out,
            &outdir.join(format!("pbfhogg-vs-osmium-{strategy_name}.osc")),
            &outdir,
            |action, kind, id| {
                if keep_relations && action == OscAction::Delete && kind == OscKind::Relation {
                    report.pbfhogg_only_relations.push(id);
                }
                report.classify(action).add(kind, id);
            },
        )?;
        let shared = if ours_sorted.is_pass() {
            Some(shared_counts(&ours, &theirs, &report)?)
        } else {
            None
        };
        report.print(strategy_name, shared.as_ref());
        membership.extend(report.membership_clause(strategy_name));
        if report.content_mismatch.total() > 0 {
            findings.fail(format!(
                "{label} element content differs from osmium on {} shared IDs",
                report.content_mismatch.total()
            ));
        }
        for failure in report.membership_failures(*strategy) {
            findings.fail(format!("{label} {failure}"));
        }

        match strategy {
            ExtractStrategy::Simple => {
                let mut pbfhogg_only = std::mem::take(&mut report.pbfhogg_only_relations);
                pbfhogg_only.sort_unstable();
                if pbfhogg_only.windows(2).any(|w| w[0] == w[1]) {
                    return Err(DevError::Verify(
                        "the simple element diff deleted one relation twice".into(),
                    ));
                }
                simple = Some(SimpleArm {
                    compared,
                    osmium_out,
                    pbfhogg_only,
                    nodes_and_ways_agree: report.nodes_and_ways_agree(),
                });
            }
            ExtractStrategy::Complete => complete = Some((osmium_out, theirs.relations)),
            ExtractStrategy::Smart => {}
        }
    }

    match (simple, complete) {
        (Some(simple), Some((osmium_complete, complete_relations))) => {
            if simple.nodes_and_ways_agree {
                simple_relations::adjudicate(
                    harness,
                    &SimpleRelations {
                        outdir: &outdir,
                        pbfhogg_simple: &simple.compared,
                        osmium_simple: &simple.osmium_out,
                        osmium_complete: &osmium_complete,
                        complete_relations,
                        pbfhogg_only: &simple.pbfhogg_only,
                    },
                    &mut findings,
                )?;
            } else {
                // The relation rule reads simple's node and way sets through
                // osmium's output, which is sound only where they agree;
                // that they do not is already a failure.
                verify_msg(
                    "  simple relations not judged: simple's node and way membership differs \
                     from osmium's",
                );
            }
        }
        _ => {
            return Err(DevError::Internal(
                "verify extract ran without its simple or complete_ways arm".into(),
            ));
        }
    }

    if let Some(line) = membership_line(&membership, &outdir) {
        verify_summary(&line);
    }
    Ok(findings)
}

/// What the simple strategy hands its relation adjudication.
struct SimpleArm {
    /// pbfhogg's simple output as compared (an osmium-sorted copy on
    /// unsorted input).
    compared: PathBuf,
    osmium_out: PathBuf,
    /// Every relation only pbfhogg's simple output holds, sorted, distinct.
    pbfhogg_only: Vec<i64>,
    nodes_and_ways_agree: bool,
}

/// The extract element diff's classes. pbfhogg is the diff's old side and
/// osmium its new side.
#[derive(Default)]
struct ExtractDiffReport {
    /// `<delete>`: elements only pbfhogg's output holds.
    pbfhogg_only: PerType,
    /// `<create>`: elements only osmium's output holds.
    osmium_only: PerType,
    /// `<modify>`: an ID both hold with unequal content. A failure.
    content_mismatch: PerType,
    /// Every ID of `pbfhogg_only`'s relations, in full, kept for simple's
    /// relation adjudication only.
    pbfhogg_only_relations: Vec<i64>,
}

impl ExtractDiffReport {
    fn classify(&mut self, action: OscAction) -> &mut PerType {
        match action {
            OscAction::Delete => &mut self.pbfhogg_only,
            OscAction::Create => &mut self.osmium_only,
            OscAction::Modify => &mut self.content_mismatch,
        }
    }

    /// Whether neither side holds a node or way the other lacks.
    fn nodes_and_ways_agree(&self) -> bool {
        [&self.pbfhogg_only, &self.osmium_only]
            .iter()
            .all(|class| class.kinds[0].count == 0 && class.kinds[1].count == 0)
    }

    /// The membership differences `strategy`'s contract does not allow.
    /// complete_ways and smart allow none. simple allows pbfhogg-only
    /// relations, which [`super::verify_extract_simple`] judges.
    fn membership_failures(&self, strategy: ExtractStrategy) -> Vec<String> {
        let mut failures = Vec::new();
        let (pbfhogg_only, osmium_only) = match strategy {
            ExtractStrategy::Simple => (
                self.pbfhogg_only.kinds[0].count + self.pbfhogg_only.kinds[1].count,
                self.osmium_only.total(),
            ),
            ExtractStrategy::Complete | ExtractStrategy::Smart => {
                (self.pbfhogg_only.total(), self.osmium_only.total())
            }
        };
        let what = if strategy == ExtractStrategy::Simple {
            "nodes or ways"
        } else {
            "elements"
        };
        if pbfhogg_only > 0 {
            failures.push(format!("holds {pbfhogg_only} {what} osmium's output omits"));
        }
        if osmium_only > 0 {
            failures.push(format!(
                "omits {osmium_only} elements osmium's output holds"
            ));
        }
        failures
    }

    /// The strategy's full account, into the check's detail.
    fn print(&self, strategy: &str, shared: Option<&[u64; 3]>) {
        let coverage = match shared {
            Some([n, w, r]) => format!("shared {n} nodes, {w} ways, {r} relations"),
            None => "shared coverage unavailable (pbfhogg's output is not in strict order)".into(),
        };
        if self.content_mismatch.total() > 0 {
            self.content_mismatch
                .print("FAIL ", "content differs on a shared ID", verify_msg);
        }
        if self.pbfhogg_only.total() + self.osmium_only.total() == 0 {
            verify_msg(&format!(
                "  extract --{strategy} vs osmium: {coverage}; same membership"
            ));
            return;
        }
        verify_msg(&format!(
            "  extract --{strategy} vs osmium: {coverage}; membership differs"
        ));
        self.pbfhogg_only.print("", "pbfhogg-only", verify_msg);
        self.osmium_only.print("", "osmium-only", verify_msg);
    }

    /// The strategy's membership difference as one clause of the summary
    /// line, `None` when both hold the same elements.
    fn membership_clause(&self, strategy: &str) -> Option<String> {
        let sides: Vec<String> = [
            ("pbfhogg-only", &self.pbfhogg_only),
            ("osmium-only", &self.osmium_only),
        ]
        .into_iter()
        .filter(|(_, class)| class.total() > 0)
        .map(|(label, class)| format!("{label} {}", nonzero_counts(class)))
        .collect();
        (!sides.is_empty()).then(|| format!("{strategy} {}", sides.join(", ")))
    }
}

/// `56307 nodes 394 ways`: the kinds a class holds, zeros left out.
fn nonzero_counts(class: &PerType) -> String {
    ["nodes", "ways", "relations"]
        .iter()
        .zip(&class.kinds)
        .filter(|(_, sampled)| sampled.count > 0)
        .map(|(kind, sampled)| format!("{} {kind}", sampled.count))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The one summary line for every strategy's membership difference, which
/// prints on a pass too, so a change in it is visible run to run; `None`
/// when no strategy differs. On a pass the only difference is the relations
/// only pbfhogg's simple holds, each judged against the rule. The element
/// lists stay in each strategy's diff.
fn membership_line(clauses: &[String], outdir: &Path) -> Option<String> {
    (!clauses.is_empty()).then(|| {
        format!(
            "  extract vs osmium, membership differences: {}; element lists in {}",
            clauses.join("; "),
            crate::output::path(&outdir.join("pbfhogg-vs-osmium-<strategy>.osc"))
        )
    })
}

/// Elements per kind both outputs hold: pbfhogg's count less its only-side,
/// which must equal osmium's count less its only-side. Computed only over
/// outputs whose order check passed, so every count is of distinct IDs; an
/// accounting that does not close is an error, never a verdict.
fn shared_counts(
    ours: &InspectCounts,
    theirs: &InspectCounts,
    report: &ExtractDiffReport,
) -> Result<[u64; 3], DevError> {
    let ours = [ours.nodes, ours.ways, ours.relations];
    let theirs = [theirs.nodes, theirs.ways, theirs.relations];
    let mut shared = [0; 3];
    for kind in 0..3 {
        let a = ours[kind].checked_sub(report.pbfhogg_only.kinds[kind].count);
        let b = theirs[kind].checked_sub(report.osmium_only.kinds[kind].count);
        match (a, b) {
            (Some(a), Some(b)) if a == b => shared[kind] = a,
            _ => {
                return Err(DevError::Verify(format!(
                    "extract element accounting does not close for kind {kind}: pbfhogg {} \
                     (only {}), osmium {} (only {})",
                    ours[kind],
                    report.pbfhogg_only.kinds[kind].count,
                    theirs[kind],
                    report.osmium_only.kinds[kind].count
                )));
            }
        }
    }
    Ok(shared)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn counts(nodes: u64, ways: u64, relations: u64) -> InspectCounts {
        InspectCounts {
            nodes,
            ways,
            relations,
        }
    }

    #[test]
    fn membership_and_content_land_in_their_classes() {
        let mut report = ExtractDiffReport::default();
        report.classify(OscAction::Delete).add(OscKind::Relation, 7);
        report.classify(OscAction::Create).add(OscKind::Relation, 8);
        report.classify(OscAction::Modify).add(OscKind::Way, 9);
        assert_eq!(report.pbfhogg_only.kinds[2].count, 1);
        assert_eq!(report.osmium_only.kinds[2].count, 1);
        assert_eq!(report.content_mismatch.kinds[1].count, 1);
    }

    #[test]
    fn shared_coverage_closes_or_is_an_error() {
        let mut report = ExtractDiffReport::default();
        report.classify(OscAction::Delete).add(OscKind::Node, 1);
        report.classify(OscAction::Create).add(OscKind::Relation, 2);
        let shared = shared_counts(&counts(10, 5, 3), &counts(9, 5, 4), &report).unwrap();
        assert_eq!(shared, [9, 5, 3]);
        assert!(shared_counts(&counts(10, 5, 3), &counts(10, 5, 4), &report).is_err());
        assert!(shared_counts(&counts(0, 5, 3), &counts(0, 5, 4), &report).is_err());
    }

    #[test]
    fn complete_and_smart_allow_no_membership_difference() {
        for strategy in [ExtractStrategy::Complete, ExtractStrategy::Smart] {
            let mut report = ExtractDiffReport::default();
            assert!(report.membership_failures(strategy).is_empty());
            report.classify(OscAction::Delete).add(OscKind::Relation, 7);
            assert_eq!(report.membership_failures(strategy).len(), 1);
            report.classify(OscAction::Create).add(OscKind::Node, 8);
            assert_eq!(report.membership_failures(strategy).len(), 2);
        }
    }

    #[test]
    fn simple_leaves_only_pbfhogg_only_relations_to_its_adjudication() {
        let mut report = ExtractDiffReport::default();
        report.classify(OscAction::Delete).add(OscKind::Relation, 7);
        assert!(
            report
                .membership_failures(ExtractStrategy::Simple)
                .is_empty()
        );
        assert!(report.nodes_and_ways_agree());

        let mut report = ExtractDiffReport::default();
        report.classify(OscAction::Create).add(OscKind::Relation, 7);
        assert_eq!(report.membership_failures(ExtractStrategy::Simple).len(), 1);
        assert!(report.nodes_and_ways_agree());

        for (action, kind) in [
            (OscAction::Delete, OscKind::Node),
            (OscAction::Delete, OscKind::Way),
            (OscAction::Create, OscKind::Way),
        ] {
            let mut report = ExtractDiffReport::default();
            report.classify(action).add(kind, 1);
            assert_eq!(report.membership_failures(ExtractStrategy::Simple).len(), 1);
            assert!(!report.nodes_and_ways_agree());
        }
    }

    #[test]
    fn smart_runs_osmium_with_boundary_completion() {
        assert_eq!(
            osmium_strategy(ExtractStrategy::Smart),
            ["-s", "smart", "-S", "types=multipolygon,boundary"]
        );
        assert_eq!(osmium_strategy(ExtractStrategy::Simple), ["-s", "simple"]);
        assert_eq!(
            osmium_strategy(ExtractStrategy::Complete),
            ["-s", "complete_ways"]
        );
    }
}
