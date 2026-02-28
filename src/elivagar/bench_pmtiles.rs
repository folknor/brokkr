//! Benchmark: PMTiles writer micro-benchmark.
//!
//! Replaces `bench-pmtiles.sh`. Builds the `bench_pmtiles` example and runs it
//! with passthrough output.

use std::path::Path;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(
    target_dir: &Path,
    project_root: &Path,
    tiles: Option<usize>,
    runs: Option<usize>,
) -> Result<(), DevError> {
    let tile_count = tiles.unwrap_or(500_000);
    let run_count = runs.unwrap_or(5);

    // Build the example.
    output::build_msg("cargo build --release --example bench_pmtiles");

    let captured = output::run_captured(
        "cargo",
        &[
            "build",
            "--release",
            "--example",
            "bench_pmtiles",
        ],
        project_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "bench_pmtiles build failed: {stderr}"
        )));
    }

    // Run the example binary.
    let binary = target_dir.join("release/examples/bench_pmtiles");
    if !binary.exists() {
        return Err(DevError::Build(format!(
            "bench_pmtiles binary not found at {}",
            binary.display()
        )));
    }

    let tiles_str = tile_count.to_string();
    let runs_str = run_count.to_string();
    let binary_str = binary.display().to_string();

    output::bench_msg(&format!(
        "bench_pmtiles: {tile_count} tiles, {run_count} runs"
    ));

    let captured = output::run_captured(
        &binary_str,
        &["--tiles", &tiles_str, "--runs", &runs_str],
        project_root,
    )?;

    // Print stdout (the benchmark results).
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }

    // Print stderr.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    if !captured.status.success() {
        return Err(DevError::Subprocess {
            program: binary_str,
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    Ok(())
}
