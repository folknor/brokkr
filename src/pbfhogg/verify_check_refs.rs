//! Verify: check --refs - pbfhogg check --refs vs osmium check-refs.
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

use super::verify::{Findings, Verdict, VerifyHarness};
use crate::error::DevError;
use crate::output::verify_msg;

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
    /// "N (M references)" - we extract M. osmium outputs M directly.
    relations_in_relations: Option<u64>,
    /// pbfhogg only: the distinct missing relation members, the N of
    /// "N (M references)". It is N, not M, that sums into the total.
    relations_distinct: Option<u64>,
    /// pbfhogg only: the N of "Referential integrity: FAILED (N missing
    /// references)".
    total_missing: Option<u64>,
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

/// Extract the first number after the line's first `:`.
fn leading_number_after_colon(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once(':')?;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse()
        .ok()
}

/// pbfhogg's ways-only missing-node-in-ways count.
///
/// The parser recognises no per-class line for this mode: a clean file says
/// `Referential integrity: OK` (0), a dirty one `FAILED (N missing
/// references)`, and with only way node refs checked that N is the count.
fn pbfhogg_ways_only(counts: &CheckRefCounts) -> Option<u64> {
    if counts.integrity_ok {
        Some(0)
    } else {
        counts.nodes_in_ways.or(counts.total_missing)
    }
}

/// pbfhogg's with-relations counts as `(nodes in relations, ways in
/// relations, relation member references)`.
///
/// A clean file prints only `Referential integrity: OK`, which is all zeros -
/// the case that used to read as "could not parse counts". A dirty file
/// omits the lines of classes with nothing missing (the sample in
/// [`parse_pbfhogg`] has no nodes-in-ways line), so an absent class line is
/// taken as 0 - but only when the classes present account for the whole
/// FAILED total. If they do not, some line went unrecognised and this returns
/// `None` rather than guess.
fn pbfhogg_with_relations(counts: &CheckRefCounts) -> Option<(u64, u64, u64)> {
    if counts.integrity_ok {
        return Some((0, 0, 0));
    }
    let total = counts.total_missing?;
    let nodes = counts.nodes_in_relations.unwrap_or(0);
    let ways = counts.ways_in_relations.unwrap_or(0);
    let rel_distinct = counts.relations_distinct.unwrap_or(0);
    let rel_refs = counts.relations_in_relations.unwrap_or(0);
    let accounted = nodes.checked_add(ways)?.checked_add(rel_distinct)?;
    (accounted == total).then_some((nodes, ways, rel_refs))
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
        } else if lower.contains("referential integrity: failed") {
            counts.total_missing = leading_number_after_colon(line);
        } else if lower.contains("missing node members in relations") {
            counts.nodes_in_relations = trailing_number(line);
        } else if lower.contains("missing way refs in relations") {
            counts.ways_in_relations = trailing_number(line);
        } else if lower.contains("missing relation members") {
            counts.relations_in_relations = trailing_number(line);
            counts.relations_distinct = leading_number_after_colon(line);
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
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<Findings, DevError> {
    let mut findings = Findings::new();
    let outdir = harness.subdir("check-refs")?;
    let pbf_str = pbf.display().to_string();

    // --- Ways only ---
    verify_msg("--- check --refs (ways only) ---");
    let mut pbf_ways_args = vec!["check", "--refs", &pbf_str];
    if direct_io {
        pbf_ways_args.push("--direct-io");
    }
    let pbfhogg_text = capture_and_log(
        harness,
        "pbfhogg",
        &pbf_ways_args,
        &outdir.join("pbfhogg-ways.txt"),
        "pbfhogg (ways only)",
    )?;
    let osmium_text = capture_and_log(
        harness,
        "osmium",
        &["check-refs", &pbf_str],
        &outdir.join("osmium-ways.txt"),
        "osmium (ways only)",
    )?;

    let pbf_counts = parse_pbfhogg(&pbfhogg_text);
    let osm_counts = parse_osmium(&osmium_text);

    findings.record(
        "ways only",
        compare_count("ways only", pbfhogg_ways_only(&pbf_counts), osm_counts.nodes_in_ways),
    );

    // --- With relations ---
    verify_msg("--- check --refs (with relations) ---");
    let mut pbf_all_args = vec!["check", "--refs", &pbf_str, "--check-relations"];
    if direct_io {
        pbf_all_args.push("--direct-io");
    }
    let pbfhogg_text = capture_and_log(
        harness,
        "pbfhogg",
        &pbf_all_args,
        &outdir.join("pbfhogg-all.txt"),
        "pbfhogg (with relations)",
    )?;
    let osmium_text = capture_and_log(
        harness,
        "osmium",
        &["check-refs", "-r", &pbf_str],
        &outdir.join("osmium-all.txt"),
        "osmium (with relations)",
    )?;

    let pbf_counts = parse_pbfhogg(&pbfhogg_text);
    let osm_counts = parse_osmium(&osmium_text);

    let pbf_rel = pbfhogg_with_relations(&pbf_counts);
    if pbf_rel.is_none() {
        verify_msg(
            "  pbfhogg (with relations): per-class counts do not reconcile with the \
             FAILED total (or no verdict line) - output format changed?",
        );
    }
    for (label, pbfhogg, osmium) in [
        (
            "nodes in relations",
            pbf_rel.map(|(n, _, _)| n),
            osm_counts.nodes_in_relations,
        ),
        (
            "ways in relations",
            pbf_rel.map(|(_, w, _)| w),
            osm_counts.ways_in_relations,
        ),
        (
            "relation members",
            pbf_rel.map(|(_, _, r)| r),
            osm_counts.relations_in_relations,
        ),
    ] {
        findings.record(label, compare_count(label, pbfhogg, osmium));
    }

    Ok(findings)
}

fn compare_count(label: &str, pbfhogg: Option<u64>, osmium: Option<u64>) -> Verdict {
    match (pbfhogg, osmium) {
        (Some(p), Some(o)) if p == o => {
            verify_msg(&format!("  PASS ({label}): both report {p}"));
            Verdict::Pass
        }
        (Some(p), Some(o)) => {
            verify_msg(&format!("  FAIL ({label}): pbfhogg={p}, osmium={o}"));
            Verdict::Fail
        }
        _ => {
            verify_msg(&format!(
                "  FAIL ({label}): could not parse counts (pbfhogg={pbfhogg:?}, osmium={osmium:?})"
            ));
            Verdict::Fail
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const PBFHOGG_WAYS_OK: &str = "\
Elements: 52489653 nodes, 6616526 ways, 0 relations
Referential integrity: OK
";

    const PBFHOGG_RELATIONS_FAILED: &str = "\
Elements: 52489653 nodes, 6616526 ways, 46103 relations
Missing way refs in relations: 32943
Missing node members in relations: 441
Missing relation members: 706 (777 references)
Referential integrity: FAILED (34090 missing references)
";

    const OSMIUM_WAYS: &str = "\
There are 52489653 nodes, 6616526 ways, and 46103 relations in this file.
Nodes in ways missing: 0
";

    const OSMIUM_RELATIONS: &str = "\
There are 52489653 nodes, 6616526 ways, and 46103 relations in this file.
Nodes     in ways      missing: 0
Nodes     in relations missing: 441
Ways      in relations missing: 32943
Relations in relations missing: 777
";

    #[test]
    fn pbfhogg_ways_only_clean_is_zero() {
        let c = parse_pbfhogg(PBFHOGG_WAYS_OK);
        assert!(c.integrity_ok);
        assert_eq!(pbfhogg_ways_only(&c), Some(0));
    }

    #[test]
    fn pbfhogg_ways_only_failed_uses_total() {
        let c = parse_pbfhogg("Referential integrity: FAILED (12 missing references)\n");
        assert_eq!(pbfhogg_ways_only(&c), Some(12));
    }

    #[test]
    fn pbfhogg_with_relations_sample_parses() {
        let c = parse_pbfhogg(PBFHOGG_RELATIONS_FAILED);
        assert_eq!(c.ways_in_relations, Some(32_943));
        assert_eq!(c.nodes_in_relations, Some(441));
        assert_eq!(c.relations_in_relations, Some(777));
        assert_eq!(c.relations_distinct, Some(706));
        assert_eq!(c.total_missing, Some(34_090));
        assert_eq!(pbfhogg_with_relations(&c), Some((441, 32_943, 777)));
    }

    #[test]
    fn pbfhogg_with_relations_clean_dataset_is_all_zero() {
        // The bug: a clean dataset prints only the OK line and used to read
        // as "could not parse counts".
        let text = "\
Elements: 52489653 nodes, 6616526 ways, 46103 relations
Referential integrity: OK
";
        let c = parse_pbfhogg(text);
        assert_eq!(pbfhogg_with_relations(&c), Some((0, 0, 0)));
    }

    #[test]
    fn pbfhogg_with_relations_absent_class_is_zero_when_total_reconciles() {
        let text = "\
Missing way refs in relations: 10
Referential integrity: FAILED (10 missing references)
";
        let c = parse_pbfhogg(text);
        assert_eq!(pbfhogg_with_relations(&c), Some((0, 10, 0)));
    }

    #[test]
    fn pbfhogg_with_relations_unreconciled_total_is_none() {
        // A class line the parser does not recognise leaves the total
        // unaccounted for - refuse rather than read the gap as zero.
        let text = "\
Missing way refs in relations: 10
Missing something new: 5
Referential integrity: FAILED (15 missing references)
";
        let c = parse_pbfhogg(text);
        assert_eq!(pbfhogg_with_relations(&c), None);
        // No verdict line at all is not a clean dataset either.
        assert_eq!(pbfhogg_with_relations(&parse_pbfhogg("garbage\n")), None);
    }

    #[test]
    fn osmium_samples_parse() {
        let w = parse_osmium(OSMIUM_WAYS);
        assert_eq!(w.nodes_in_ways, Some(0));
        let r = parse_osmium(OSMIUM_RELATIONS);
        assert_eq!(r.nodes_in_ways, Some(0));
        assert_eq!(r.nodes_in_relations, Some(441));
        assert_eq!(r.ways_in_relations, Some(32_943));
        assert_eq!(r.relations_in_relations, Some(777));
    }

    #[test]
    fn samples_agree_across_tools() {
        let p = pbfhogg_with_relations(&parse_pbfhogg(PBFHOGG_RELATIONS_FAILED)).unwrap();
        let o = parse_osmium(OSMIUM_RELATIONS);
        assert_eq!(Some(p.0), o.nodes_in_relations);
        assert_eq!(Some(p.1), o.ways_in_relations);
        assert_eq!(Some(p.2), o.relations_in_relations);
    }

    #[test]
    fn compare_count_verdicts() {
        assert_eq!(compare_count("x", Some(3), Some(3)), Verdict::Pass);
        assert_eq!(compare_count("x", Some(3), Some(4)), Verdict::Fail);
        assert_eq!(compare_count("x", None, Some(0)), Verdict::Fail);
    }
}
