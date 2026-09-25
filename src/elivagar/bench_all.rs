//! Combined elivagar benchmark suite: self, planetiler, node-store, pmtiles, tilemaker.

use std::path::Path;

use crate::build;
use crate::config::ResolvedPaths;
use crate::error::DevError;
use crate::harness::BenchHarness;
use crate::output;

use super::{bench_node_store, bench_planetiler, bench_pmtiles, bench_self, bench_tilemaker};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    paths: &ResolvedPaths,
    project_root: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    _data_dir: &Path,
    _scratch_dir: &Path,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    // 1. bench self -- full elivagar pipeline
    output::bench_msg("=== bench self ===");
    let binary = build::cargo_build(
        &build::BuildConfig::release_with_owned_features(None, &paths.features),
        project_root,
    )?;
    bench_self::run(
        harness,
        &binary,
        pbf_path,
        file_mb,
        runs,
        &paths.data_dir,
        &paths.scratch_dir,
        project_root,
        None,
        opts,
    )?;

    // The two external baselines no longer downgrade a failure to "skipped".
    // A missing JDK and a crash mid-measurement both used to print one line
    // and let the suite exit 0, so a suite run could silently lack its
    // comparison rows. The remaining arms still run (one broken external tool
    // should not cost the others' numbers), and the suite fails at the end.
    let mut failures: Vec<String> = Vec::new();

    // 2. bench planetiler -- comparison baseline
    output::bench_msg("=== bench planetiler ===");
    if let Err(e) = bench_planetiler::run(
        harness,
        pbf_path,
        file_mb,
        runs,
        &paths.data_dir,
        &paths.scratch_dir,
        project_root,
    ) {
        if matches!(e, DevError::Interrupted) {
            return Err(e);
        }
        output::error(&format!("planetiler failed: {e}"));
        failures.push("planetiler".into());
    }

    // 3. bench node-store -- micro-benchmark
    output::bench_msg("=== bench node-store ===");
    bench_node_store::run(harness, project_root, 50, runs)?;

    // 4. bench pmtiles -- micro-benchmark
    output::bench_msg("=== bench pmtiles ===");
    bench_pmtiles::run(harness, project_root, 500_000, runs)?;

    // 5. bench tilemaker -- comparison baseline
    output::bench_msg("=== bench tilemaker ===");
    if let Err(e) = bench_tilemaker::run(
        harness,
        pbf_path,
        file_mb,
        runs,
        &paths.data_dir,
        &paths.scratch_dir,
        project_root,
    ) {
        if matches!(e, DevError::Interrupted) {
            return Err(e);
        }
        output::error(&format!("tilemaker failed: {e}"));
        failures.push("tilemaker".into());
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(DevError::Reported(format!(
            "bench all: {} failed",
            failures.join(", ")
        )))
    }
}
