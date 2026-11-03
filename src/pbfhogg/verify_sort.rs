//! Verify: sort - pbfhogg sort vs osmium sort.

use std::path::Path;

use super::verify::{Findings, PbfArgv, VerifyHarness};
use super::verify_reference::ToolRun;
use crate::error::DevError;
use crate::output::verify_msg;

/// The file name of this check's osmium sort output. The artifact name is
/// part of the reference key, so another check sorting the same input under
/// this name (in its own directory) reuses this check's cached run.
pub(super) const OSMIUM_SORTED: &str = "osmium.osm.pbf";

/// `osmium sort <input> -o <output> --overwrite` as a reference run, its
/// exit status required to be 0.
pub(super) fn osmium_sort(
    harness: &VerifyHarness,
    input: &Path,
    output: &Path,
    stdout: &Path,
) -> Result<(), DevError> {
    let captured = harness.reference(
        &ToolRun::osmium("sort.1")
            .arg("sort")
            .input(input)
            .arg("-o")
            .output(output)
            .arg("--overwrite"),
        stdout,
    )?;
    harness.check_exit(&captured, "osmium sort")
}

/// Cross-validate `pbfhogg sort` against `osmium sort`.
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<Findings, DevError> {
    let outdir = harness.subdir("sort")?;

    verify_msg("=== verify sort ===");

    // --- pbfhogg sort ---
    let pbfhogg_out = outdir.join("pbfhogg.osm.pbf");

    let captured = harness.run_pbfhogg(
        PbfArgv::new("sort")
            .input(pbf)?
            .output(&pbfhogg_out)?
            .arg_if(direct_io, "--direct-io"),
    )?;
    harness.check_exit(&captured, "pbfhogg sort")?;

    // --- osmium sort ---
    let osmium_out = outdir.join(OSMIUM_SORTED);
    osmium_sort(harness, pbf, &osmium_out, &outdir.join("osmium.stdout"))?;

    // --- Element counts (pbfhogg's output is summarised by its sort check) ---
    harness.print_inspect("osmium", &osmium_out)?;

    let mut findings = Findings::new();

    // --- Diff ---
    let diff = harness.diff_pbfs(
        &pbfhogg_out,
        &osmium_out,
        &super::verify::listing_beside(&pbfhogg_out),
    )?;
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
