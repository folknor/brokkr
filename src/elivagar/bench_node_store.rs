//! Benchmark: node store micro-benchmark.
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
    nodes_millions: usize,
    runs: usize,
) -> Result<(), DevError> {
    let binary = build::cargo_build(
        &build::BuildConfig {
            package: None,
            bin: None,
            example: Some("bench_node_store".into()),
            features: vec![],
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
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: 1, // example handles its own iterations
        cli_args: None,
        #[allow(clippy::cast_possible_wrap)]
        metadata: vec![KvPair::int("meta.nodes_millions", nodes_millions as i64), KvPair::int("meta.internal_runs", runs as i64)],
    };

    harness.run_internal(&config, |_i| {
        let captured = output::run_captured(
            &binary_str,
            &["--nodes", &nodes_str, "--runs", &runs_str],
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
            #[allow(clippy::cast_possible_wrap)]
            kv: vec![KvPair::int("nodes_millions", nodes_millions as i64), KvPair::int("internal_runs", runs as i64)],
            distribution: None,
            hotpath: None,
        })
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Hotpath variant
// ---------------------------------------------------------------------------

/// Run with hotpath instrumentation (`brokkr hotpath node-store`).
pub fn run_hotpath(
    harness: &BenchHarness,
    scratch_dir: &Path,
    project_root: &Path,
    nodes_millions: usize,
    runs: usize,
    alloc: bool,
) -> Result<(), DevError> {
    let feature = harness::hotpath_feature(alloc);
    let binary = build::cargo_build(
        &build::BuildConfig {
            package: None,
            bin: None,
            example: Some("bench_node_store".into()),
            features: vec![feature.into()],
            default_features: true,
            profile: "release",
        },
        project_root,
    )?;
    let binary_str = binary.display().to_string();

    let nodes_str = nodes_millions.to_string();
    let runs_str = runs.to_string();

    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("node-store{variant_suffix}");

    output::hotpath_msg(&format!(
        "=== bench_node_store {feature}: {nodes_millions}M nodes, {runs} runs ==="
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
        #[allow(clippy::cast_possible_wrap)]
        metadata: vec![KvPair::int("meta.nodes_millions", nodes_millions as i64), KvPair::int("meta.internal_runs", runs as i64), KvPair::text("meta.alloc", alloc.to_string())],
    };

    harness.run_internal(&config, |_i| {
        harness::run_hotpath_capture(
            &binary_str,
            &["--nodes", &nodes_str, "--runs", &runs_str],
            scratch_dir,
            project_root,
            &[],
        )
    })?;

    Ok(())
}
