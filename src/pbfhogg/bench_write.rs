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
    compressions: &[(String, String)],
    project_root: &Path,
) -> Result<(), DevError> {
    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    for (label, spec) in compressions {
        for writer_mode in &["sync", "pipelined"] {
            let variant = format!("{writer_mode}-{label}");
            output::bench_msg(&format!("variant: {variant}"));

            let config = BenchConfig {
                command: "bench write".into(),
                variant: Some(variant),
                input_file: Some(basename.clone()),
                input_mb: Some(file_mb),
                cargo_features: Some("zlib-ng".into()),
                cargo_profile: "release".into(),
                runs,
            };

            harness.run_external_with_kv(
                &config,
                binary,
                &["bench-write", pbf_str, "--compression", spec, "--writer", writer_mode],
                project_root,
            )?;
        }
    }

    Ok(())
}
