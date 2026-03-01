//! Benchmark: merge a base PBF with an OSC diff (subprocess).

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

/// Run the merge benchmark for each compression x I/O variant.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    osc_path: &Path,
    file_mb: f64,
    runs: usize,
    compressions: &[String],
    uring: bool,
    scratch_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;

    let output_path = scratch_dir.join("bench-merge-output.osm.pbf");
    let (basename, pbf_str) = super::path_strs(pbf_path)?;
    let osc_str = osc_path
        .to_str()
        .ok_or_else(|| DevError::Config("OSC path is not valid UTF-8".into()))?;
    let output_str = output_path.display().to_string();

    let io_modes: Vec<&str> = if uring {
        vec!["buffered", "uring", "uring-sqpoll"]
    } else {
        vec!["buffered"]
    };

    for compression in compressions {
        for io_mode in &io_modes {
            let variant = format!("{io_mode}+{compression}");
            output::bench_msg(&format!("variant: {variant}"));

            let bench_args: Vec<&str> = vec![
                "bench-merge", pbf_str, osc_str,
                "-o", &output_str,
                "--compression", compression,
                "--io-mode", io_mode,
            ];

            let config = BenchConfig {
                command: "bench merge".into(),
                variant: Some(variant),
                input_file: Some(basename.clone()),
                input_mb: Some(file_mb),
                cargo_features: None,
                cargo_profile: "release".into(),
                runs,
                cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &bench_args)),
                metadata: Some(serde_json::json!({
                    "compression": compression,
                    "io_mode": io_mode,
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

    // Clean up
    std::fs::remove_file(&output_path).ok();

    Ok(())
}
