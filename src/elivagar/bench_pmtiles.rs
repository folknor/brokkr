//! Benchmark: PMTiles writer micro-benchmark.
//!
//! - `run()` builds without hotpath (clean baseline).
//! - `run_hotpath()` builds with hotpath instrumentation for profiling.

use std::path::Path;

use crate::build;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness, BenchResult};
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(
    harness: &BenchHarness,
    project_root: &Path,
    features: &[String],
    tiles: usize,
    runs: usize,
) -> Result<(), DevError> {
    let mut config = super::build_config(features);
    config.example = Some("bench_pmtiles".into());
    let binary = build::cargo_build(&config, project_root)?.path;
    let binary_str = binary.display().to_string();

    let tiles_str = tiles.to_string();
    let runs_str = runs.to_string();

    output::bench_msg(&format!("bench_pmtiles: {tiles} tiles, {runs} runs"));

    let config = BenchConfig {
        // The CLI id of `brokkr pmtiles-writer`, the same name the hotpath
        // path records under (`ExampleCommand::id`).
        command: "pmtiles-writer".into(),
        // Tiles count is in cli_args (`--tiles N`) and brokkr_args.
        // Measurement mode comes from the harness.
        input_file: None,
        input_mb: None,
        cargo_features: None,
        external_build: None,
        runs: 1, // example handles its own iterations
        cli_args: None,
        metadata: vec![],
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

        Ok(BenchResult {
            elapsed_ms: ms,
            elapsed_us: None,
            #[allow(clippy::cast_possible_wrap)]
            kv: vec![
                KvPair::int("tiles", tiles as i64),
                KvPair::int("internal_runs", runs as i64),
            ],
            iterations: Vec::new(),
            distribution: None,
            hotpath: None,
        })
    })?;

    Ok(())
}
