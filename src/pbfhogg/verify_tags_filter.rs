//! Verify: tags-filter - pbfhogg tags-filter vs osmium tags-filter.

use std::path::Path;

use super::verify::{Findings, PbfArgv, VerifyHarness};
use super::verify_reference::ToolRun;
use crate::error::DevError;
use crate::output::verify_msg;

/// Expression and output label pairs for the three test cases.
const EXPRESSIONS: &[(&str, &str)] = &[
    ("highway=primary", "highway-R"),
    ("amenity=restaurant", "amenity-R"),
    ("w/highway=primary", "w-highway-R"),
];

/// Run tags-filter cross-validation: pbfhogg vs osmium, 3 expressions with `-R`.
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<Findings, DevError> {
    let outdir = harness.subdir("tags-filter")?;
    let mut findings = Findings::new();

    for (expr, label) in EXPRESSIONS {
        verify_msg(&format!("--- tags-filter {expr} -R ---"));

        // pbfhogg: tags-filter <pbf> -R <expr> -o <out>
        let pbfhogg_out = outdir.join(format!("pbfhogg-{label}.osm.pbf"));
        let captured = harness.run_pbfhogg(
            PbfArgv::new("tags-filter")
                .input(pbf)?
                .arg("-R")
                .arg(*expr)
                .output(&pbfhogg_out)?
                .arg_if(direct_io, "--direct-io"),
        )?;
        harness.check_exit(&captured, "pbfhogg tags-filter")?;

        // osmium: tags-filter <pbf> <expr> -R -o <out> --overwrite
        let osmium_out = outdir.join(format!("osmium-{label}.osm.pbf"));
        let captured = harness.reference(
            &ToolRun::osmium("tags-filter.1")
                .arg("tags-filter")
                .input(pbf)
                .args(&[expr, "-R", "-o"])
                .output(&osmium_out)
                .arg("--overwrite"),
            &outdir.join(format!("osmium-{label}.stdout")),
        )?;
        harness.check_exit(&captured, "osmium tags-filter")?;

        // Print inspect output for both outputs.
        harness.print_inspect("pbfhogg", &pbfhogg_out)?;
        harness.print_inspect("osmium", &osmium_out)?;

        // Diff and report.
        let diff = harness.diff_pbfs(
            &pbfhogg_out,
            &osmium_out,
            &super::verify::listing_beside(&pbfhogg_out),
        )?;
        if diff.is_pass() {
            verify_msg(&format!("  {label}: PASS (identical)"));
        } else {
            verify_msg(&format!("  {label}: FAIL (differences found)"));
        }
        findings.record(&format!("{label} diff"), diff);

        // Compare sort feature flags.
        findings.record(
            &format!("{label} sort feature"),
            harness.compare_sort_feature(&pbfhogg_out, &osmium_out)?,
        );
    }

    Ok(findings)
}
