//! Benchmark: count all elements using each pbfhogg read mode (subprocess).

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

/// Read modes.
#[derive(Debug, Clone, Copy)]
pub enum ReadMode {
    Sequential,
    Parallel,
    Pipelined,
    Mmap,
    BlobReader,
}

impl ReadMode {
    pub fn name(self) -> &'static str {
        match self {
            ReadMode::Sequential => "sequential",
            ReadMode::Parallel => "parallel",
            ReadMode::Pipelined => "pipelined",
            ReadMode::Mmap => "mmap",
            ReadMode::BlobReader => "blobreader",
        }
    }
}

pub const ALL_MODES: &[ReadMode] = &[
    ReadMode::Sequential,
    ReadMode::Parallel,
    ReadMode::Pipelined,
    ReadMode::Mmap,
    ReadMode::BlobReader,
];

/// Parse comma-separated mode names.
pub fn parse_modes(input: &str) -> Result<Vec<ReadMode>, DevError> {
    let mut modes = Vec::new();
    for token in input.split(',') {
        let trimmed = token.trim();
        let mode = match trimmed.to_ascii_lowercase().as_str() {
            "sequential" => ReadMode::Sequential,
            "parallel" => ReadMode::Parallel,
            "pipelined" => ReadMode::Pipelined,
            "mmap" => ReadMode::Mmap,
            "blobreader" => ReadMode::BlobReader,
            _ => return Err(DevError::Config(format!("unknown read mode: {trimmed}"))),
        };
        modes.push(mode);
    }
    Ok(modes)
}

/// Run the read benchmark for each requested mode via subprocess.
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    modes: &[ReadMode],
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

    for &mode in modes {
        output::bench_msg(&format!("mode: {}", mode.name()));

        let bench_args: Vec<&str> = vec!["bench-read", pbf_str, "--mode", mode.name()];

        let config = BenchConfig {
            command: "bench read".into(),
            variant: Some(mode.name().into()),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs,
            cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &bench_args)),
            metadata: vec![KvPair::text("meta.mode", mode.name())],
        };

        harness.run_external_with_kv(
            &config,
            binary,
            &bench_args,
            project_root,
        )?;
    }

    Ok(())
}
