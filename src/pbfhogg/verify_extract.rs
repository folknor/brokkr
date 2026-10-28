//! Verify: extract - pbfhogg extract vs osmium extract for each strategy.

use std::path::Path;

use super::commands::{ExtractStrategy, PbfhoggCommand};
use super::verify::{ExternalTool, Findings, VerifyHarness};
use crate::datasets::Bbox;
use crate::error::DevError;
use crate::output::{verify_msg, verify_summary};

/// Cross-validate `pbfhogg extract` against `osmium extract` for
/// simple, complete-ways, and smart strategies.
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    bbox: &Bbox,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let outdir = harness.subdir("extract")?;

    let pbf_str = pbf.display().to_string();
    let mut findings = Findings::new();

    for strategy in ExtractStrategy::all() {
        let strategy_name = strategy.name();
        verify_msg(&format!("=== verify extract --{strategy_name} ==="));

        // --- pbfhogg extract ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{strategy_name}.osm.pbf"));
        let command = PbfhoggCommand::Extract {
            strategy: *strategy,
        };
        let pbfhogg_args = command
            .extract_args_for(pbf, bbox, &pbfhogg_out)?
            .arg_if(direct_io, "--direct-io");
        let captured = harness.run_pbfhogg(pbfhogg_args)?;
        harness.check_exit(&captured, "pbfhogg extract")?;

        // --- osmium extract ---
        let osmium_out = outdir.join(format!("osmium-{strategy_name}.osm.pbf"));
        let osmium_out_str = osmium_out.display().to_string();
        let captured = harness.run_tool(
            ExternalTool::Osmium,
            &[
                "extract",
                &pbf_str,
                "-b",
                bbox.as_str(),
                "-s",
                strategy.osmium_name(),
                "-o",
                &osmium_out_str,
                "--overwrite",
            ],
        )?;
        harness.check_exit(&captured, "osmium extract")?;

        // --- Element counts ---
        harness.print_inspect("pbfhogg", &pbfhogg_out)?;
        harness.print_inspect("osmium", &osmium_out)?;

        // --- Diff (informational) ---
        //
        // Deliberately NOT a verdict: extract is known to differ from osmium
        // in minor ways, so an element diff is expected and gating on it
        // would fail every run. Making this a verdict needs the expected
        // differences characterised first (as verify_merge does for osmium's
        // version-based deletes), not just counted. What this check gates is
        // that both tools completed (a crashed diff is still an `Err` from
        // `diff_pbfs`) and that the output is sorted. Because a quiet run
        // discards detail on pass, the difference is surfaced on the summary
        // channel rather than left in the buffer where nobody sees it.
        let diff = harness.diff_pbfs(&pbfhogg_out, &osmium_out)?;
        if diff.is_pass() {
            verify_msg(&format!("  diff ({strategy_name}): PASS (identical)"));
        } else {
            verify_summary(&format!(
                "extract --{strategy_name}: differs from osmium (informational, not gated)"
            ));
        }

        // --- Sort order ---
        let label = format!("pbfhogg extract --{strategy_name}");
        findings.record(
            &format!("{label} output order"),
            harness.check_sorted(&label, &pbfhogg_out)?,
        );
    }

    Ok(findings)
}
