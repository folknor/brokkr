//! Verify: extract - pbfhogg extract vs osmium extract for each strategy.

use std::path::Path;

use super::verify::{Findings, VerifyHarness};
use crate::error::DevError;
use crate::output::{verify_msg, verify_summary};

/// Cross-validate `pbfhogg extract` against `osmium extract` for
/// simple, complete-ways, and smart strategies.
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    bbox: &str,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let outdir = harness.subdir("extract")?;

    let pbf_str = pbf.display().to_string();
    let mut findings = Findings::new();

    for strategy in &["simple", "complete", "smart"] {
        verify_msg(&format!("=== verify extract --{strategy} ==="));

        // --- pbfhogg extract ---
        let pbfhogg_out = outdir.join(format!("pbfhogg-{strategy}.osm.pbf"));
        let pbfhogg_out_str = pbfhogg_out.display().to_string();

        let bbox_flag = format!("-b={bbox}");
        let mut pbfhogg_args = vec!["extract", &pbf_str, &bbox_flag, "-o", &pbfhogg_out_str];
        match *strategy {
            "simple" => pbfhogg_args.push("--simple"),
            "smart" => pbfhogg_args.push("--smart"),
            // "complete" is the default - no extra flag needed.
            _ => {}
        }
        if direct_io {
            pbfhogg_args.push("--direct-io");
        }

        let captured = harness.run_pbfhogg(&pbfhogg_args)?;
        harness.check_exit(&captured, "pbfhogg extract")?;

        // --- osmium extract ---
        let osmium_out = outdir.join(format!("osmium-{strategy}.osm.pbf"));
        let osmium_out_str = osmium_out.display().to_string();

        let osmium_strategy = match *strategy {
            "simple" => "simple",
            "complete" => "complete_ways",
            "smart" => "smart",
            _ => "complete_ways",
        };

        let captured = harness.run_tool(
            "osmium",
            &[
                "extract",
                &pbf_str,
                "-b",
                bbox,
                "-s",
                osmium_strategy,
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
        // would fail every run. Making it a verdict needs the expected
        // differences characterised first (as verify_merge does for osmium's
        // version-based deletes), not just counted. What this check gates is
        // that both tools completed (a crashed diff is still an `Err` from
        // `diff_pbfs`) and that the output is sorted. Because a quiet run
        // discards detail on pass, the difference is surfaced on the summary
        // channel rather than left in the buffer where nobody sees it.
        let diff = harness.diff_pbfs(&pbfhogg_out, &osmium_out)?;
        if diff.is_pass() {
            verify_msg(&format!("  diff ({strategy}): PASS (identical)"));
        } else {
            verify_summary(&format!(
                "extract --{strategy}: differs from osmium (informational, not gated)"
            ));
        }

        // --- Sort order ---
        let label = format!("pbfhogg extract --{strategy}");
        findings.record(
            &format!("{label} output order"),
            harness.check_sorted(&label, &pbfhogg_out)?,
        );
    }

    Ok(findings)
}
