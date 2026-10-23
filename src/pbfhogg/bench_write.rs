//! Benchmark: decode + write all elements through BlockBuilder (subprocess).

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::pbfhogg::binary::{PbfhoggBinary, Writes};

/// Run the write benchmark for each compression mode (sync + pipelined).
pub fn run(
    harness: &BenchHarness,
    binary: &PbfhoggBinary,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    compressions: &[String],
    project_root: &Path,
) -> Result<(), DevError> {
    let (basename, pbf_str) = crate::utf8_path::path_strings(pbf_path)?;

    let variant_names: Vec<String> = compressions
        .iter()
        .flat_map(|c| vec![format!("sync-{c}"), format!("pipelined-{c}")])
        .collect();
    let variant_refs: Vec<&str> = variant_names.iter().map(String::as_str).collect();

    crate::bench_variants::run_variants("variant", &variant_refs, |variant| {
        // Parse "writer_mode-compression" back out.
        let (writer_mode, compression) = variant.split_once('-').unwrap_or(("sync", variant));

        // bench-write encodes in memory and names no output path.
        let invocation = binary.invocation_strs(
            &[
                "bench-write",
                pbf_str,
                "--compression",
                compression,
                "--writer",
                writer_mode,
            ],
            Writes::Nothing,
        )?;

        let config = BenchConfig {
            command: "write".into(),
            // Writer-mode/compression discriminators are in cli_args
            // (`--writer`, `--compression`). Measurement mode and
            // brokkr_args come from the harness.
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            external_build: None,
            runs,
            cli_args: None,
            metadata: vec![],
        };

        harness
            .run_external_with_kv(&config, &invocation, project_root)
            .map(|_| ())
    })
}
