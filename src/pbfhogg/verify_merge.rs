//! Verify: apply-changes - 4-tool comparison: pbfhogg, osmium, osmosis, osmconvert.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use super::verify::{self, VerifyHarness};
use crate::error::DevError;
use crate::osc::{self, OscDiff};
use crate::output;
use crate::output::verify_msg;
use crate::tools::OsmosisTools;

/// Cap on the number of failing IDs printed per category (nodes /
/// ways / relations) per failure class so a runaway mismatch doesn't
/// bury the verify summary. The full counts are still reported.
const MAX_REPORTED_IDS: usize = 10;

/// Cross-validate `pbfhogg apply-changes` against osmium, osmosis, and osmconvert.
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    osc: &Path,
    osmosis: Option<&OsmosisTools>,
    direct_io: bool,
) -> Result<(), DevError> {
    let outdir = harness.subdir("merge")?;

    verify_msg("=== verify apply-changes ===");
    verify_msg(&format!("  base: {}", pbf.display()));
    verify_msg(&format!("  diff: {}", osc.display()));

    let pbf_str = pbf.display().to_string();
    let osc_str = osc.display().to_string();

    // --- pbfhogg apply-changes ---
    let pbfhogg_out = outdir.join("pbfhogg.osm.pbf");
    let pbfhogg_out_str = pbfhogg_out.display().to_string();

    verify_msg("--- pbfhogg apply-changes ---");
    let mut pbfhogg_args = vec!["apply-changes", &pbf_str, &osc_str, "-o", &pbfhogg_out_str];
    if direct_io {
        pbfhogg_args.push("--direct-io");
    }
    let captured = harness.run_pbfhogg(&pbfhogg_args)?;
    harness.check_exit(&captured, "pbfhogg apply-changes")?;

    // --- osmium apply-changes ---
    let osmium_out = outdir.join("osmium.osm.pbf");
    let osmium_out_str = osmium_out.display().to_string();

    verify_msg("--- osmium apply-changes ---");
    let captured = harness.run_tool(
        "osmium",
        &[
            "apply-changes",
            &pbf_str,
            &osc_str,
            "-o",
            &osmium_out_str,
            "--overwrite",
        ],
    )?;
    harness.check_exit(&captured, "osmium apply-changes")?;

    // --- osmosis (optional) ---
    let osmosis_out = outdir.join("osmosis.osm.pbf");
    if let Some(tools) = osmosis {
        verify_msg("--- osmosis --apply-change ---");
        let osmosis_out_str = osmosis_out.display().to_string();
        match run_osmosis(
            &tools.osmosis,
            &tools.java_home,
            &[
                "--read-xml-change",
                &format!("file={osc_str}"),
                "--read-pbf",
                &format!("file={pbf_str}"),
                "--apply-change",
                "--write-pbf",
                &format!("file={osmosis_out_str}"),
            ],
            &harness.project_root,
        ) {
            Ok(captured) => {
                if let Err(e) = harness.check_exit(&captured, "osmosis") {
                    verify_msg(&format!("  osmosis failed: {e}"));
                }
            }
            Err(e) => {
                verify_msg(&format!("  osmosis skipped: {e}"));
            }
        }
    }

    // --- osmconvert (optional) ---
    let osmconvert_out = outdir.join("osmconvert.osm.pbf");
    if verify::which_exists("osmconvert") {
        verify_msg("--- osmconvert ---");
        let osmconvert_out_str = osmconvert_out.display().to_string();
        let out_arg = format!("-o={osmconvert_out_str}");
        match harness.run_tool("osmconvert", &[&pbf_str, &osc_str, &out_arg]) {
            Ok(captured) => {
                if let Err(e) = harness.check_exit(&captured, "osmconvert") {
                    verify_msg(&format!("  osmconvert failed: {e}"));
                }
            }
            Err(e) => {
                verify_msg(&format!("  osmconvert skipped: {e}"));
            }
        }
    }

    // --- Element counts ---
    verify_msg("=== element counts ===");
    harness.print_inspect("pbfhogg", &pbfhogg_out)?;
    harness.print_inspect("osmium", &osmium_out)?;
    if osmosis_out.exists() {
        harness.print_inspect("osmosis", &osmosis_out)?;
    }
    if osmconvert_out.exists() {
        harness.print_inspect("osmconvert", &osmconvert_out)?;
    }

    // --- Sort check ---
    harness.check_sorted("pbfhogg apply-changes", &pbfhogg_out)?;

    // --- Strict pbfhogg-vs-osmium element diff (with delete-set tolerance) ---
    //
    // Cross-validation needs more than equal element counts. We diff
    // the two outputs element-by-element and exempt the one
    // legitimate semantic difference: osmium does version-based
    // deletes while pbfhogg/osmosis/osmconvert delete unconditionally,
    // so osmium-only elements whose IDs appear in the input OSC's
    // `<delete>` section aren't bugs - both behaviours are spec-allowed.
    // Anything else (osmium-only IDs not in the delete set, pbfhogg-only
    // elements, or content-level `<modify>` differences) fails verify.
    verify_pbfhogg_vs_osmium(harness, osc, &pbfhogg_out, &osmium_out, &outdir)?;

    Ok(())
}

/// Run `pbfhogg diff --format osc` on `(pbfhogg_out, osmium_out)`,
/// then categorise each diff entry against the input OSC's delete
/// set. Returns `Err(DevError::Verify(...))` if any unexplained
/// difference remains.
fn verify_pbfhogg_vs_osmium(
    harness: &VerifyHarness,
    input_osc: &Path,
    pbfhogg_out: &Path,
    osmium_out: &Path,
    outdir: &Path,
) -> Result<(), DevError> {
    verify_msg("=== pbfhogg vs osmium element diff ===");

    let applied = osc::parse_osc_file(input_osc)?;
    verify_msg(&format!(
        "  input OSC delete set: {} nodes, {} ways, {} relations",
        applied.deleted_nodes.len(),
        applied.deleted_ways.len(),
        applied.deleted_relations.len(),
    ));

    let diff_path = outdir.join("pbfhogg-vs-osmium.osc");
    let diff_path_str = diff_path.display().to_string();
    let pbfhogg_str = pbfhogg_out.display().to_string();
    let osmium_str = osmium_out.display().to_string();

    // pbfhogg diff old new: <delete> blocks = old-only, <create>
    // blocks = new-only, <modify> blocks = same id different content.
    // We pass pbfhogg as `old` and osmium as `new` so:
    //   diff.created_* = osmium-only (carve-out candidates)
    //   diff.deleted_* = pbfhogg-only (always failures)
    //   diff.modified_* = content mismatches (always failures)
    let captured = harness.run_pbfhogg(&[
        "diff",
        "--format",
        "osc",
        "--suppress-common",
        "-o",
        &diff_path_str,
        &pbfhogg_str,
        &osmium_str,
    ])?;

    // pbfhogg diff exits non-zero when differences are found - that's
    // expected. A signal kill or argument error would also surface as
    // non-success, so distinguish: trust the exit code, but only fail
    // hard if the output OSC is missing.
    if !diff_path.exists() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Verify(format!(
            "pbfhogg diff produced no output OSC at {} - stderr: {}",
            diff_path.display(),
            stderr.trim()
        )));
    }

    let delta = osc::parse_osc_file(&diff_path)?;
    if delta.is_empty() {
        verify_msg("  pbfhogg and osmium outputs are element-identical PASS");
        return Ok(());
    }

    let mut report = MergeDiffReport::default();
    classify_diff(&delta, &applied, &mut report);
    report.print();

    if report.is_failure() {
        return Err(DevError::Verify(format!(
            "pbfhogg vs osmium: {} unexplained differences \
             (see element diff above)",
            report.total_failures()
        )));
    }
    Ok(())
}

/// Outcome buckets for the per-element diff. Sorted IDs (BTreeSet
/// from `OscDiff`) keep the printed report deterministic. Capped at
/// `MAX_REPORTED_IDS` per category so a runaway 10k-mismatch doesn't
/// bury the summary.
#[derive(Default)]
struct MergeDiffReport {
    /// osmium-only IDs accounted for by the input OSC's delete set.
    /// These are *expected* differences - osmium's version-based
    /// deletes vs pbfhogg's unconditional deletes.
    osmium_only_explained: PerType,
    /// osmium-only IDs not in the input OSC delete set. These are
    /// pbfhogg dropping elements it shouldn't have.
    osmium_only_unexplained: PerType,
    /// pbfhogg-only IDs (osmium dropped, pbfhogg kept). Always a
    /// failure - means osmium did something pbfhogg didn't.
    pbfhogg_only: PerType,
    /// IDs present in both outputs but with different content.
    content_mismatch: PerType,
}

#[derive(Default)]
struct PerType {
    nodes: Vec<i64>,
    ways: Vec<i64>,
    relations: Vec<i64>,
}

impl PerType {
    fn total(&self) -> usize {
        self.nodes.len() + self.ways.len() + self.relations.len()
    }
}

impl MergeDiffReport {
    fn total_failures(&self) -> usize {
        self.osmium_only_unexplained.total()
            + self.pbfhogg_only.total()
            + self.content_mismatch.total()
    }

    fn is_failure(&self) -> bool {
        self.total_failures() > 0
    }

    fn print(&self) {
        if self.osmium_only_explained.total() > 0 {
            verify_msg(&format!(
                "  osmium-only explained by delete set: {} nodes, {} ways, {} relations OK",
                self.osmium_only_explained.nodes.len(),
                self.osmium_only_explained.ways.len(),
                self.osmium_only_explained.relations.len(),
            ));
        }
        print_failure_class(
            "osmium-only NOT in delete set",
            &self.osmium_only_unexplained,
        );
        print_failure_class("pbfhogg-only (osmium dropped)", &self.pbfhogg_only);
        print_failure_class("content differs", &self.content_mismatch);
    }
}

fn print_failure_class(label: &str, p: &PerType) {
    if p.total() == 0 {
        return;
    }
    verify_msg(&format!(
        "  FAIL {label}: {} nodes, {} ways, {} relations",
        p.nodes.len(),
        p.ways.len(),
        p.relations.len(),
    ));
    print_id_sample("nodes", &p.nodes);
    print_id_sample("ways", &p.ways);
    print_id_sample("relations", &p.relations);
}

fn print_id_sample(kind: &str, ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    let shown = ids.iter().take(MAX_REPORTED_IDS);
    let suffix = if ids.len() > MAX_REPORTED_IDS {
        format!(" (+{} more)", ids.len() - MAX_REPORTED_IDS)
    } else {
        String::new()
    };
    let list = shown
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    verify_msg(&format!("    {kind}: {list}{suffix}"));
}

/// Walk every section of `delta` and bucket each ID into the
/// appropriate report class, using `applied`'s delete set for
/// the carve-out.
fn classify_diff(delta: &OscDiff, applied: &OscDiff, report: &mut MergeDiffReport) {
    classify_osmium_only(
        &delta.created_nodes,
        &applied.deleted_nodes,
        &mut report.osmium_only_explained.nodes,
        &mut report.osmium_only_unexplained.nodes,
    );
    classify_osmium_only(
        &delta.created_ways,
        &applied.deleted_ways,
        &mut report.osmium_only_explained.ways,
        &mut report.osmium_only_unexplained.ways,
    );
    classify_osmium_only(
        &delta.created_relations,
        &applied.deleted_relations,
        &mut report.osmium_only_explained.relations,
        &mut report.osmium_only_unexplained.relations,
    );

    report.pbfhogg_only.nodes.extend(delta.deleted_nodes.iter().copied());
    report.pbfhogg_only.ways.extend(delta.deleted_ways.iter().copied());
    report
        .pbfhogg_only
        .relations
        .extend(delta.deleted_relations.iter().copied());

    report
        .content_mismatch
        .nodes
        .extend(delta.modified_nodes.iter().copied());
    report
        .content_mismatch
        .ways
        .extend(delta.modified_ways.iter().copied());
    report
        .content_mismatch
        .relations
        .extend(delta.modified_relations.iter().copied());
}

fn classify_osmium_only(
    osmium_only: &std::collections::BTreeSet<i64>,
    applied_deletes: &std::collections::BTreeSet<i64>,
    explained: &mut Vec<i64>,
    unexplained: &mut Vec<i64>,
) {
    for &id in osmium_only {
        if applied_deletes.contains(&id) {
            explained.push(id);
        } else {
            unexplained.push(id);
        }
    }
}

/// Run osmosis with `JAVA_HOME` set, returning a `CapturedOutput`.
fn run_osmosis(
    osmosis: &Path,
    java_home: &Path,
    args: &[&str],
    cwd: &Path,
) -> Result<output::CapturedOutput, DevError> {
    let start = Instant::now();
    let result = Command::new(osmosis.display().to_string())
        .args(args)
        .env("JAVA_HOME", java_home.display().to_string())
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| DevError::Subprocess {
            program: "osmosis".into(),
            code: None,
            stderr: e.to_string(),
        })?;

    Ok(output::CapturedOutput {
        status: result.status,
        stdout: result.stdout,
        stderr: result.stderr,
        elapsed: start.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::collections::BTreeSet;

    fn s(ids: &[i64]) -> BTreeSet<i64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn classify_partitions_osmium_only_by_delete_set() {
        let mut explained = Vec::new();
        let mut unexplained = Vec::new();
        let osmium_only = s(&[1, 2, 3, 4]);
        let applied_deletes = s(&[1, 3]);
        classify_osmium_only(&osmium_only, &applied_deletes, &mut explained, &mut unexplained);
        assert_eq!(explained, vec![1, 3]);
        assert_eq!(unexplained, vec![2, 4]);
    }

    #[test]
    fn classify_diff_buckets_each_section() {
        let delta = OscDiff {
            created_nodes: s(&[10, 20]),
            created_ways: s(&[30]),
            modified_nodes: s(&[40]),
            deleted_ways: s(&[50]),
            ..OscDiff::default()
        };
        let applied = OscDiff {
            deleted_nodes: s(&[10]),
            ..OscDiff::default()
        };
        let mut report = MergeDiffReport::default();
        classify_diff(&delta, &applied, &mut report);

        assert_eq!(report.osmium_only_explained.nodes, vec![10]);
        assert_eq!(report.osmium_only_unexplained.nodes, vec![20]);
        assert_eq!(report.osmium_only_unexplained.ways, vec![30]);
        assert_eq!(report.content_mismatch.nodes, vec![40]);
        assert_eq!(report.pbfhogg_only.ways, vec![50]);
        assert!(report.is_failure());
        assert_eq!(report.total_failures(), 4); // 20, 30, 40, 50
    }

    #[test]
    fn classify_diff_clean_when_all_explained() {
        let delta = OscDiff {
            created_nodes: s(&[1, 2, 3]),
            created_ways: s(&[10]),
            created_relations: s(&[100]),
            ..OscDiff::default()
        };
        let applied = OscDiff {
            deleted_nodes: s(&[1, 2, 3]),
            deleted_ways: s(&[10]),
            deleted_relations: s(&[100]),
            ..OscDiff::default()
        };
        let mut report = MergeDiffReport::default();
        classify_diff(&delta, &applied, &mut report);

        assert!(!report.is_failure());
        assert_eq!(report.osmium_only_explained.nodes, vec![1, 2, 3]);
        assert_eq!(report.osmium_only_explained.ways, vec![10]);
        assert_eq!(report.osmium_only_explained.relations, vec![100]);
    }

    #[test]
    fn classify_diff_pbfhogg_only_always_fails() {
        // pbfhogg-only entries are always failures, even when the
        // input OSC requested those deletes (because in that case,
        // osmium also deleted them and pbfhogg shouldn't be holding
        // on to them either).
        let delta = OscDiff {
            deleted_nodes: s(&[7, 8]),
            ..OscDiff::default()
        };
        let applied = OscDiff {
            deleted_nodes: s(&[7, 8]),
            ..OscDiff::default()
        };
        let mut report = MergeDiffReport::default();
        classify_diff(&delta, &applied, &mut report);

        assert!(report.is_failure());
        assert_eq!(report.pbfhogg_only.nodes, vec![7, 8]);
    }

    #[test]
    fn classify_diff_modify_always_fails() {
        let delta = OscDiff {
            modified_nodes: s(&[42]),
            ..OscDiff::default()
        };
        let applied = OscDiff::default();
        let mut report = MergeDiffReport::default();
        classify_diff(&delta, &applied, &mut report);

        assert!(report.is_failure());
        assert_eq!(report.content_mismatch.nodes, vec![42]);
    }
}
