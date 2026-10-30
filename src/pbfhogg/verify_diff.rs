//! Verify: diff - pbfhogg diff vs osmium diff summary comparison.

use std::fs;
use std::path::Path;

use super::verify::{Findings, PbfArgv, Verdict, VerifyHarness};
use super::verify_reference::ToolRun;
use super::verify_stream;
use crate::error::DevError;
use crate::output::verify_msg;

/// `pbfhogg diff -c OLD NEW`, a text diff to stdout with automatic jobs:
/// its run scratch goes under the verify directory.
fn text_diff(harness: &VerifyHarness, old: &Path, new: &Path) -> Result<PbfArgv, DevError> {
    Ok(PbfArgv::new("diff")
        .arg("--suppress-common")
        .input(old)?
        .input(new)?
        .scratch(&harness.output_dir))
}

/// Cross-validate `pbfhogg diff` against `osmium diff --summary`.
///
/// Creates a "new" PBF by merging, then diffs old vs new with both tools
/// and compares their summary output and line counts, then reruns pbfhogg's
/// diff on the pair stripped of indexdata against its indexed answer.
pub fn run(harness: &VerifyHarness, pbf: &Path, osc: &Path) -> Result<Findings, DevError> {
    let outdir = harness.subdir("diff")?;

    verify_msg("=== verify diff ===");
    verify_msg(&format!("  old: {}", crate::output::path(pbf)));
    verify_msg(&format!(
        "  osc: {} (used to create 'new' via apply-changes)",
        crate::output::path(osc)
    ));

    // Create "new" PBF by applying the OSC.
    let new_pbf = outdir.join("new.osm.pbf");

    verify_msg("--- creating 'new' PBF via apply-changes ---");
    let captured = harness.run_pbfhogg(
        PbfArgv::new("apply-changes")
            .input(pbf)?
            .input(osc)?
            .output(&new_pbf)?,
    )?;
    harness.check_exit(&captured, "pbfhogg apply-changes")?;

    // pbfhogg diff exits 1 when differences exist; 2 and above mean it did
    // not complete, and its missing summary must not reach the parser.
    // Listings go to files, never memory: their size is the size of the
    // change set, which at planet scale is not bounded by any capture.
    verify_msg("--- pbfhogg diff ---");
    let pbfhogg_listing = outdir.join("pbfhogg-diff.txt");
    let captured_indexed =
        harness.run_pbfhogg_to(text_diff(harness, pbf, &new_pbf)?, &pbfhogg_listing)?;
    captured_indexed
        .check_success_or("pbfhogg diff", crate::output::child::Exits::answer(&[0, 1]))?;

    fs::write(outdir.join("pbfhogg-summary.txt"), &captured_indexed.stderr)?;
    let pbfhogg_summary = String::from_utf8_lossy(&captured_indexed.stderr);

    // osmium diff - exits 1 when differences exist, so only 0 and 1 pass.
    // `--suppress-common` (osmium's -c) is required to match pbfhogg's `-c`
    // above: without it osmium prints every object (common ones included),
    // making the line-count comparison meaningless (it emitted ~all of
    // denmark against pbfhogg's changes-only output). `--summary` prints its
    // stats on stderr, which we capture separately below.
    let osmium_listing = outdir.join("osmium-diff.txt");
    let captured = harness.reference(
        &ToolRun::osmium("diff.1")
            .args(&["diff", "--suppress-common"])
            .input(pbf)
            .input(&new_pbf)
            .arg("--summary")
            .accept(&[1]),
        &osmium_listing,
    )?;
    // osmium diff exits 1 when the inputs differ; anything else means its
    // summary cannot be trusted, however complete it looks.
    captured.check_success_or("osmium diff", crate::output::child::Exits::answer(&[0, 1]))?;

    fs::write(outdir.join("osmium-summary.txt"), &captured.stderr)?;
    let osmium_summary = String::from_utf8_lossy(&captured.stderr);

    // Print summaries (from stderr).
    verify_msg("=== pbfhogg diff summary ===");
    for line in pbfhogg_summary.lines() {
        verify_msg(&format!("  {line}"));
    }

    verify_msg("=== osmium diff summary ===");
    for line in osmium_summary.lines() {
        verify_msg(&format!("  {line}"));
    }

    // Raw line counts are informational only: pbfhogg emits one line per
    // modify (`*n.. v5 -> v6`) while osmium emits a -/+ pair, so the totals
    // are structurally unequal even when the tools agree. The gate below is
    // a per-class comparison of the two summaries instead.
    verify_msg("=== output line counts (informational) ===");
    verify_msg(&format!(
        "  pbfhogg: {} lines",
        verify_stream::count_lines(&pbfhogg_listing)?
    ));
    verify_msg(&format!(
        "  osmium:  {} lines",
        verify_stream::count_lines(&osmium_listing)?
    ));

    // compare_diff_classes reports every failure as an `Err`.
    compare_diff_classes(&pbfhogg_summary, &osmium_summary)?;

    let mut findings = Findings::new();
    findings.record(
        "unindexed diff vs indexed diff",
        unindexed_diff_matches(
            harness,
            &outdir,
            pbf,
            &new_pbf,
            &pbfhogg_listing,
            &pbfhogg_summary,
        )?,
    );
    Ok(findings)
}

/// Rerun the diff on the same pair stripped of indexdata and require the
/// indexed run's listing byte for byte and its summary's class counts.
///
/// pbfhogg diff has a separate code path for inputs without indexdata, meant
/// to answer exactly as the indexed one does. apply-changes refuses an
/// unindexed base, so a raw variant cannot drive this check; stripping both
/// sides of the pair verified above can. The indexed classes are reconciled
/// with osmium above, so matching them holds the unindexed path to osmium
/// too. The summary is compared by its classes, not its text, which leaves
/// pbfhogg free to word the two paths' stderr differently.
fn unindexed_diff_matches(
    harness: &VerifyHarness,
    outdir: &Path,
    old: &Path,
    new: &Path,
    indexed_listing: &Path,
    indexed_summary: &str,
) -> Result<Verdict, DevError> {
    let old_raw = outdir.join("old-unindexed.osm.pbf");
    let new_raw = outdir.join("new-unindexed.osm.pbf");
    verify_msg("--- stripping indexdata from old and new ---");
    harness.strip_indexdata(old, &old_raw)?;
    harness.strip_indexdata(new, &new_raw)?;

    verify_msg("--- pbfhogg diff (unindexed inputs) ---");
    let raw_listing = outdir.join("pbfhogg-unindexed-diff.txt");
    let captured = harness.run_pbfhogg_to(text_diff(harness, &old_raw, &new_raw)?, &raw_listing)?;
    captured.check_success_or(
        "pbfhogg diff (unindexed inputs)",
        crate::output::child::Exits::answer(&[0, 1]),
    )?;
    fs::write(
        outdir.join("pbfhogg-unindexed-summary.txt"),
        &captured.stderr,
    )?;
    let raw_summary = String::from_utf8_lossy(&captured.stderr);

    verify_msg("=== diff: unindexed vs indexed ===");
    let indexed_classes = parse_pbfhogg_diff_summary(indexed_summary);
    let raw_classes = parse_pbfhogg_diff_summary(&raw_summary);
    let listing_differs = verify_stream::first_difference(&raw_listing, indexed_listing)?;
    if listing_differs.is_none() && raw_classes.is_some() && raw_classes == indexed_classes {
        verify_msg("  PASS (identical listing and classes)");
        return Ok(Verdict::Pass);
    }
    if let Some(at) = listing_differs {
        verify_msg(&format!("  FAIL (listing first differs at byte {at})"));
    }
    if raw_classes != indexed_classes || raw_classes.is_none() {
        verify_msg(&format!(
            "  FAIL (classes differ: unindexed {raw_classes:?}, indexed {indexed_classes:?})"
        ));
    }
    Ok(Verdict::Fail)
}

/// Cross-check pbfhogg's `{c} created, {m} modified, {d} deleted` summary
/// against osmium's `left=.. right=..` summary.
///
/// osmium pairs objects by (type, id, version, timestamp), and a modified
/// element changes version on both sides, so osmium counts it as a left-only
/// (old version) + right-only (new version) entry rather than "different".
/// Ignoring a small deviation D (pbfhogg's documented version-comparison
/// difference), the counts reconcile as:
///
/// ```text
///   left  = deleted + modified + D
///   right = created + modified + D
/// ```
///
/// We solve D from each side and require the two to agree (structural
/// consistency), be non-negative, and stay within a small self-calibrating
/// ceiling - so the check tolerates the documented deviation without
/// hard-coding its value, but still catches a genuine class-count regression.
fn compare_diff_classes(pbfhogg_summary: &str, osmium_summary: &str) -> Result<(), DevError> {
    let Some((created, modified, deleted)) = parse_pbfhogg_diff_summary(pbfhogg_summary) else {
        return Err(DevError::Verify(format!(
            "diff verify: could not parse pbfhogg summary: {:?}",
            pbfhogg_summary.lines().next().unwrap_or("")
        )));
    };
    let Some((left, right)) = parse_osmium_diff_summary(osmium_summary) else {
        return Err(DevError::Verify(format!(
            "diff verify: could not parse osmium summary: {:?}",
            osmium_summary.lines().next().unwrap_or("")
        )));
    };

    // i128 so an osmium count smaller than pbfhogg's classes yields a negative
    // deviation (a real failure) rather than an unsigned-underflow panic.
    let d_left = i128::from(left) - i128::from(deleted) - i128::from(modified);
    let d_right = i128::from(right) - i128::from(created) - i128::from(modified);
    let changes = created + modified + deleted;
    // Generous sanity ceiling: 1% of the total changes, floor 64. The real
    // check is d_left == d_right; this only rejects a gross symmetric blow-up.
    let bound = i128::from((changes / 100).max(64));

    verify_msg("=== per-class change comparison ===");
    verify_msg(&format!(
        "  pbfhogg: {created} created, {modified} modified, {deleted} deleted"
    ));
    verify_msg(&format!("  osmium:  left={left}, right={right}"));
    verify_msg(&format!(
        "  version-comparison deviation: D_left={d_left}, D_right={d_right} (bound {bound})"
    ));

    if d_left != d_right {
        verify_msg("  FAIL (osmium left/right do not reconcile with pbfhogg classes)");
        return Err(DevError::Verify(format!(
            "diff per-class mismatch: D_left={d_left} != D_right={d_right} \
             (pbfhogg {created}/{modified}/{deleted} c/m/d vs osmium left={left} right={right})"
        )));
    }
    if d_left < 0 {
        verify_msg("  FAIL (osmium reports fewer changes than pbfhogg's classes)");
        return Err(DevError::Verify(format!(
            "diff per-class mismatch: negative deviation D={d_left}"
        )));
    }
    if d_left > bound {
        verify_msg("  FAIL (deviation exceeds sanity bound)");
        return Err(DevError::Verify(format!(
            "diff per-class mismatch: deviation D={d_left} exceeds bound {bound} \
             ({changes} total changes)"
        )));
    }

    verify_msg(&format!(
        "  PASS (classes reconcile; version-comparison deviation D={d_left} within {bound})"
    ));
    Ok(())
}

/// Parse pbfhogg's diff summary into `(created, modified, deleted)`.
/// Handles both `"Files are identical (N common elements)"` (all zero) and
/// `"{total} differences: {c} created, {m} modified, {d} deleted ({common} common)"`.
fn parse_pbfhogg_diff_summary(text: &str) -> Option<(u64, u64, u64)> {
    if text.contains("Files are identical") {
        return Some((0, 0, 0));
    }
    // The count for each class is the whitespace token immediately before the
    // class keyword (e.g. `5589 created`).
    let num_before = |kw: &str| -> Option<u64> {
        let idx = text.find(kw)?;
        text[..idx].split_whitespace().last()?.parse::<u64>().ok()
    };
    Some((
        num_before("created")?,
        num_before("modified")?,
        num_before("deleted")?,
    ))
}

/// Parse osmium's `--summary` line into `(left, right)`.
/// Format: `"Summary: left=4746 right=9092 same=.. different=.."`.
fn parse_osmium_diff_summary(text: &str) -> Option<(u64, u64)> {
    let field = |kw: &str| -> Option<u64> {
        let idx = text.find(kw)?;
        let rest = &text[idx + kw.len()..];
        rest.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u64>()
            .ok()
    };
    Some((field("left=")?, field("right=")?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{compare_diff_classes, parse_osmium_diff_summary, parse_pbfhogg_diff_summary};

    #[test]
    fn parse_pbfhogg_summary_standard() {
        let s = "10321 differences: 5589 created, 3489 modified, 1243 deleted (59147550 common)";
        assert_eq!(parse_pbfhogg_diff_summary(s), Some((5589, 3489, 1243)));
    }

    #[test]
    fn parse_pbfhogg_summary_identical() {
        let s = "Files are identical (59152282 common elements)";
        assert_eq!(parse_pbfhogg_diff_summary(s), Some((0, 0, 0)));
    }

    #[test]
    fn parse_osmium_summary_standard() {
        let s = "Summary: left=4746 right=9092 same=59147536 different=0";
        assert_eq!(parse_osmium_diff_summary(s), Some((4746, 9092)));
    }

    #[test]
    fn diff_class_summaries_reconcile_through_the_verifier() {
        let pbfhogg = "19 differences: 10 created, 5 modified, 4 deleted (0 common)";
        let osmium = "Summary: left=9 right=15 same=0 different=0";
        assert!(compare_diff_classes(pbfhogg, osmium).is_ok());
    }

    #[test]
    fn diff_class_summaries_reject_a_mismatch() {
        let pbfhogg = "19 differences: 10 created, 5 modified, 4 deleted (0 common)";
        let osmium = "Summary: left=9 right=16 same=0 different=0";
        assert!(compare_diff_classes(pbfhogg, osmium).is_err());
    }
}
