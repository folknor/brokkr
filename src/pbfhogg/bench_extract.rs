//! Benchmark: extract strategies (simple/complete/smart) with bbox.

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};

pub const ALL_STRATEGIES: &[&str] = &["simple", "complete", "smart"];

fn strategy_args(name: &str, pbf: &str, bbox: &str, output: &str) -> Vec<String> {
    match name {
        "simple" => vec![
            "extract".into(),
            pbf.into(),
            "--simple".into(),
            "-b".into(),
            bbox.into(),
            "-o".into(),
            output.into(),
        ],
        "complete" => vec![
            "extract".into(),
            pbf.into(),
            "-b".into(),
            bbox.into(),
            "-o".into(),
            output.into(),
        ],
        "smart" => vec![
            "extract".into(),
            pbf.into(),
            "--smart".into(),
            "-b".into(),
            bbox.into(),
            "-o".into(),
            output.into(),
        ],
        _ => unreachable!("unknown strategy: {name}"),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    bbox: &str,
    strategies: &[&str],
    project_root: &Path,
    scratch_dir: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;
    let output_path = scratch_dir.join("bench-extract-output.osm.pbf");
    let output_str = output_path.display().to_string();

    let (basename, pbf_str) = super::path_strs(pbf_path)?;

    let result = crate::harness::run_variants(strategies, |name| {
        let args = strategy_args(name, pbf_str, bbox, &output_str);
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: "bench extract".into(),
            variant: Some(name.into()),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs,
            cli_args: Some(crate::harness::format_cli_args(
                &binary.display().to_string(),
                &args_refs,
            )),
            metadata: vec![
                KvPair::text("meta.strategy", name),
                KvPair::text("meta.bbox", bbox),
            ],
        };

        harness.run_external(&config, binary, &args_refs, project_root).map(|_| ())
    });

    std::fs::remove_file(&output_path).ok();

    result
}
