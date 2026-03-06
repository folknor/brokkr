//! Verify: check --refs — pbfhogg check --refs vs osmium check-refs.

use std::fs;
use std::path::Path;

use crate::error::DevError;
use crate::output::verify_msg;
use super::verify::VerifyHarness;

/// Run check --refs cross-validation: pbfhogg check --refs vs osmium check-refs.
///
/// Two modes: ways-only (default) and with relations. Both tools may exit
/// non-zero when missing refs are found, so we do not check exit status.
pub fn run(harness: &VerifyHarness, pbf: &Path) -> Result<(), DevError> {
    let outdir = harness.subdir("check-refs")?;
    let pbf_str = pbf.display().to_string();

    // --- Ways only ---
    verify_msg("--- check --refs (ways only) ---");

    let captured = harness.run_pbfhogg(&["check", "--refs", &pbf_str])?;
    let pbfhogg_text = format!(
        "{}{}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr),
    );
    fs::write(outdir.join("pbfhogg-ways.txt"), &pbfhogg_text)?;

    let captured = harness.run_tool("osmium", &["check-refs", &pbf_str])?;
    let osmium_text = format!(
        "{}{}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr),
    );
    fs::write(outdir.join("osmium-ways.txt"), &osmium_text)?;

    verify_msg("  pbfhogg (ways only):");
    for line in pbfhogg_text.lines() {
        verify_msg(&format!("    {line}"));
    }
    verify_msg("  osmium (ways only):");
    for line in osmium_text.lines() {
        verify_msg(&format!("    {line}"));
    }

    let ways_match = pbfhogg_text.trim() == osmium_text.trim();
    if ways_match {
        verify_msg("  PASS (ways only)");
    } else {
        verify_msg("  FAIL (ways only): output differs");
    }

    // --- With relations ---
    verify_msg("--- check --refs (with relations) ---");

    let captured = harness.run_pbfhogg(&["check", "--refs", &pbf_str, "--check-relations"])?;
    let pbfhogg_text = format!(
        "{}{}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr),
    );
    fs::write(outdir.join("pbfhogg-all.txt"), &pbfhogg_text)?;

    let captured = harness.run_tool("osmium", &["check-refs", "-r", &pbf_str])?;
    let osmium_text = format!(
        "{}{}",
        String::from_utf8_lossy(&captured.stdout),
        String::from_utf8_lossy(&captured.stderr),
    );
    fs::write(outdir.join("osmium-all.txt"), &osmium_text)?;

    verify_msg("  pbfhogg (with relations):");
    for line in pbfhogg_text.lines() {
        verify_msg(&format!("    {line}"));
    }
    verify_msg("  osmium (with relations):");
    for line in osmium_text.lines() {
        verify_msg(&format!("    {line}"));
    }

    let relations_match = pbfhogg_text.trim() == osmium_text.trim();
    if relations_match {
        verify_msg("  PASS (with relations)");
    } else {
        verify_msg("  FAIL (with relations): output differs");
    }

    if !ways_match || !relations_match {
        return Err(DevError::Verify(
            "check --refs: pbfhogg and osmium output differ".into(),
        ));
    }

    Ok(())
}
