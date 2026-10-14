//! Benchmark: merge a base PBF with an OSC diff (subprocess).

use std::path::Path;

use crate::clean::layout::PbfhoggLayout;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};

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

    let output_path = PbfhoggLayout::fixed_scratch_file(scratch_dir, "bench-merge-output.osm.pbf");
    let (basename, pbf_str) = crate::utf8_path::path_strings(pbf_path)?;
    let osc_str = crate::utf8_path::path_str(osc_path)?;
    let output_str = crate::utf8_path::path_str(&output_path)?;

    let io_modes: Vec<&str> = if uring {
        vec!["buffered", "uring"]
    } else {
        vec!["buffered"]
    };

    let variant_names: Vec<String> = compressions
        .iter()
        .flat_map(|c| io_modes.iter().map(move |io| format!("{io}+{c}")))
        .collect();
    let variant_refs: Vec<&str> = variant_names.iter().map(String::as_str).collect();

    let result = crate::bench_variants::run_variants("variant", &variant_refs, |variant| {
        let (io_mode, compression) = variant.split_once('+').unwrap_or(("buffered", variant));

        let bench_args: Vec<&str> = vec![
            "bench-merge",
            pbf_str,
            osc_str,
            "-o",
            output_str,
            "--compression",
            compression,
            "--io-mode",
            io_mode,
        ];

        let config = BenchConfig {
            command: "merge".into(),
            // io_mode / compression discriminators are in cli_args.
            // Measurement mode and brokkr_args come from the harness.
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            external_build: None,
            runs,
            cli_args: Some(crate::harness::format_cli_args(
                &binary.display().to_string(),
                &bench_args,
            )),
            metadata: vec![],
        };

        harness
            .run_external_with_kv(&config, binary, &bench_args, project_root)
            .map(|_| ())
    });

    // Clean up
    std::fs::remove_file(&output_path).ok();

    result
}
