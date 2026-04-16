//! Benchmark: full elivagar pipeline (PBF -> PMTiles).
//!
//! Replaces `bench-self.sh`. Builds the release binary, runs N times (best of),
//! and parses self-reported kv metrics from stderr (total_ms, phase12_ms,
//! ocean_ms, phase3_ms, phase4_ms, features, tiles, output_bytes).

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    data_dir: &Path,
    scratch_dir: &Path,
    project_root: &Path,
    skip_to: Option<&str>,
    compression_level: Option<u32>,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    std::fs::create_dir_all(scratch_dir)?;

    let output_path = scratch_dir.join("bench-self-output.pmtiles");
    let output_str = output_path.display().to_string();

    let tmp_dir = data_dir.join("tilegen_tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_dir_str = tmp_dir.display().to_string();

    // Build the command args: elivagar run <pbf> -o <output> [flags]
    let mut args: Vec<String> = vec![
        "run".into(),
        pbf_str.into(),
        "-o".into(),
        output_str,
        "--tmp-dir".into(),
        tmp_dir_str,
    ];

    if let Some(phase) = skip_to {
        args.push("--skip-to".into());
        args.push(phase.into());
    }
    if let Some(level) = compression_level {
        args.push("--compression-level".into());
        args.push(level.to_string());
    }
    opts.push_args(&mut args, data_dir);

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!(
        "elivagar pipeline: {basename} ({file_mb:.0} MB), {runs} run(s)"
    ));

    // All the pipeline tuning options (skip_to, compression_level, opts
    // flags like --no-ocean, --force-sorted, etc.) are in cli_args and
    // brokkr_args — no need to mirror them here. Metadata is empty;
    // locations_on_ways_detected is attached below from stderr.
    let metadata: Vec<KvPair> = Vec::new();

    let mut config = BenchConfig {
        command: "self".into(),
        variant: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(crate::harness::format_cli_args(
            &binary.display().to_string(),
            &arg_refs,
        )),
        brokkr_args: None,
        metadata,
    };

    // Use kv parsing: elivagar emits elapsed_ms, phase12_ms, ocean_ms,
    // phase3_ms, phase4_ms, features, tiles, output_bytes to stderr.
    // Use _raw so we can detect LocationsOnWays from stderr before recording.
    let (result, stderr) =
        harness.run_external_with_kv_raw(&config, binary, &arg_refs, project_root)?;
    let detected = super::detect_locations_on_ways_stderr(&stderr);
    config.metadata.push(KvPair::text(
        "meta.locations_on_ways_detected",
        detected.to_string(),
    ));
    harness.record_result(&config, &result)?;

    // Clean up output.
    std::fs::remove_file(&output_path).ok();

    Ok(())
}
