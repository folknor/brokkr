//! Combined elivagar benchmark suite: self, planetiler, node-store, pmtiles, tilemaker.

use std::path::Path;

use crate::config::ResolvedPaths;
use crate::build;
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
    data_dir: &Path,
    scratch_dir: &Path,
) -> Result<(), DevError> {
    // 1. bench self -- full elivagar pipeline
    output::bench_msg("=== bench self ===");
    let binary = build::cargo_build(&build::BuildConfig::release(None), project_root)?;
    bench_self::run(
        harness,
        &binary,
        pbf_path,
        file_mb,
        runs,
        data_dir,
        scratch_dir,
        project_root,
        None,    // skip_to
        false,   // no_ocean
        None,    // compression_level
    )?;

    // 2. bench planetiler -- comparison baseline
    output::bench_msg("=== bench planetiler ===");
    match bench_planetiler::run(
        harness,
        pbf_path,
        file_mb,
        runs,
        &paths.data_dir,
        scratch_dir,
        project_root,
    ) {
        Ok(()) => {}
        Err(e) => output::bench_msg(&format!("planetiler skipped: {e}")),
    }

    // 3. bench node-store -- micro-benchmark
    output::bench_msg("=== bench node-store ===");
    bench_node_store::run(harness, project_root, 50, runs)?;

    // 4. bench pmtiles -- micro-benchmark
    output::bench_msg("=== bench pmtiles ===");
    bench_pmtiles::run(harness, project_root, 500_000, runs)?;

    // 5. bench tilemaker -- comparison baseline (stub)
    output::bench_msg("=== bench tilemaker ===");
    match bench_tilemaker::run() {
        Ok(()) => {}
        Err(e) => output::bench_msg(&format!("tilemaker skipped: {e}")),
    }

    Ok(())
}
