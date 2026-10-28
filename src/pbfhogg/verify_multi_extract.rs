//! Verify: multi-extract - compare single-pass multi-extract against
//! N sequential single-region extracts.
//!
//! Generates N non-overlapping longitude strips from the dataset bbox,
//! runs `pbfhogg extract --config <json> --simple` (single-pass), then
//! runs N individual `pbfhogg extract --simple -b=<strip>` calls
//! (sequential reference). Compares element counts per region.

use std::path::Path;

use super::commands::{ExtractStrategy, PbfhoggCommand};
use super::inspect_report::InspectReport;
use super::verify::{Findings, PbfArgv, VerifyHarness};
use crate::datasets::Bbox;
use crate::error::DevError;
use crate::output::verify_msg;

pub(super) use super::inspect_report::InspectCounts as Counts;

/// Existing call shape retained while the one report parser owns formatting.
fn parse_inspect_counts(stdout: &str) -> Option<Counts> {
    InspectReport::parse(stdout).counts()
}

/// Get element counts for a PBF file via `pbfhogg inspect`.
pub(super) fn inspect_counts(harness: &VerifyHarness, pbf: &Path) -> Result<Counts, DevError> {
    let captured = harness.run_pbfhogg(PbfArgv::new("inspect").input(pbf)?)?;
    harness.check_exit(&captured, "pbfhogg inspect")?;

    let stdout = String::from_utf8_lossy(&captured.stdout);
    parse_inspect_counts(&stdout).ok_or_else(|| {
        DevError::Verify(format!(
            "could not parse element counts from inspect output for {}",
            pbf.display()
        ))
    })
}

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

    // --- Compare element counts ---
    verify_msg("  comparing element counts...");
    let mut findings = Findings::new();

    for i in 0..regions {
        let multi_pbf = multi_dir.join(format!("strip-{i}.osm.pbf"));
        let seq_pbf = seq_dir.join(format!("strip-{i}.osm.pbf"));

        let multi_counts = inspect_counts(harness, &multi_pbf)?;
        let seq_counts = inspect_counts(harness, &seq_pbf)?;

        let nodes_ok = multi_counts.nodes == seq_counts.nodes;
        let ways_ok = multi_counts.ways == seq_counts.ways;
        let rels_ok = multi_counts.relations == seq_counts.relations;

        if nodes_ok && ways_ok && rels_ok {
            verify_msg(&format!(
                "  strip-{i}: PASS ({} nodes, {} ways, {} relations)",
                multi_counts.nodes, multi_counts.ways, multi_counts.relations
            ));
        } else {
            findings.fail(format!(
                "strip-{i} element counts differ from sequential extract"
            ));
            verify_msg(&format!("  strip-{i}: FAIL"));
            if !nodes_ok {
                verify_msg(&format!(
                    "    nodes: multi={}, seq={}",
                    multi_counts.nodes, seq_counts.nodes
                ));
            }
            if !ways_ok {
                verify_msg(&format!(
                    "    ways: multi={}, seq={}",
                    multi_counts.ways, seq_counts.ways
                ));
            }
            if !rels_ok {
                verify_msg(&format!(
                    "    relations: multi={}, seq={}",
                    multi_counts.relations, seq_counts.relations
                ));
            }
        }
    }

    if findings.is_clean() {
        verify_msg("  multi-extract: PASS (all regions match)");
    }
    Ok(findings)
}
