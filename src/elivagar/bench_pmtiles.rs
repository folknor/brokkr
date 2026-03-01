//! Benchmark: PMTiles writer micro-benchmark.
//!
//! - `run()` builds without hotpath (clean baseline).
//! - `run_hotpath()` builds with hotpath instrumentation for profiling.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness, BenchResult};
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(
    harness: &BenchHarness,
    project_root: &Path,
    tiles: usize,
    runs: usize,
) -> Result<(), DevError> {
    let binary = build::cargo_build(
        &build::BuildConfig {
            package: None,
            bin: None,
            example: Some("bench_pmtiles".into()),
            features: vec![],
            default_features: true,
            profile: "release",
        },
        project_root,
    )?;
    let binary_str = binary.display().to_string();

    let tiles_str = tiles.to_string();
    let runs_str = runs.to_string();

    output::bench_msg(&format!(
        "bench_pmtiles: {tiles} tiles, {runs} runs"
    ));

    let config = BenchConfig {
        command: "bench pmtiles".into(),
        variant: None,
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: 1, // example handles its own iterations
        cli_args: None,
        metadata: Some(serde_json::json!({
            "tiles": tiles,
            "internal_runs": runs,
        })),
    };

    harness.run_internal(&config, |_i| {
        let captured = output::run_captured(
            &binary_str,
            &["--tiles", &tiles_str, "--runs", &runs_str],
            project_root,
        )?;

        // Print stdout (benchmark results).
        let stdout = String::from_utf8_lossy(&captured.stdout);
        if !stdout.is_empty() {
            print!("{stdout}");
        }
        let stderr = String::from_utf8_lossy(&captured.stderr);
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }

        captured.check_success(&binary_str)?;

        let ms = harness::elapsed_to_ms(&captured.elapsed);
        let extra = serde_json::json!({
            "tiles": tiles,
            "internal_runs": runs,
        });

        Ok(BenchResult {
            elapsed_ms: ms,
            extra: Some(extra),
        })
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Hotpath variant
// ---------------------------------------------------------------------------

/// Run with hotpath instrumentation (`brokkr hotpath pmtiles`).
pub fn run_hotpath(
    harness: &BenchHarness,
    scratch_dir: &Path,
    project_root: &Path,
    tiles: usize,
    runs: usize,
    alloc: bool,
) -> Result<(), DevError> {
    let feature = harness::hotpath_feature(alloc);
    let binary = build::cargo_build(
        &build::BuildConfig {
            package: None,
            bin: None,
            example: Some("bench_pmtiles".into()),
            features: vec![feature.into()],
            default_features: true,
            profile: "release",
        },
        project_root,
    )?;
    let binary_str = binary.display().to_string();

    let tiles_str = tiles.to_string();
    let runs_str = runs.to_string();

    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("pmtiles{variant_suffix}");

    output::hotpath_msg(&format!(
        "=== bench_pmtiles {feature}: {tiles} tiles, {runs} runs ==="
    ));

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: None,
        input_mb: None,
        cargo_features: Some(feature.into()),
        cargo_profile: "release".into(),
        runs: 1,
        cli_args: None,
        metadata: Some(serde_json::json!({
            "tiles": tiles,
            "internal_runs": runs,
            "alloc": alloc,
        })),
    };

    harness.run_internal(&config, |_i| {
        harness::run_hotpath_capture(
            &binary_str,
            &["--tiles", &tiles_str, "--runs", &runs_str],
            scratch_dir,
            project_root,
            &[],
        )
    })?;

    Ok(())
}
