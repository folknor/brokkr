//! Benchmark: compare allocators (default, jemalloc, mimalloc) via check --refs.

use std::path::Path;

use crate::build::{self, BuildConfig};
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};

pub const ALL_ALLOCATORS: &[&str] = &["default", "jemalloc", "mimalloc"];

pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    project_root: &Path,
) -> Result<(), DevError> {
    let (basename, pbf_str) = super::path_strs(pbf_path)?;

    crate::harness::run_variants("allocator", ALL_ALLOCATORS, |name| {
        let build_config = match name {
            "jemalloc" => BuildConfig::release_with_features(Some("pbfhogg-cli"), &["jemalloc"]),
            "mimalloc" => BuildConfig::release_with_features(Some("pbfhogg-cli"), &["mimalloc"]),
            _ => BuildConfig::release(Some("pbfhogg-cli")),
        };

        let binary = build::cargo_build(&build_config, project_root)?;
        let args: Vec<&str> = vec!["check", "--refs", pbf_str];

        let features_label = match name {
            "jemalloc" => Some("jemalloc".into()),
            "mimalloc" => Some("mimalloc".into()),
            _ => None,
        };

        let config = BenchConfig {
            command: "allocator".into(),
            // Allocator discriminator (jemalloc/mimalloc/default) is in
            // the `cargo_features` column. Measurement mode and
            // brokkr_args come from the harness.
            mode: None,
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: features_label,
            cargo_profile: crate::build::CargoProfile::Release,
            runs,
            cli_args: Some(crate::harness::format_cli_args(
                &binary.display().to_string(),
                &args,
            )),
            brokkr_args: None,
            metadata: vec![],
        };

        harness.run_external(&config, &binary, &args, project_root).map(|_| ())
    })
}
