//! Verify: cat - pbfhogg cat vs osmium cat for each element type.

use std::path::Path;

use super::verify::{Findings, PbfArgv, VerifyHarness};
use super::verify_input_order::{CLAIM_UNSORTED, InputOrder, compare_outputs};
use super::verify_reference::ToolRun;
use crate::error::DevError;
use crate::output::verify_msg;

/// Cross-validate `pbfhogg cat` against `osmium cat` for node/way/relation types.
///
/// pbfhogg cat takes unsorted input and keeps its order, so on input without
/// the sort claim osmium reads an osmium-sorted copy and each pbfhogg output
/// is compared through one ([`compare_outputs`]).
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<Findings, DevError> {
    let outdir = harness.subdir("cat")?;
    let order = InputOrder::of(harness, pbf, &outdir)?;

    let mut findings = Findings::new();

    for elem_type in &["node", "way", "relation"] {
        verify_msg(&format!("=== verify cat -t {elem_type} ==="));

        // --- pbfhogg cat ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{elem_type}.osm.pbf"));

        let captured = harness.run_pbfhogg(
            PbfArgv::new("cat")
                .input(pbf)?
                .arg("--type")
                .arg(*elem_type)
                .output(&pbfhogg_out)?
                .arg_if(direct_io, "--direct-io"),
        )?;
        harness.check_exit(&captured, "pbfhogg cat")?;

        // --- osmium cat ---
        let osmium_out = outdir.join(format!("osmium-{elem_type}.osm.pbf"));

        let captured = harness.reference(
            &ToolRun::osmium("cat.1")
                .arg("cat")
                .input(&order.reference_input)
                .args(&["-t", elem_type, "-o"])
                .output(&osmium_out)
                .arg("--overwrite"),
            &outdir.join(format!("osmium-{elem_type}.stdout")),
        )?;
        harness.check_exit(&captured, "osmium cat")?;

        // --- Element counts ---
        harness.print_inspect("pbfhogg", &pbfhogg_out)?;
        harness.print_inspect("osmium", &osmium_out)?;

        compare_outputs(
            harness,
            &order,
            &format!("cat -t {elem_type}"),
            "osmium cat",
            &pbfhogg_out,
            &osmium_out,
            &mut findings,
        )?;
    }

    Ok(if order.sorted {
        findings
    } else {
        findings.with_claim(CLAIM_UNSORTED)
    })
}
