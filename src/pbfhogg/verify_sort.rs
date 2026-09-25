//! Verify: sort - pbfhogg sort vs osmium sort.

use std::path::Path;

use super::verify::{Findings, VerifyHarness};
use crate::error::DevError;
use crate::output::verify_msg;

/// Cross-validate `pbfhogg sort` against `osmium sort`.
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<Findings, DevError> {
    let outdir = harness.subdir("sort")?;

    verify_msg("=== verify sort ===");

    // --- pbfhogg sort ---
    let pbf_str = pbf.display().to_string();
    let pbfhogg_out = outdir.join("pbfhogg.osm.pbf");
    let pbfhogg_out_str = pbfhogg_out.display().to_string();

    let mut pbfhogg_args = vec!["sort", &pbf_str, "-o", &pbfhogg_out_str];
    if direct_io {
        pbfhogg_args.push("--direct-io");
    }
    let captured = harness.run_pbfhogg(&pbfhogg_args)?;
    harness.check_exit(&captured, "pbfhogg sort")?;

    // --- osmium sort ---
    let osmium_out = outdir.join("osmium.osm.pbf");
    let osmium_out_str = osmium_out.display().to_string();

    let captured = harness.run_tool(
        "osmium",
        &["sort", &pbf_str, "-o", &osmium_out_str, "--overwrite"],
    )?;
    harness.check_exit(&captured, "osmium sort")?;

    // --- Element counts ---
    harness.print_inspect("pbfhogg", &pbfhogg_out)?;
    harness.print_inspect("osmium", &osmium_out)?;

    let mut findings = Findings::new();

    // --- Diff ---
    let diff = harness.diff_pbfs(&pbfhogg_out, &osmium_out)?;
    if diff.is_pass() {
        verify_msg("  diff: PASS (identical)");
    } else {
        verify_msg("  diff: FAIL (differences found)");
    }
    findings.record("pbfhogg vs osmium diff", diff);

    // --- Sort order ---
    findings.record(
        "pbfhogg sort output order",
        harness.check_sorted("pbfhogg sort", &pbfhogg_out)?,
    );

    Ok(findings)
}
