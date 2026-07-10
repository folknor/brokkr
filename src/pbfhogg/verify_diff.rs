//! Verify: diff - pbfhogg diff vs osmium diff summary comparison.

use std::fs;
use std::path::Path;

use super::verify::VerifyHarness;
use crate::error::DevError;
use crate::output::verify_msg;

/// Cross-validate `pbfhogg diff` against `osmium diff --summary`.
///
/// Creates a "new" PBF by merging, then diffs old vs new with both tools
/// and compares their summary output and line counts.
pub fn run(harness: &VerifyHarness, pbf: &Path, osc: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("diff")?;

    verify_msg("=== verify diff ===");
    verify_msg(&format!("  old: {}", pbf.display()));
    verify_msg(&format!(
        "  osc: {} (used to create 'new' via apply-changes)",
        osc.display()
    ));

    let pbf_str = pbf.display().to_string();
    let osc_str = osc.display().to_string();

    // Create "new" PBF by applying the OSC.
    let new_pbf = outdir.join("new.osm.pbf");
    let new_pbf_str = new_pbf.display().to_string();

    verify_msg("--- creating 'new' PBF via apply-changes ---");
    let captured =
        harness.run_pbfhogg(&["apply-changes", &pbf_str, &osc_str, "-o", &new_pbf_str])?;
    harness.check_exit(&captured, "pbfhogg apply-changes")?;

    // pbfhogg diff - exits non-zero when differences exist, so do NOT check_exit.
    verify_msg("--- pbfhogg diff ---");
    let captured = harness.run_pbfhogg(&["diff", "-c", &pbf_str, &new_pbf_str])?;

    fs::write(outdir.join("pbfhogg-diff.txt"), &captured.stdout)?;
    fs::write(outdir.join("pbfhogg-summary.txt"), &captured.stderr)?;
    let pbfhogg_diff = String::from_utf8_lossy(&captured.stdout);
    let pbfhogg_summary = String::from_utf8_lossy(&captured.stderr);

    // osmium diff - exits non-zero when differences exist, so do NOT check_exit.
    // `--suppress-common` (osmium's -c) is required to match pbfhogg's `-c`
    // above: without it osmium prints every object (common ones included),
    // making the line-count comparison meaningless (it emitted ~all of
    // denmark against pbfhogg's changes-only output). `--summary` prints its
    // stats on stderr, which we capture separately below.
    let captured = harness.run_tool(
        "osmium",
        &["diff", "--suppress-common", &pbf_str, &new_pbf_str, "--summary"],
    )?;

    fs::write(outdir.join("osmium-diff.txt"), &captured.stdout)?;
    fs::write(outdir.join("osmium-summary.txt"), &captured.stderr)?;
    let osmium_diff = String::from_utf8_lossy(&captured.stdout);
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
    verify_msg(&format!("  pbfhogg: {} lines", pbfhogg_diff.lines().count()));
    verify_msg(&format!("  osmium:  {} lines", osmium_diff.lines().count()));

    compare_diff_classes(&pbfhogg_summary, &osmium_summary)
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
    use super::{parse_osmium_diff_summary, parse_pbfhogg_diff_summary};

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
    fn denmark_numbers_reconcile() {
        // The documented decomposition: D=14 on both sides.
        let (created, modified, deleted) =
            parse_pbfhogg_diff_summary("10321 differences: 5589 created, 3489 modified, 1243 deleted (0 common)").unwrap();
        let (left, right) =
            parse_osmium_diff_summary("Summary: left=4746 right=9092 same=0 different=0").unwrap();
        let d_left = i128::from(left) - i128::from(deleted) - i128::from(modified);
        let d_right = i128::from(right) - i128::from(created) - i128::from(modified);
        assert_eq!(d_left, 14);
        assert_eq!(d_right, 14);
    }
}
