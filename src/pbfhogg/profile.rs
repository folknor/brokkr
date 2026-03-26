//! Two-pass profiling: timing instrumentation followed by allocation tracking.
//!
//! Replaces `profile-region.sh`. Builds the CLI binary twice — once with
//! `--features hotpath` for timing, once with `--features hotpath-alloc` for
//! allocation metrics. Results are stored in the database via the bench harness.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::harness::BenchHarness;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    osc_path: &Path,
    dataset_name: &str,
    file_mb: f64,
    scratch_dir: &Path,
    extra_features: &[String],
    project_root: &Path,
    bbox: Option<&str>,
) -> Result<(), DevError> {
    output::hotpath_msg(&format!("=== {dataset_name} ({file_mb:.0} MB) ==="));

    // -----------------------------------------------------------------------
    // TIMING PASS
    // -----------------------------------------------------------------------

    output::hotpath_msg("=== TIMING PASS ===");

    let mut timing_features: Vec<&str> = vec!["hotpath"];
    timing_features.extend(extra_features.iter().map(String::as_str));
    let binary = build::cargo_build(
        &build::BuildConfig::release_with_features(Some("pbfhogg-cli"), &timing_features),
        project_root,
    )?;

    super::hotpath::run(
        harness,
        &binary,
        pbf_path,
        osc_path,
        file_mb,
        1, // single run per test for profiling
        false,
        scratch_dir,
        project_root,
        None,
        dataset_name,
        bbox,
    )?;

    // -----------------------------------------------------------------------
    // ALLOCATION PASS
    // -----------------------------------------------------------------------

    output::hotpath_msg("=== ALLOCATION PASS ===");
    output::hotpath_msg(
        "NOTE: alloc profiling -- wall-clock times are not meaningful",
    );

    let mut alloc_features: Vec<&str> = vec!["hotpath-alloc"];
    alloc_features.extend(extra_features.iter().map(String::as_str));
    let binary = build::cargo_build(
        &build::BuildConfig::release_with_features(Some("pbfhogg-cli"), &alloc_features),
        project_root,
    )?;

    super::hotpath::run(
        harness,
        &binary,
        pbf_path,
        osc_path,
        file_mb,
        1, // single run per test for profiling
        true,
        scratch_dir,
        project_root,
        None,
        dataset_name,
        bbox,
    )?;

    // -----------------------------------------------------------------------
    // Done
    // -----------------------------------------------------------------------

    output::hotpath_msg(&format!("=== {dataset_name} COMPLETE ==="));

    Ok(())
}
