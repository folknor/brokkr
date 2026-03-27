//! Ingest benchmark for nidhogg.
//!
//! Replaces `bench_ingest.sh`. Times N runs of `nidhogg ingest <pbf> <output>`
//! using the BenchHarness external runner. Cleans up the scratch output
//! directory between runs.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Benchmark nidhogg ingestion performance.
///
/// Creates a temporary output directory in `scratch_dir`, runs `nidhogg ingest`
/// N times (cleaning between runs), and reports best-of-N timing.
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    scratch_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let pbf_str = super::client::path_str(pbf_path)?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let output_dir = scratch_dir.join("bench-ingest-output");
    let output_str = output_dir.display().to_string();

    output::bench_msg(&format!(
        "nidhogg ingest: {basename} ({file_mb:.0} MB), {runs} run(s)"
    ));

    let args: Vec<&str> = vec!["ingest", pbf_str, &output_str];

    let config = BenchConfig {
        command: "bench ingest".into(),
        variant: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(crate::harness::format_cli_args(
            &binary.display().to_string(),
            &args,
        )),
        metadata: vec![],
    };

    // Clean scratch before first run.
    clean_scratch(&output_dir)?;

    // Use run_external with per-run cleanup.
    // We cannot use harness.run_external directly because we need to clean
    // between runs. Use run_internal with manual timing instead.
    harness.run_internal(&config, |_i| {
        // Ensure clean output dir for each run.
        clean_scratch(&output_dir)?;

        let start = std::time::Instant::now();

        let captured = output::run_captured(&binary.display().to_string(), &args, project_root)?;

        let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);

        captured.check_success(&binary.display().to_string())?;

        Ok(crate::harness::BenchResult {
            elapsed_ms,
            kv: vec![],
            distribution: None,
            hotpath: None,
        })
    })?;

    // Final cleanup.
    std::fs::remove_dir_all(&output_dir).ok();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Remove and recreate the scratch output directory.
fn clean_scratch(dir: &Path) -> Result<(), DevError> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}
