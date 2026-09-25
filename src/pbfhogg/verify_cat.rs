//! Verify: cat - pbfhogg cat vs osmium cat for each element type.

use std::path::Path;

use super::verify::{Findings, VerifyHarness};
use crate::error::DevError;
use crate::output::verify_msg;

/// Cross-validate `pbfhogg cat` against `osmium cat` for node/way/relation types.
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<Findings, DevError> {
    let outdir = harness.subdir("cat")?;

    let pbf_str = pbf.display().to_string();
    let mut findings = Findings::new();

    for elem_type in &["node", "way", "relation"] {
        verify_msg(&format!("=== verify cat -t {elem_type} ==="));

        // --- pbfhogg cat ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{elem_type}.osm.pbf"));
        let pbfhogg_out_str = pbfhogg_out.display().to_string();

        let mut pbfhogg_args = vec!["cat", &pbf_str, "-t", elem_type, "-o", &pbfhogg_out_str];
        if direct_io {
            pbfhogg_args.push("--direct-io");
        }
        let captured = harness.run_pbfhogg(&pbfhogg_args)?;
        harness.check_exit(&captured, "pbfhogg cat")?;

        // --- osmium cat ---
        let osmium_out = outdir.join(format!("osmium-{elem_type}.osm.pbf"));
        let osmium_out_str = osmium_out.display().to_string();

        let captured = harness.run_tool(
            "osmium",
            &[
                "cat",
                &pbf_str,
                "-t",
                elem_type,
                "-o",
                &osmium_out_str,
                "--overwrite",
            ],
        )?;
        harness.check_exit(&captured, "osmium cat")?;

        // --- Element counts ---
        harness.print_inspect("pbfhogg", &pbfhogg_out)?;
        harness.print_inspect("osmium", &osmium_out)?;

        // --- Diff ---
        let diff = harness.diff_pbfs(&pbfhogg_out, &osmium_out)?;
        if diff.is_pass() {
            verify_msg(&format!("  diff ({elem_type}): PASS (identical)"));
        } else {
            verify_msg(&format!("  diff ({elem_type}): FAIL (differences found)"));
        }
        findings.record(&format!("cat -t {elem_type} diff"), diff);

        // --- Sort feature comparison ---
        findings.record(
            &format!("cat -t {elem_type} sort feature"),
            harness.compare_sort_feature(&pbfhogg_out, &osmium_out)?,
        );
    }

    Ok(findings)
}
