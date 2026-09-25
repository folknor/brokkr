//! Benchmark: full elivagar pipeline (PBF -> PMTiles), `bench all`'s self arm.
//!
//! Builds nothing itself (the caller hands in the binary) and measures the
//! same way `brokkr tilegen --bench` does (`dispatch::run_elivagar_wallclock`):
//! brokkr's own best-of-N external wall-clock via `run_external_ok`, with the
//! argv built by the same `ElivagarCommand::Tilegen::build_args`. tilegen
//! emits its metrics as FIFO counters (sidecar.db) as of the elivagar side's
//! 54f9b07 and no longer prints `elapsed_ms=` on stderr, so the old
//! `run_external_with_kv_raw` path - which requires that line - failed this
//! arm outright.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

use super::commands::ElivagarCommand;

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

    let command = ElivagarCommand::Tilegen { opts, skip_to };
    let args = command.build_args(pbf_str, scratch_dir, data_dir)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!(
        "elivagar pipeline: {basename} ({file_mb:.0} MB), {runs} run(s)"
    ));

    // Same row shape as `tilegen --bench`: the pipeline contract (skip_to plus
    // everything the tilegen block expands to, ocean inputs included) lands in
    // cli_args verbatim, and `brokkr results --grep` selects on it.
    let config = BenchConfig {
        command: command.result_command().into(),
        mode: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs,
        cli_args: Some(crate::harness::format_cli_args(
            &binary.display().to_string(),
            &arg_refs,
        )),
        brokkr_args: None,
        metadata: command.metadata(),
    };

    let result = harness.run_external_ok(&config, binary, &arg_refs, project_root, &[]);

    // The self arm measures; it does not feed the durable store. Clean up on
    // failure too, so a half-written archive is not left in scratch.
    for path in command.output_files(scratch_dir) {
        std::fs::remove_file(path).ok();
    }

    result.map(|_| ())
}
