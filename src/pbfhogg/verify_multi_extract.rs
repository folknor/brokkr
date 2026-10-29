//! Verify: multi-extract - compare single-pass multi-extract against
//! N sequential single-region extracts.
//!
//! Generates N non-overlapping longitude strips from the dataset bbox,
//! runs `pbfhogg extract --config <json> --simple` (single-pass), then
//! runs N individual `pbfhogg extract --simple -b=<strip>` calls
//! (sequential reference). Per region, the multi output must be in strict
//! order and element-content identical to its sequential extract
//! (`pbfhogg diff`, which leaves metadata and headers out).
//!
//! Only the simple strategy is checked: it is the one with a single-pass
//! implementation, while complete-ways and smart under `--config` run the
//! per-region path, which would be compared with itself. Nothing here proves
//! the single-pass path ran rather than pbfhogg's per-region fallback. The
//! regions are adjacent strips, so overlapping or identical regions and a
//! deliberately empty region are never constructed (an element can still
//! fall in several strips, and a strip can still come out empty).

use std::path::Path;

use super::commands::{ExtractStrategy, PbfhoggCommand};
use super::verify::{Findings, PbfArgv, VerifyHarness};
use crate::datasets::Bbox;
use crate::error::DevError;
use crate::output::verify_msg;

pub(super) use super::inspect_report::InspectCounts as Counts;

/// Element counts for a PBF file (an output in a check's directory), from a
/// plain `inspect --json` report written beside it
/// ([`super::inspect_report`]): an inverted getid output keeps its input's
/// ordering, so its report's size is not bounded by the output being sorted.
pub(super) fn inspect_counts(harness: &VerifyHarness, pbf: &Path) -> Result<Counts, DevError> {
    Ok(super::inspect_report::inspect(harness, pbf, false)?.counts())
}

/// The claim a passing multi-extract check's summary line carries.
const CLAIM: &str = "per strip, the simple multi-extract output is in strict order and \
                     element-content identical to a sequential pbfhogg extract; osmium is \
                     not consulted";

/// Cross-validate single-pass multi-extract against sequential extracts.
#[allow(clippy::too_many_lines)]
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    bbox: &Bbox,
    regions: usize,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let strips = bbox.strips(regions)?;
    let outdir = harness.subdir("multi-extract")?;

    verify_msg(&format!("=== verify multi-extract ({regions} regions) ==="));

    // --- Single-pass multi-extract via --config ---
    let multi_dir = outdir.join("multi");
    std::fs::create_dir_all(&multi_dir)?;
    let config = super::commands::multi_extract_config(&multi_dir, &strips)?;
    let config_path = super::commands::multi_extract_config_path(&outdir);
    crate::reservations::write(&config_path, config.json.as_bytes())?;

    verify_msg("  running single-pass multi-extract...");
    let mut multi_args = PbfArgv::new("extract")
        .input(pbf)?
        .extract_config(&config_path, config.outputs)?;
    if let Some(flag) = ExtractStrategy::Simple.flag() {
        multi_args = multi_args.arg(flag);
    }
    // The outputs are named in the config; one --overwrite covers them all.
    let captured = harness.run_pbfhogg(multi_args.arg_if(direct_io, "--direct-io"))?;
    harness.check_exit(&captured, "pbfhogg multi-extract")?;

    // --- Sequential single-region extracts ---
    let seq_dir = outdir.join("sequential");
    std::fs::create_dir_all(&seq_dir)?;

    verify_msg("  running sequential extracts...");
    for (i, strip) in strips.iter().enumerate() {
        let seq_out = seq_dir.join(format!("strip-{i}.osm.pbf"));
        let command = PbfhoggCommand::Extract {
            strategy: ExtractStrategy::Simple,
        };
        let seq_args = command
            .extract_args_for(pbf, strip, &seq_out)?
            .arg_if(direct_io, "--direct-io");
        let captured = harness.run_pbfhogg(seq_args)?;
        harness.check_exit(&captured, &format!("pbfhogg extract strip-{i}"))?;
    }

    // --- Compare each strip: order, then element content ---
    //
    // Counts alone let a strip with substituted IDs or changed content pass,
    // so each strip is diffed against its sequential extract; the counts are
    // printed as context. The diff does not see order (two equally
    // misordered outputs can diff clean), so the multi output's order is a
    // verdict of its own.
    verify_msg("  comparing strips...");
    let mut findings = Findings::claiming(CLAIM);

    for i in 0..regions {
        let multi_pbf = multi_dir.join(format!("strip-{i}.osm.pbf"));
        let seq_pbf = seq_dir.join(format!("strip-{i}.osm.pbf"));

        let label = format!("multi-extract strip-{i}");
        let (sorted, counts) = harness.check_sorted_counted(&label, &multi_pbf)?;
        findings.record(&format!("{label} output order"), sorted);
        let same = harness.diff_pbfs(
            &multi_pbf,
            &seq_pbf,
            &super::verify::listing_beside(&multi_pbf),
        )?;
        findings.record(
            &format!("{label} element content differs from the sequential extract"),
            same,
        );
        let outcome = if same.is_pass() { "PASS" } else { "FAIL" };
        verify_msg(&format!(
            "  strip-{i}: same elements as sequential {outcome} ({} nodes, {} ways, {} \
             relations)",
            counts.nodes, counts.ways, counts.relations
        ));
    }

    Ok(findings)
}
