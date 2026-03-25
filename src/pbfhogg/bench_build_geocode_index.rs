//! Benchmark: build-geocode-index command.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    scratch_dir: &Path,
    dataset: &str,
    project_root: &Path,
) -> Result<(), DevError> {
    let (basename, pbf_str) = super::path_strs(pbf_path)?;

    let output_dir = scratch_dir.join(format!("geocode-{dataset}"));
    let output_dir_str = output_dir
        .to_str()
        .ok_or_else(|| DevError::Config("output dir path is not valid UTF-8".into()))?;

    std::fs::create_dir_all(scratch_dir).map_err(|e| {
        DevError::Config(format!("failed to create scratch dir: {e}"))
    })?;

    output::bench_msg(&format!("build-geocode-index → {output_dir_str}"));

    let args: Vec<&str> = vec![
        "build-geocode-index",
        pbf_str,
        "--output-dir",
        output_dir_str,
        "--force",
    ];

    let config = BenchConfig {
        command: "bench build-geocode-index".into(),
        variant: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &args)),
        metadata: vec![],
    };

    harness.run_external(&config, binary, &args, project_root)?;

    Ok(())
}
