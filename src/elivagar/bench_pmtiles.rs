//! Benchmark: PMTiles writer micro-benchmark.
//!
//! Builds the `bench_pmtiles` example and runs it.
//! Results are stored in the database via the bench harness.

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
            features: Vec::new(),
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
    };

    harness.run_internal(&config, |_i| {
        let captured = output::run_captured(
            &binary_str,
            &["--tiles", &tiles_str, "--runs", &runs_str],
            project_root,
        )?;

        // Print stdout (benchmark results) and stderr.
        let stdout = String::from_utf8_lossy(&captured.stdout);
        if !stdout.is_empty() {
            print!("{stdout}");
        }
        let stderr = String::from_utf8_lossy(&captured.stderr);
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }

        if !captured.status.success() {
            return Err(DevError::Subprocess {
                program: binary_str.clone(),
                code: captured.status.code(),
                stderr: stderr.into_owned(),
            });
        }

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
