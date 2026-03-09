//! Verify: check --refs — pbfhogg check --refs vs osmium check-refs.
//!
//! The two tools use different output formats, so we parse the missing-reference
//! counts structurally rather than comparing raw text. Known semantic differences:
//!
//! - Ways-only mode: pbfhogg skips relation blobs and reports "0 relations" in its
//!   element summary, while osmium always reports the full relation count. Both
//!   agree on the actual check result (missing node reference count).
//!
//! - With-relations mode: pbfhogg reports "Missing relation members: 706 (777
//!   references)" where 777 is the occurrence count that matches osmium's output.
//!   We extract the trailing number (the references count) for comparison.

use std::fs;
use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use super::verify::VerifyHarness;

/// Parsed check-refs counts for structural comparison.
#[derive(Default)]
struct CheckRefCounts {
    /// Missing node references in ways (ways-only and with-relations modes).
    nodes_in_ways: Option<u64>,
    /// Missing node members in relations.
    nodes_in_relations: Option<u64>,
    /// Missing way members in relations.
    ways_in_relations: Option<u64>,
    /// Missing relation member references (occurrences). pbfhogg outputs
    /// "N (M references)" — we extract M. osmium outputs M directly.
    relations_in_relations: Option<u64>,
    /// Whether the output indicates all checks passed with no issues.
    integrity_ok: bool,
}

/// Extract the last number from a line (the count is always at the end).
fn trailing_number(line: &str) -> Option<u64> {
    line.split(|c: char| !c.is_ascii_digit())
        .rev()
        .find(|s| !s.is_empty())?
        .parse()
        .ok()
}

/// Parse pbfhogg check --refs output.
///
/// Ways-only format:
/// ```text
/// Elements: 52489653 nodes, 6616526 ways, 0 relations
/// Referential integrity: OK
/// ```
///
/// With-relations format:
/// ```text
/// Elements: 52489653 nodes, 6616526 ways, 46103 relations
/// Missing way refs in relations: 32943
/// Missing node members in relations: 441
/// Missing relation members: 706 (777 references)
/// Referential integrity: FAILED (34090 missing references)
/// ```
fn parse_pbfhogg(text: &str) -> CheckRefCounts {
    let mut counts = CheckRefCounts::default();
    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.contains("referential integrity: ok") {
            counts.integrity_ok = true;
        } else if lower.contains("missing node members in relations") {
            counts.nodes_in_relations = trailing_number(line);
        } else if lower.contains("missing way refs in relations") {
            counts.ways_in_relations = trailing_number(line);
        } else if lower.contains("missing relation members") {
            counts.relations_in_relations = trailing_number(line);
        }
    }
    counts
}

/// Parse osmium check-refs output.
///
/// Ways-only format:
/// ```text
/// There are 52489653 nodes, 6616526 ways, and 46103 relations in this file.
/// Nodes in ways missing: 0
/// ```
///
/// With-relations format:
/// ```text
/// There are 52489653 nodes, 6616526 ways, and 46103 relations in this file.
/// Nodes     in ways      missing: 0
/// Nodes     in relations missing: 441
/// Ways      in relations missing: 32943
/// Relations in relations missing: 777
/// ```
fn parse_osmium(text: &str) -> CheckRefCounts {
    let mut counts = CheckRefCounts::default();
    for line in text.lines() {
        let lower = line.to_lowercase();
        // Normalize whitespace for matching (osmium uses padding)
        let normalized: String = lower.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.contains("nodes in ways missing") {
            counts.nodes_in_ways = trailing_number(line);
        } else if normalized.contains("nodes in relations missing") {
            counts.nodes_in_relations = trailing_number(line);
        } else if normalized.contains("ways in relations missing") {
            counts.ways_in_relations = trailing_number(line);
        } else if normalized.contains("relations in relations missing") {
            counts.relations_in_relations = trailing_number(line);
        }
    }
    counts
}

/// Capture output from a tool, save to file, and print with prefix.
fn capture_and_log(
    harness: &VerifyHarness,
    tool: &str,
    args: &[&str],
    out_file: &Path,
    label: &str,
) -> Result<String, DevError> {
    let captured = if tool == "pbfhogg" {
        harness.run_pbfhogg(args)?
    } else {
        harness.run_tool(tool, args)?
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr),
    );
    fs::write(out_file, &text)?;
    verify_msg(&format!("  {label}:"));
    for line in text.lines() {
        verify_msg(&format!("    {line}"));
    }
    Ok(text)
}

/// Run check --refs cross-validation: pbfhogg check --refs vs osmium check-refs.
///
/// Two modes: ways-only (default) and with relations. Both tools may exit
/// non-zero when missing refs are found, so we do not check exit status.
pub fn run(harness: &VerifyHarness, pbf: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("check-refs")?;
    let pbf_str = pbf.display().to_string();

    // --- Ways only ---
    verify_msg("--- check --refs (ways only) ---");
    let pbfhogg_text = capture_and_log(
        harness, "pbfhogg", &["check", "--refs", &pbf_str],
        &outdir.join("pbfhogg-ways.txt"), "pbfhogg (ways only)",
    )?;
    let osmium_text = capture_and_log(
        harness, "osmium", &["check-refs", &pbf_str],
        &outdir.join("osmium-ways.txt"), "osmium (ways only)",
    )?;

    let pbf_counts = parse_pbfhogg(&pbfhogg_text);
    let osm_counts = parse_osmium(&osmium_text);

    // pbfhogg reports "Referential integrity: OK" with no count line when 0.
    let pbf_ways = if pbf_counts.integrity_ok { Some(0) } else { pbf_counts.nodes_in_ways };
    let ways_ok = compare_count("ways only", pbf_ways, osm_counts.nodes_in_ways);

    // --- With relations ---
    verify_msg("--- check --refs (with relations) ---");
    let pbfhogg_text = capture_and_log(
        harness, "pbfhogg", &["check", "--refs", &pbf_str, "--check-relations"],
        &outdir.join("pbfhogg-all.txt"), "pbfhogg (with relations)",
    )?;
    let osmium_text = capture_and_log(
        harness, "osmium", &["check-refs", "-r", &pbf_str],
        &outdir.join("osmium-all.txt"), "osmium (with relations)",
    )?;

    let pbf_counts = parse_pbfhogg(&pbfhogg_text);
    let osm_counts = parse_osmium(&osmium_text);

    let nodes_ok = compare_count(
        "nodes in relations", pbf_counts.nodes_in_relations, osm_counts.nodes_in_relations,
    );
    let ways_rel_ok = compare_count(
        "ways in relations", pbf_counts.ways_in_relations, osm_counts.ways_in_relations,
    );

    let rels_ok = compare_count(
        "relation members", pbf_counts.relations_in_relations, osm_counts.relations_in_relations,
    );

    if !ways_ok || !nodes_ok || !ways_rel_ok || !rels_ok {
        return Err(DevError::Verify(
            "check --refs: missing reference counts differ between pbfhogg and osmium".into(),
        ));
    }

    Ok(())
}

fn compare_count(label: &str, pbfhogg: Option<u64>, osmium: Option<u64>) -> bool {
    match (pbfhogg, osmium) {
        (Some(p), Some(o)) if p == o => {
            verify_msg(&format!("  PASS ({label}): both report {p}"));
            true
        }
        (Some(p), Some(o)) => {
            verify_msg(&format!("  FAIL ({label}): pbfhogg={p}, osmium={o}"));
            false
        }
        _ => {
            verify_msg(&format!("  FAIL ({label}): could not parse counts (pbfhogg={pbfhogg:?}, osmium={osmium:?})"));
            false
        }
    }
}
