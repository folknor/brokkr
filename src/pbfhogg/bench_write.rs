//! Benchmark: decode + write all elements through BlockBuilder (subprocess).

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};

/// Run the write benchmark for each compression mode (sync + pipelined).
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    compressions: &[String],
    project_root: &Path,
) -> Result<(), DevError> {
    let (basename, pbf_str) = super::path_strs(pbf_path)?;

    let variant_names: Vec<String> = compressions
        .iter()
        .flat_map(|c| vec![format!("sync-{c}"), format!("pipelined-{c}")])
        .collect();
    let variant_refs: Vec<&str> = variant_names.iter().map(String::as_str).collect();

    crate::harness::run_variants("variant", &variant_refs, |variant| {
        // Parse "writer_mode-compression" back out.
        let (writer_mode, compression) = variant.split_once('-').unwrap_or(("sync", variant));

        let bench_args: Vec<&str> = vec![
            "bench-write",
            pbf_str,
            "--compression",
            compression,
            "--writer",
            writer_mode,
        ];

        let config = BenchConfig {
            command: "write".into(),
            // Writer-mode/compression discriminators are in cli_args
            // (`--writer`, `--compression`). Measurement mode and
            // brokkr_args come from the harness.
            mode: None,
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: crate::build::CargoProfile::Release,
            runs,
            cli_args: Some(crate::harness::format_cli_args(
                &binary.display().to_string(),
                &bench_args,
            )),
            brokkr_args: None,
            metadata: vec![],
        };

        harness.run_external_with_kv(&config, binary, &bench_args, project_root).map(|_| ())
    })
}
