//! Verify: extract - pbfhogg extract vs osmium extract for each strategy.
//!
//! What a PASS establishes, per strategy: pbfhogg's output is in strict
//! standard order, and every element pbfhogg and osmium both emit has equal
//! content (coordinates, tags, refs, members - pbfhogg diff's content
//! equality, which leaves metadata out). Neither tool rewrites an element it
//! extracts (osmium never clips ways or drops relation members, and simple's
//! dangling refs are absent elements, not edited ones), so a content mismatch
//! on a shared ID is a real disagreement and fails the check.
//!
//! Which elements each tool includes is NOT adjudicated. pbfhogg's extract
//! contract states its strategies but no per-strategy membership equivalence
//! with osmium, and the two are known to include different relations (and,
//! under smart, different nodes), so the membership difference is reported
//! with counts, samples and the shared coverage, and gates nothing. A
//! selection bug that drops or invents elements therefore passes; so does a
//! corrupted element only pbfhogg emits. Deriving the expected membership
//! from the input would be a third extract implementation with brokkr's
//! reading of osmium as its oracle, so it is not done here.

use std::path::Path;

use super::commands::{ExtractStrategy, PbfhoggCommand};
use super::inspect_report::InspectCounts;
use super::verify::{Findings, VerifyHarness};
use super::verify_reference::ToolRun;
use super::verify_element_diff::{self as element_diff, PerType};
use crate::datasets::Bbox;
use crate::error::DevError;
use crate::osc::OscAction;
use crate::output::{verify_msg, verify_summary};

/// The claim a passing extract check's summary line carries.
const CLAIM: &str = "output order, and element content where pbfhogg and osmium hold the \
                     same ID; which elements each includes is not adjudicated";

/// Cross-validate `pbfhogg extract` against `osmium extract` for
/// simple, complete-ways, and smart strategies.
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    bbox: &Bbox,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let outdir = harness.subdir("extract")?;

    let mut findings = Findings::claiming(CLAIM);

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
        let captured = harness.reference(
            &ToolRun::osmium("extract.1")
                .arg("extract")
                .input(pbf)
                .args(&["-b", bbox.as_str(), "-s", strategy.osmium_name(), "-o"])
                .output(&osmium_out)
                .arg("--overwrite"),
            &outdir.join(format!("osmium-{strategy_name}.stdout")),
        )?;
        harness.check_exit(&captured, "osmium extract")?;

        // --- Sort order (pbfhogg's is a verdict; osmium's is a precondition
        // of comparing by ID) ---
        let label = format!("pbfhogg extract --{strategy_name}");
        let (ours_sorted, ours) = harness.check_sorted_counted(&label, &pbfhogg_out)?;
        findings.record(&format!("{label} output order"), ours_sorted);
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
        let mut report = ExtractDiffReport::default();
        element_diff::scan_osc_diff(
            harness,
            &pbfhogg_out,
            &osmium_out,
            &outdir.join(format!("pbfhogg-vs-osmium-{strategy_name}.osc")),
            &outdir,
            |action, kind, id| report.classify(action).add(kind, id),
        )?;
        let shared = if ours_sorted.is_pass() {
            Some(shared_counts(&ours, &theirs, &report)?)
        } else {
            None
        };
        report.print(strategy_name, shared.as_ref());
        if report.content_mismatch.total() > 0 {
            findings.fail(format!(
                "{label} element content differs from osmium on {} shared IDs",
                report.content_mismatch.total()
            ));
        }
    }

    Ok(findings)
}

/// The extract element diff's classes. pbfhogg is the diff's old side and
/// osmium its new side.
#[derive(Default)]
struct ExtractDiffReport {
    /// `<delete>`: elements only pbfhogg's output holds. Not adjudicated.
    pbfhogg_only: PerType,
    /// `<create>`: elements only osmium's output holds. Not adjudicated.
    osmium_only: PerType,
    /// `<modify>`: an ID both hold with unequal content. A failure.
    content_mismatch: PerType,
}

impl ExtractDiffReport {
    fn classify(&mut self, action: OscAction) -> &mut PerType {
        match action {
            OscAction::Delete => &mut self.pbfhogg_only,
            OscAction::Create => &mut self.osmium_only,
            OscAction::Modify => &mut self.content_mismatch,
        }
    }

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
            verify_summary(&format!(
                "extract --{strategy} vs osmium: {coverage}; same membership"
            ));
            return;
        }
        verify_summary(&format!(
            "extract --{strategy} vs osmium: {coverage}; membership differs (not adjudicated): \
             pbfhogg-only {}; osmium-only {}",
            self.pbfhogg_only.counts_line(),
            self.osmium_only.counts_line()
        ));
        self.pbfhogg_only.print("", "pbfhogg-only", verify_summary);
        self.osmium_only.print("", "osmium-only", verify_summary);
    }
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
    use crate::osc::OscKind;

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
}
