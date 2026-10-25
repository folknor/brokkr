//! Verify: diff --format osc - roundtrip validation via pbfhogg and osmium.

use std::path::Path;

use super::verify::{ExternalTool, Findings, Verdict, VerifyHarness, Writes};
use crate::error::DevError;
use crate::osc;
use crate::output::verify_msg;

/// Cross-validate `pbfhogg diff --format osc` against `osmium derive-changes`.
///
/// Creates a "new" PBF by applying changes, derives changes from old->new with
/// both tools, then roundtrips each derived OSC back through apply-changes and compares.
/// Finally derives again from the pair stripped of indexdata and requires the
/// indexed OSC's content.
#[allow(clippy::too_many_lines)]
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    osc: &Path,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let outdir = harness.subdir("derive-changes")?;

    verify_msg("=== verify diff --format osc ===");
    verify_msg(&format!("  old: {}", pbf.display()));
    verify_msg(&format!(
        "  osc: {} (used to create 'new' via apply-changes)",
        osc.display()
    ));

    let pbf_str = pbf.display().to_string();
    let osc_str = osc.display().to_string();

    // Step 1: Create "new" PBF by applying the OSC.
    let new_pbf = outdir.join("new.osm.pbf");
    let new_pbf_str = new_pbf.display().to_string();

    verify_msg("--- creating 'new' PBF via apply-changes ---");
    let mut ac_args = vec!["apply-changes", &pbf_str, &osc_str, "-o", &new_pbf_str];
    if direct_io {
        ac_args.push("--direct-io");
    }
    let captured = harness.run_pbfhogg(&ac_args, Writes::OwnedOutput)?;
    harness.check_exit(&captured, "pbfhogg apply-changes")?;

    // Step 2: Derive changes with both tools.
    let pbfhogg_osc = outdir.join("pbfhogg.osc.gz");
    let pbfhogg_osc_str = pbfhogg_osc.display().to_string();

    verify_msg("--- pbfhogg diff --format osc ---");
    let captured = harness.run_pbfhogg(
        &[
            "diff",
            "--format",
            "osc",
            &pbf_str,
            &new_pbf_str,
            "-o",
            &pbfhogg_osc_str,
        ],
        Writes::OwnedOutput,
    )?;
    // Like a text diff, it exits 1 when the inputs differ - the normal case
    // here - and still writes its OSC; 2 and above are real failures.
    captured.check_success_or("pbfhogg diff --format osc", &[1])?;

    let osmium_osc = outdir.join("osmium.osc.gz");
    let osmium_osc_str = osmium_osc.display().to_string();

    verify_msg("--- osmium derive-changes ---");
    let captured = harness.run_tool(
        ExternalTool::Osmium,
        &[
            "derive-changes",
            &pbf_str,
            &new_pbf_str,
            "-o",
            &osmium_osc_str,
            "--overwrite",
        ],
    )?;
    harness.check_exit(&captured, "osmium derive-changes")?;

    // Step 3: Report OSC file sizes.
    verify_msg("=== OSC file sizes ===");
    if let Ok(meta) = std::fs::metadata(&pbfhogg_osc) {
        verify_msg(&format!("  pbfhogg: {} bytes", meta.len()));
    }
    if let Ok(meta) = std::fs::metadata(&osmium_osc) {
        verify_msg(&format!("  osmium:  {} bytes", meta.len()));
    }

    // Step 4: Roundtrip - apply each derived OSC back to old.
    let rt_pbfhogg = outdir.join("roundtrip-pbfhogg.osm.pbf");
    let rt_pbfhogg_str = rt_pbfhogg.display().to_string();

    verify_msg("--- roundtrip: apply pbfhogg OSC ---");
    let mut rt_args = vec![
        "apply-changes",
        &pbf_str,
        &pbfhogg_osc_str,
        "-o",
        &rt_pbfhogg_str,
    ];
    if direct_io {
        rt_args.push("--direct-io");
    }
    let captured = harness.run_pbfhogg(&rt_args, Writes::OwnedOutput)?;
    harness.check_exit(&captured, "pbfhogg apply-changes (roundtrip)")?;

    let rt_osmium = outdir.join("roundtrip-osmium.osm.pbf");
    let rt_osmium_str = rt_osmium.display().to_string();

    verify_msg("--- roundtrip: apply osmium OSC ---");
    let captured = harness.run_tool(
        ExternalTool::Osmium,
        &[
            "apply-changes",
            &pbf_str,
            &osmium_osc_str,
            "-o",
            &rt_osmium_str,
            "--overwrite",
        ],
    )?;
    harness.check_exit(&captured, "osmium apply-changes (roundtrip)")?;

    // pbfhogg diff is a merge-join and refuses an input that does not declare
    // Sort.Type_then_ID. osmium's apply-changes output is content-sorted but
    // does not stamp the feature, so normalise it through `pbfhogg sort` - a
    // passthrough plus flag stamp, content unchanged - before comparing.
    // `--force` because osmium writes no blob-level indexdata.
    let rt_osmium_sorted = outdir.join("roundtrip-osmium-sorted.osm.pbf");
    let rt_osmium_sorted_str = rt_osmium_sorted.display().to_string();
    verify_msg("--- pbfhogg sort (normalise osmium roundtrip for merge-join diff) ---");
    let captured = harness.run_pbfhogg(
        &[
            "sort",
            "--force",
            &rt_osmium_str,
            "-o",
            &rt_osmium_sorted_str,
        ],
        Writes::OwnedOutput,
    )?;
    harness.check_exit(&captured, "pbfhogg sort (normalise osmium roundtrip)")?;

    // Element counts.
    verify_msg("=== element counts ===");
    harness.print_inspect("new", &new_pbf)?;
    harness.print_inspect("roundtrip-pbfhogg", &rt_pbfhogg)?;
    harness.print_inspect("roundtrip-osmium", &rt_osmium)?;

    let mut findings = Findings::new();

    for (label, a, b) in [
        ("pbfhogg roundtrip vs new", &rt_pbfhogg, &new_pbf),
        ("osmium roundtrip vs new", &rt_osmium_sorted, &new_pbf),
        (
            "pbfhogg roundtrip vs osmium roundtrip",
            &rt_pbfhogg,
            &rt_osmium_sorted,
        ),
    ] {
        verify_msg(&format!("=== diff: {label} ==="));
        let diff = harness.diff_pbfs(a, b)?;
        if diff.is_pass() {
            verify_msg("  PASS (identical)");
        } else {
            verify_msg("  FAIL (differences found)");
        }
        findings.record(&format!("diff {label}"), diff);
    }

    findings.record(
        "unindexed OSC vs indexed OSC",
        unindexed_osc_matches(harness, &outdir, pbf, &new_pbf, &pbfhogg_osc)?,
    );

    // Sort checks.
    findings.record("new order", harness.check_sorted("new", &new_pbf)?);
    findings.record(
        "roundtrip-pbfhogg order",
        harness.check_sorted("roundtrip-pbfhogg", &rt_pbfhogg)?,
    );

    Ok(findings)
}

/// Rerun the derive on the same pair stripped of indexdata and require the
/// indexed run's OSC exactly.
///
/// pbfhogg diff has a separate code path for inputs without indexdata, meant
/// to write the same OSC, full metadata included. apply-changes refuses an
/// unindexed base, so a raw variant cannot drive this check; stripping both
/// sides of the pair verified above can. The indexed OSC is cross-checked
/// against osmium by the roundtrips, so matching it holds the unindexed path
/// to osmium too.
fn unindexed_osc_matches(
    harness: &VerifyHarness,
    outdir: &Path,
    old: &Path,
    new: &Path,
    indexed_osc: &Path,
) -> Result<Verdict, DevError> {
    let old_raw = outdir.join("old-unindexed.osm.pbf");
    let new_raw = outdir.join("new-unindexed.osm.pbf");
    verify_msg("--- stripping indexdata from old and new ---");
    harness.strip_indexdata(old, &old_raw)?;
    harness.strip_indexdata(new, &new_raw)?;

    let raw_osc = outdir.join("pbfhogg-unindexed.osc.gz");
    let old_raw_str = old_raw.display().to_string();
    let new_raw_str = new_raw.display().to_string();
    let raw_osc_str = raw_osc.display().to_string();
    verify_msg("--- pbfhogg diff --format osc (unindexed inputs) ---");
    let captured = harness.run_pbfhogg(
        &[
            "diff",
            "--format",
            "osc",
            &old_raw_str,
            &new_raw_str,
            "-o",
            &raw_osc_str,
        ],
        Writes::OwnedOutput,
    )?;
    captured.check_success_or("pbfhogg diff --format osc (unindexed inputs)", &[1])?;

    verify_msg("=== diff: unindexed OSC vs indexed OSC ===");
    match osc::first_content_difference(&raw_osc, indexed_osc)? {
        None => {
            verify_msg("  PASS (identical content)");
            Ok(Verdict::Pass)
        }
        Some(offset) => {
            verify_msg(&format!(
                "  FAIL (decompressed content first differs at byte {offset})"
            ));
            Ok(Verdict::Fail)
        }
    }
}
