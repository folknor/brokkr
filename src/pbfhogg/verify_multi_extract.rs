//! Verify: multi-extract - compare single-pass multi-extract against
//! N sequential single-region extracts.
//!
//! Generates N non-overlapping longitude strips from the dataset bbox,
//! runs `pbfhogg extract --config <json> --simple` (single-pass), then
//! runs N individual `pbfhogg extract --simple -b=<strip>` calls
//! (sequential reference). Compares element counts per region.

use std::path::Path;

use super::verify::VerifyHarness;
use crate::error::DevError;
use crate::output::verify_msg;

/// Element counts parsed from `pbfhogg inspect` output.
struct Counts {
    nodes: u64,
    ways: u64,
    relations: u64,
}

/// Strip comma separators from a number string (e.g. "918,549" → "918549").
fn strip_commas(s: &str) -> String {
    s.chars().filter(|c| *c != ',').collect()
}

/// Parse element counts from `pbfhogg inspect` stdout.
///
/// The output format is:
/// ```text
///   Nodes:       771,666  (25,597 tagged)
///   Ways:        144,781
///   Relations:   2,102
/// ```
fn parse_inspect_counts(stdout: &str) -> Option<Counts> {
    let mut nodes = None;
    let mut ways = None;
    let mut relations = None;

    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Nodes:") {
            // "771,666  (25,597 tagged)" or just "771,666"
            let num_str = rest.split_whitespace().next()?;
            nodes = Some(strip_commas(num_str).parse().ok()?);
        } else if let Some(rest) = line.strip_prefix("Ways:") {
            let num_str = rest.split_whitespace().next()?;
            ways = Some(strip_commas(num_str).parse().ok()?);
        } else if let Some(rest) = line.strip_prefix("Relations:") {
            let num_str = rest.split_whitespace().next()?;
            relations = Some(strip_commas(num_str).parse().ok()?);
        }
    }

    Some(Counts {
        nodes: nodes?,
        ways: ways?,
        relations: relations?,
    })
}

/// Get element counts for a PBF file via `pbfhogg inspect`.
fn inspect_counts(harness: &VerifyHarness, pbf: &Path) -> Result<Counts, DevError> {
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
    bbox: &str,
    regions: usize,
    direct_io: bool,
) -> Result<(), DevError> {
    if regions == 0 {
        return Err(DevError::Config(
            "multi-extract verify requires at least 1 region".into(),
        ));
    }

    // Parse bbox.
    let parts: Vec<f64> = bbox
        .split(',')
        .map(|s| s.trim().parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DevError::Config(format!("invalid bbox: {e}")))?;
    if parts.len() != 4 {
        return Err(DevError::Config(format!(
            "bbox must have 4 values, got {}",
            parts.len()
        )));
    }
    let (min_lon, min_lat, max_lon, max_lat) = (parts[0], parts[1], parts[2], parts[3]);
    let strip_width = (max_lon - min_lon) / regions as f64;

    // Build strip bboxes.
    let mut strips: Vec<(f64, f64, f64, f64)> = Vec::new();
    for i in 0..regions {
        let s_min = min_lon + strip_width * i as f64;
        let s_max = if i + 1 == regions {
            max_lon
        } else {
            min_lon + strip_width * (i + 1) as f64
        };
        strips.push((s_min, min_lat, s_max, max_lat));
    }

    let outdir = harness.subdir("multi-extract")?;
    let pbf_str = pbf.display().to_string();

    verify_msg(&format!(
        "=== verify multi-extract ({regions} regions) ==="
    ));

    // --- Single-pass multi-extract via --config ---
    let multi_dir = outdir.join("multi");
    std::fs::create_dir_all(&multi_dir)?;

    let mut extracts = Vec::new();
    for (i, (s_min, s_min_lat, s_max, s_max_lat)) in strips.iter().enumerate() {
        extracts.push(format!(
            r#"    {{ "output": "strip-{i}.osm.pbf", "bbox": [{s_min}, {s_min_lat}, {s_max}, {s_max_lat}] }}"#
        ));
    }
    let config_json = format!(
        "{{\n  \"directory\": \"{}\",\n  \"extracts\": [\n{}\n  ]\n}}",
        multi_dir.display(),
        extracts.join(",\n"),
    );
    let config_path = outdir.join("multi-extract-config.json");
    std::fs::write(&config_path, &config_json)?;

    verify_msg("  running single-pass multi-extract...");
    let mut multi_args = vec![
        "extract",
        &pbf_str,
        "--config",
        &config_path.display().to_string(),
        "--simple",
    ];
    // Need to hold the string alive for the borrow.
    let config_path_str = config_path.display().to_string();
    multi_args = vec!["extract", &pbf_str, "--config", &config_path_str, "--simple"];
    if direct_io {
        multi_args.push("--direct-io");
    }
    let captured = harness.run_pbfhogg(&multi_args)?;
    harness.check_exit(&captured, "pbfhogg multi-extract")?;

    // --- Sequential single-region extracts ---
    let seq_dir = outdir.join("sequential");
    std::fs::create_dir_all(&seq_dir)?;

    verify_msg("  running sequential extracts...");
    for (i, (s_min, s_min_lat, s_max, s_max_lat)) in strips.iter().enumerate() {
        let seq_out = seq_dir.join(format!("strip-{i}.osm.pbf"));
        let seq_out_str = seq_out.display().to_string();
        let bbox_flag = format!("-b={s_min},{s_min_lat},{s_max},{s_max_lat}");

        let mut seq_args = vec![
            "extract",
            &pbf_str,
            "--simple",
            &bbox_flag,
            "-o",
            &seq_out_str,
        ];
        if direct_io {
            seq_args.push("--direct-io");
        }
        let captured = harness.run_pbfhogg(&seq_args)?;
        harness.check_exit(&captured, &format!("pbfhogg extract strip-{i}"))?;
    }

    // --- Compare element counts ---
    verify_msg("  comparing element counts...");
    let mut all_match = true;

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
            all_match = false;
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

    if !all_match {
        return Err(DevError::Verify(
            "multi-extract element counts differ from sequential extracts".into(),
        ));
    }

    verify_msg("  multi-extract: PASS (all regions match)");
    Ok(())
}
