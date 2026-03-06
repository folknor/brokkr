//! Verify: diff — pbfhogg diff vs osmium diff summary comparison.

use std::fs;
use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use super::verify::VerifyHarness;

/// Cross-validate `pbfhogg diff` against `osmium diff --summary`.
///
/// Creates a "new" PBF by merging, then diffs old vs new with both tools
/// and compares their summary output and line counts.
pub fn run(harness: &VerifyHarness, pbf: &Path, osc: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("diff")?;

    verify_msg("=== verify diff ===");
    verify_msg(&format!("  old: {}", pbf.display()));
    verify_msg(&format!("  osc: {} (used to create 'new' via apply-changes)", osc.display()));

    let pbf_str = pbf.display().to_string();
    let osc_str = osc.display().to_string();

    // Create "new" PBF by applying the OSC.
    let new_pbf = outdir.join("new.osm.pbf");
    let new_pbf_str = new_pbf.display().to_string();

    verify_msg("--- creating 'new' PBF via apply-changes ---");
    let captured =
        harness.run_pbfhogg(&["apply-changes", &pbf_str, &osc_str, "-o", &new_pbf_str])?;
    harness.check_exit(&captured, "pbfhogg apply-changes")?;

    // pbfhogg diff — exits non-zero when differences exist, so do NOT check_exit.
    verify_msg("--- pbfhogg diff ---");
    let captured = harness.run_pbfhogg(&["diff", "-c", &pbf_str, &new_pbf_str])?;

    fs::write(outdir.join("pbfhogg-diff.txt"), &captured.stdout)?;
    fs::write(outdir.join("pbfhogg-summary.txt"), &captured.stderr)?;
    let pbfhogg_diff = String::from_utf8_lossy(&captured.stdout);
    let pbfhogg_summary = String::from_utf8_lossy(&captured.stderr);

    // osmium diff — exits non-zero when differences exist, so do NOT check_exit.
    verify_msg("--- osmium diff ---");
    let captured =
        harness.run_tool("osmium", &["diff", &pbf_str, &new_pbf_str, "--summary"])?;

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

    let pbfhogg_lines = pbfhogg_diff.lines().count();
    let osmium_lines = osmium_diff.lines().count();

    verify_msg("=== output line counts ===");
    verify_msg(&format!("  pbfhogg: {pbfhogg_lines} lines"));
    verify_msg(&format!("  osmium:  {osmium_lines} lines"));

    if pbfhogg_lines == osmium_lines {
        verify_msg("  PASS (line counts match)");
    } else {
        verify_msg("  FAIL (line counts differ)");
        return Err(DevError::Verify(format!(
            "diff line count mismatch: pbfhogg={pbfhogg_lines}, osmium={osmium_lines}"
        )));
    }

    Ok(())
}
