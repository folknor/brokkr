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
use super::verify::{Findings, VerifyHarness};
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
    let pbf_str = pbf.display().to_string();
    let captured = harness.run_pbfhogg(&["inspect", &pbf_str])?;
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
    let pbf_str = pbf.display().to_string();

    verify_msg(&format!("=== verify multi-extract ({regions} regions) ==="));

    // --- Single-pass multi-extract via --config ---
    let multi_dir = outdir.join("multi");
    std::fs::create_dir_all(&multi_dir)?;
    let config_json = super::commands::multi_extract_config(&multi_dir, &strips)?;
    let config_path = super::commands::multi_extract_config_path(&outdir);
    std::fs::write(&config_path, config_json)?;

    verify_msg("  running single-pass multi-extract...");
    let config_path_str = config_path.display().to_string();
    let mut multi_args = vec![
        "extract".to_owned(),
        pbf_str.clone(),
        "--config".into(),
        config_path_str,
    ];
    if let Some(flag) = ExtractStrategy::Simple.flag() {
        multi_args.push(flag.into());
    }
    if direct_io {
        multi_args.push("--direct-io".into());
    }
    let multi_arg_refs: Vec<&str> = multi_args.iter().map(String::as_str).collect();
    let captured = harness.run_pbfhogg(&multi_arg_refs)?;
    harness.check_exit(&captured, "pbfhogg multi-extract")?;

    // --- Sequential single-region extracts ---
    let seq_dir = outdir.join("sequential");
    std::fs::create_dir_all(&seq_dir)?;

    verify_msg("  running sequential extracts...");
    for (i, strip) in strips.iter().enumerate() {
        let seq_out = seq_dir.join(format!("strip-{i}.osm.pbf"));
        let seq_out_str = seq_out.display().to_string();
        let command = PbfhoggCommand::Extract {
            strategy: ExtractStrategy::Simple,
        };
        let mut seq_args = command.extract_args_for(&pbf_str, strip, &seq_out_str)?;
        if direct_io {
            seq_args.push("--direct-io".into());
        }
        let seq_arg_refs: Vec<&str> = seq_args.iter().map(String::as_str).collect();
        let captured = harness.run_pbfhogg(&seq_arg_refs)?;
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
