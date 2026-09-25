//! Verify: add-locations-to-ways - pbfhogg vs osmium, across index-type modes.

use std::path::Path;

use super::verify::{Findings, Verdict, VerifyHarness};
use crate::cli::AltwMode;
use crate::error::DevError;
use crate::output::{CapturedOutput, verify_msg};

/// Cross-validate `pbfhogg add-locations-to-ways` against `osmium add-locations-to-ways`.
///
/// `mode` selects which index-type variants are exercised:
/// - `Hash` / `Sparse` / `Dense` / `External`: runs that single mode and diffs its
///   output directly against osmium's.
/// - `All`: runs hash (diffed against osmium), then sparse/dense/external diffed
///   against the hash baseline. Sparse and dense may skip with an allocation
///   failure on memory-constrained systems.
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    mode: AltwMode,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let outdir = harness.subdir("add-locations-to-ways")?;

    verify_msg("=== verify add-locations-to-ways ===");

    let pbf_str = pbf.display().to_string();

    // Osmium runs once as the reference for every mode.
    let osmium_out = outdir.join("osmium.osm.pbf");
    let osmium_out_str = osmium_out.display().to_string();
    let captured = harness.run_tool(
        "osmium",
        &[
            "add-locations-to-ways",
            &pbf_str,
            "-o",
            &osmium_out_str,
            "--overwrite",
        ],
    )?;
    harness.check_exit(&captured, "osmium add-locations-to-ways")?;
    harness.print_inspect("osmium", &osmium_out)?;

    match mode {
        AltwMode::All => run_all(harness, &outdir, &pbf_str, &osmium_out, direct_io),
        AltwMode::Hash => run_single(harness, &outdir, &pbf_str, &osmium_out, None, direct_io),
        AltwMode::Sparse => {
            run_single(harness, &outdir, &pbf_str, &osmium_out, Some("sparse"), direct_io)
        }
        AltwMode::Dense => {
            run_single(harness, &outdir, &pbf_str, &osmium_out, Some("dense"), direct_io)
        }
        AltwMode::External => run_single(
            harness,
            &outdir,
            &pbf_str,
            &osmium_out,
            Some("external"),
            direct_io,
        ),
    }
}

fn run_all(
    harness: &VerifyHarness,
    outdir: &Path,
    pbf_str: &str,
    osmium_out: &Path,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let mut findings = Findings::new();

    // Hash is the baseline - must succeed, diffed directly against osmium.
    let hash_out = outdir.join("pbfhogg.osm.pbf");
    let captured = run_pbfhogg_mode(harness, pbf_str, &hash_out, None, direct_io)?;
    harness.check_exit(&captured, "pbfhogg add-locations-to-ways")?;
    harness.print_inspect("pbfhogg", &hash_out)?;
    findings.record(
        "hash vs osmium diff",
        report_diff(harness, osmium_out, &hash_out, "hash vs osmium")?,
    );
    findings.record(
        "hash sort feature",
        harness.compare_sort_feature(&hash_out, osmium_out)?,
    );

    // Optional variants - compared against hash baseline, tolerating alloc failure.
    for (index_type, alloc_may_fail) in [("sparse", false), ("dense", true), ("external", false)] {
        run_optional_variant(
            harness,
            outdir,
            pbf_str,
            &hash_out,
            index_type,
            direct_io,
            alloc_may_fail,
            &mut findings,
        )?;
    }

    Ok(findings)
}

fn run_single(
    harness: &VerifyHarness,
    outdir: &Path,
    pbf_str: &str,
    osmium_out: &Path,
    index_type: Option<&str>,
    direct_io: bool,
) -> Result<Findings, DevError> {
    let mut findings = Findings::new();
    let label = index_type.unwrap_or("hash");
    let filename = format!("pbfhogg-{label}.osm.pbf");
    let out_path = outdir.join(&filename);

    let captured = run_pbfhogg_mode(harness, pbf_str, &out_path, index_type, direct_io)?;
    harness.check_exit(&captured, &format!("pbfhogg add-locations-to-ways ({label})"))?;
    harness.print_inspect(&format!("pbfhogg ({label})"), &out_path)?;
    findings.record(
        &format!("{label} vs osmium diff"),
        report_diff(harness, osmium_out, &out_path, &format!("{label} vs osmium"))?,
    );
    findings.record(
        &format!("{label} sort feature"),
        harness.compare_sort_feature(&out_path, osmium_out)?,
    );

    Ok(findings)
}

/// Run one non-baseline index variant and diff it against the hash output.
///
/// A non-zero exit is a failure, except for a variant marked
/// `alloc_may_fail` (dense), whose up-front allocation legitimately fails on
/// hosts without `vm.overcommit_memory=1`. That exemption keys on the exit
/// status alone - brokkr does not parse pbfhogg's allocation-failure message -
/// so the variant's stderr is echoed to make a different cause visible.
#[allow(clippy::too_many_arguments)]
fn run_optional_variant(
    harness: &VerifyHarness,
    outdir: &Path,
    pbf_str: &str,
    hash_out: &Path,
    index_type: &str,
    direct_io: bool,
    alloc_may_fail: bool,
    findings: &mut Findings,
) -> Result<(), DevError> {
    verify_msg(&format!("--- {index_type} index variant ---"));

    let filename = format!("pbfhogg-{index_type}.osm.pbf");
    let out_path = outdir.join(&filename);

    let result = run_pbfhogg_mode(harness, pbf_str, &out_path, Some(index_type), direct_io)?;

    if result.status.success() {
        findings.record(
            &format!("hash vs {index_type} diff"),
            report_diff(harness, hash_out, &out_path, &format!("hash vs {index_type}"))?,
        );
    } else {
        let stderr = String::from_utf8_lossy(&result.stderr);
        if alloc_may_fail {
            verify_msg(&format!(
                "  {index_type} index skipped (non-zero exit, taken as allocation failure - \
                 expected on systems without vm.overcommit_memory=1)"
            ));
        } else {
            verify_msg(&format!("  {index_type} index FAILED (non-zero exit)"));
            findings.fail(format!("{index_type} index exited non-zero"));
        }
        for line in stderr.lines() {
            verify_msg(&format!("    {line}"));
        }
    }

    Ok(())
}

fn run_pbfhogg_mode(
    harness: &VerifyHarness,
    pbf_str: &str,
    out_path: &Path,
    index_type: Option<&str>,
    direct_io: bool,
) -> Result<CapturedOutput, DevError> {
    let out_str = out_path.display().to_string();
    let mut args = vec!["add-locations-to-ways", pbf_str, "-o", &out_str];
    if let Some(it) = index_type {
        args.push("--index-type");
        args.push(it);
    }
    if direct_io {
        args.push("--direct-io");
    }
    harness.run_pbfhogg(&args)
}

/// Diff `a` against `b`, narrate the outcome, and hand the verdict back for
/// the caller to record.
fn report_diff(
    harness: &VerifyHarness,
    a: &Path,
    b: &Path,
    label: &str,
) -> Result<Verdict, DevError> {
    let verdict = harness.diff_pbfs(a, b)?;
    if verdict.is_pass() {
        verify_msg(&format!("  diff ({label}): PASS (identical)"));
    } else {
        verify_msg(&format!("  diff ({label}): FAIL (differences found)"));
    }
    Ok(verdict)
}
