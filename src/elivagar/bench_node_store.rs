//! Benchmark: node store micro-benchmark.
//!
//! Replaces `bench-node-store.sh`. Builds the `bench_node_store` example with
//! the `hotpath` feature and runs it with passthrough output.

use std::path::Path;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(
    target_dir: &Path,
    project_root: &Path,
    nodes_millions: Option<usize>,
    runs: Option<usize>,
) -> Result<(), DevError> {
    let nodes_m = nodes_millions.unwrap_or(50);
    let run_count = runs.unwrap_or(5);

    // Build the example.
    output::build_msg("cargo build --release --features hotpath --example bench_node_store");

    let captured = output::run_captured(
        "cargo",
        &[
            "build",
            "--release",
            "--features",
            "hotpath",
            "--example",
            "bench_node_store",
        ],
        project_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "bench_node_store build failed: {stderr}"
        )));
    }

    // Run the example binary.
    let binary = target_dir.join("release/examples/bench_node_store");
    if !binary.exists() {
        return Err(DevError::Build(format!(
            "bench_node_store binary not found at {}",
            binary.display()
        )));
    }

    let nodes_str = nodes_m.to_string();
    let runs_str = run_count.to_string();
    let binary_str = binary.display().to_string();

    output::bench_msg(&format!(
        "bench_node_store: {nodes_m}M nodes, {run_count} runs"
    ));

    let captured = output::run_captured_with_env(
        &binary_str,
        &["--nodes", &nodes_str, "--runs", &runs_str],
        project_root,
        &[("HOTPATH_METRICS_SERVER_OFF", "true")],
    )?;

    // Print stdout (the benchmark results).
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }

    // Print stderr (hotpath output).
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
