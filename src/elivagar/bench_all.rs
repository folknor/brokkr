//! Combined elivagar benchmark suite: self + planetiler (+ tilemaker when available).
//!
//! Replaces the combined benchmark invocation pattern. Runs bench_self first,
//! then bench_planetiler as a comparison baseline, then bench_tilemaker (skipped
//! with a message if not yet implemented).

use std::path::Path;

use crate::config::{DevConfig, ResolvedPaths};
use crate::build;
use crate::error::DevError;
use crate::harness::BenchHarness;
use crate::output;

use super::{bench_planetiler, bench_self, bench_tilemaker};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    _dev_config: &DevConfig,
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

    // 3. bench tilemaker -- comparison baseline (stub)
    output::bench_msg("=== bench tilemaker ===");
    match bench_tilemaker::run() {
        Ok(()) => {}
        Err(e) => output::bench_msg(&format!("tilemaker skipped: {e}")),
    }

    Ok(())
}
