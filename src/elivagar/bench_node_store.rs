//! Benchmark: node store micro-benchmark.
//!
//! Builds the `bench_node_store` example with the `hotpath` feature and runs it.
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
    nodes_millions: usize,
    runs: usize,
) -> Result<(), DevError> {
    let binary = build::cargo_build(
        &build::BuildConfig {
            package: None,
            bin: None,
            example: Some("bench_node_store".into()),
            features: vec!["hotpath".into()],
            default_features: true,
            profile: "release",
        },
        project_root,
    )?;

    let binary_str = binary.display().to_string();
    let nodes_str = nodes_millions.to_string();
    let runs_str = runs.to_string();

    output::bench_msg(&format!(
        "bench_node_store: {nodes_millions}M nodes, {runs} runs"
    ));

    let config = BenchConfig {
        command: "bench node-store".into(),
        variant: None,
        input_file: None,
        input_mb: None,
        cargo_features: Some("hotpath".into()),
        cargo_profile: "release".into(),
        runs: 1, // example handles its own iterations
        cli_args: None,
        metadata: Some(serde_json::json!({
            "nodes_millions": nodes_millions,
            "internal_runs": runs,
        })),
    };

    harness.run_internal(&config, |_i| {
        let captured = output::run_captured_with_env(
            &binary_str,
            &["--nodes", &nodes_str, "--runs", &runs_str],
            project_root,
            &[("HOTPATH_METRICS_SERVER_OFF", "true")],
        )?;

        // Print stdout (benchmark results) and stderr (hotpath output).
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
            "nodes_millions": nodes_millions,
            "internal_runs": runs,
        });

        Ok(BenchResult {
            elapsed_ms: ms,
            extra: Some(extra),
        })
    })?;

    Ok(())
}
