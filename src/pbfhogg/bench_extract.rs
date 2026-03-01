//! Benchmark: extract strategies (simple/complete/smart) with bbox.

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

pub const ALL_STRATEGIES: &[&str] = &["simple", "complete", "smart"];

pub fn parse_strategies(input: &str) -> Result<Vec<&'static str>, DevError> {
    let mut out = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        let found = ALL_STRATEGIES.iter().find(|&&s| s == part);
        match found {
            Some(&s) => out.push(s),
            None => return Err(DevError::Config(format!("unknown strategy: {part}"))),
        }
    }
    Ok(out)
}

fn strategy_args(name: &str, pbf: &str, bbox: &str) -> Vec<String> {
    match name {
        "simple" => vec!["extract".into(), pbf.into(), "--simple".into(), "-b".into(), bbox.into(), "-o".into(), "/dev/null".into()],
        "complete" => vec!["extract".into(), pbf.into(), "-b".into(), bbox.into(), "-o".into(), "/dev/null".into()],
        "smart" => vec!["extract".into(), pbf.into(), "--smart".into(), "-b".into(), bbox.into(), "-o".into(), "/dev/null".into()],
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
) -> Result<(), DevError> {
    let (basename, pbf_str) = super::path_strs(pbf_path)?;

    for &name in strategies {
        output::bench_msg(&format!("strategy: {name}"));
        let args = strategy_args(name, pbf_str, bbox);
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: "bench extract".into(),
            variant: Some(name.into()),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs,
            cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &args_refs)),
            metadata: vec![KvPair::text("meta.strategy", name), KvPair::text("meta.bbox", bbox)],
        };

        harness.run_external(&config, binary, &args_refs, project_root)?;
    }

    Ok(())
}
