//! Benchmark: decode + write all elements through BlockBuilder (subprocess).

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

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

    for compression in compressions {
        for writer_mode in &["sync", "pipelined"] {
            let variant = format!("{writer_mode}-{compression}");
            output::bench_msg(&format!("variant: {variant}"));

            let bench_args: Vec<&str> = vec!["bench-write", pbf_str, "--compression", compression, "--writer", writer_mode];

            let config = BenchConfig {
                command: "bench write".into(),
                variant: Some(variant),
                input_file: Some(basename.clone()),
                input_mb: Some(file_mb),
                cargo_features: None,
                cargo_profile: "release".into(),
                runs,
                cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &bench_args)),
                metadata: Some(serde_json::json!({
                    "compression": compression,
                    "writer_mode": writer_mode,
                })),
            };

            harness.run_external_with_kv(
                &config,
                binary,
                &bench_args,
                project_root,
            )?;
        }
    }

    Ok(())
}
