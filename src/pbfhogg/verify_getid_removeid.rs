//! Verify: getid — pbfhogg getid vs osmium getid, plus getid --invert complement test.

use std::path::Path;

use super::verify::VerifyHarness;
use crate::error::DevError;
use crate::output::verify_msg;

/// Element IDs known to exist in Denmark PBFs.
const IDS: &[&str] = &[
    "n115722", "n115723", "n115724", "w2080", "w2081", "w2082", "r174", "r213", "r339",
];

/// Run getid cross-validation: pbfhogg getid vs osmium getid,
/// then pbfhogg getid --invert complement test.
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<(), DevError> {
    let outdir = harness.subdir("getid-removeid")?;
    let pbf_str = pbf.display().to_string();

    verify_msg("--- getid: pbfhogg vs osmium ---");

    // pbfhogg getid <pbf> -o <out> <ids...>
    let pbfhogg_getid = outdir.join("pbfhogg-getid.osm.pbf");
    let pbfhogg_getid_str = pbfhogg_getid.display().to_string();
    let mut pbfhogg_args: Vec<&str> = vec!["getid", &pbf_str, "-o", &pbfhogg_getid_str];
    if direct_io {
        pbfhogg_args.push("--direct-io");
    }
    pbfhogg_args.extend_from_slice(IDS);
    let captured = harness.run_pbfhogg(&pbfhogg_args)?;
    harness.check_exit(&captured, "pbfhogg getid")?;

    // osmium getid <pbf> <ids...> -o <out> --overwrite
    let osmium_getid = outdir.join("osmium-getid.osm.pbf");
    let osmium_getid_str = osmium_getid.display().to_string();
    let mut osmium_args: Vec<&str> = vec!["getid", &pbf_str];
    osmium_args.extend_from_slice(IDS);
    osmium_args.extend_from_slice(&["-o", &osmium_getid_str, "--overwrite"]);
    let captured = harness.run_tool("osmium", &osmium_args)?;
    harness.check_exit(&captured, "osmium getid")?;

    // Print inspect output for both getid outputs.
    harness.print_inspect("pbfhogg getid", &pbfhogg_getid)?;
    harness.print_inspect("osmium getid", &osmium_getid)?;

    // Diff and report.
    let identical = harness.diff_pbfs(&pbfhogg_getid, &osmium_getid)?;
    if identical {
        verify_msg("  getid: PASS (identical)");
    } else {
        verify_msg("  getid: FAIL (differences found)");
    }

    // Compare sort feature flags.
    harness.compare_sort_feature(&pbfhogg_getid, &osmium_getid)?;

    // --- getid --invert: complement test ---
    verify_msg("--- getid --invert: complement test ---");

    let pbfhogg_invert = outdir.join("pbfhogg-getid-invert.osm.pbf");
    let pbfhogg_invert_str = pbfhogg_invert.display().to_string();
    let mut invert_args: Vec<&str> = vec!["getid", "--invert", &pbf_str, "-o", &pbfhogg_invert_str];
    if direct_io {
        invert_args.push("--direct-io");
    }
    invert_args.extend_from_slice(IDS);
    let captured = harness.run_pbfhogg(&invert_args)?;
    harness.check_exit(&captured, "pbfhogg getid --invert")?;

    // Print inspect output for original, getid, and getid --invert (complement validation).
    harness.print_inspect("original", pbf)?;
    harness.print_inspect("getid", &pbfhogg_getid)?;
    harness.print_inspect("getid --invert", &pbfhogg_invert)?;

    Ok(())
}
