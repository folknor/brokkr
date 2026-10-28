//! Verify: multi-extract - compare single-pass multi-extract against
//! N sequential single-region extracts.
//!
//! Generates N non-overlapping longitude strips from the dataset bbox,
//! runs `pbfhogg extract --config <json> --simple` (single-pass), then
//! runs N individual `pbfhogg extract --simple -b=<strip>` calls
//! (sequential reference). Compares element counts per region.

use std::path::Path;

use super::commands::{ExtractStrategy, PbfhoggCommand};
use super::verify::{Findings, PbfArgv, VerifyHarness};
use crate::datasets::Bbox;
use crate::error::DevError;
use crate::output::verify_msg;

pub(super) use super::inspect_report::InspectCounts as Counts;

/// The part of `pbfhogg inspect --json` this reads. Every other field (the
/// ordering sequence, which grows with the blob count of a noncanonically
/// ordered file, among them) is skipped by the deserializer as it streams
/// past, never retained.
#[derive(serde::Deserialize)]
struct InspectJson {
    elements: InspectJsonElements,
}

#[derive(serde::Deserialize)]
struct InspectJsonElements {
    nodes: u64,
    ways: u64,
    relations: u64,
}

fn parse_inspect_json(reader: impl std::io::Read) -> serde_json::Result<Counts> {
    let json: InspectJson = serde_json::from_reader(reader)?;
    Ok(Counts {
        nodes: json.elements.nodes,
        ways: json.elements.ways,
        relations: json.elements.relations,
    })
}

/// Get element counts for a PBF file (an output in a check's directory) via
/// `pbfhogg inspect --json`, written to `<stem>.inspect.json` beside it and
/// read back through a buffered streaming parser: an inverted getid output
/// keeps its input's ordering, so its report's size is not bounded by the
/// output being sorted.
pub(super) fn inspect_counts(harness: &VerifyHarness, pbf: &Path) -> Result<Counts, DevError> {
    let report = pbf.with_extension("inspect.json");
    let captured =
        harness.run_pbfhogg_to(PbfArgv::new("inspect").arg("--json").input(pbf)?, &report)?;
    harness.check_exit(&captured, "pbfhogg inspect --json")?;
    let file = std::fs::File::open(&report)
        .map_err(|e| DevError::Verify(format!("cannot open {}: {e}", report.display())))?;
    parse_inspect_json(std::io::BufReader::new(file)).map_err(|e| {
        DevError::Verify(format!(
            "could not parse element counts from inspect --json for {}: {e}",
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn inspect_json_reads_counts_and_skips_the_rest() {
        let text = br#"{"schema_version":1,"ordering":{"sequence":["n","w","n","w"],"standard":false},
            "elements":{"nodes":5,"tagged_nodes":1,"ways":2,"relations":0,"total":7},"id_ranges":null}"#;
        let counts = super::parse_inspect_json(&text[..]).unwrap();
        assert_eq!((counts.nodes, counts.ways, counts.relations), (5, 2, 0));
        assert!(super::parse_inspect_json(&br#"{"elements":{"nodes":1}}"#[..]).is_err());
    }
}
