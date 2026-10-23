//! Benchmark: compare allocators (default, jemalloc, mimalloc) via check --refs.

use std::path::Path;

use crate::build::{self, BuildConfig};
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::pbfhogg::binary::{PbfhoggBinary, Writes};
use crate::project::Project;

pub const ALL_ALLOCATORS: &[&str] = &["default", "jemalloc", "mimalloc"];

pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    project_root: &Path,
) -> Result<(), DevError> {
    let (basename, pbf_str) = crate::utf8_path::path_strings(pbf_path)?;

    crate::bench_variants::run_variants("allocator", ALL_ALLOCATORS, |name| {
        let build_config = match name {
            "jemalloc" => {
                BuildConfig::release_with_features(Project::Pbfhogg.cli_package(), &["jemalloc"])
            }
            "mimalloc" => {
                BuildConfig::release_with_features(Project::Pbfhogg.cli_package(), &["mimalloc"])
            }
            _ => BuildConfig::release(Project::Pbfhogg.cli_package()),
        };

        // Each allocator is its own build at the same path, so each gets its
        // own handle, used only until the next variant builds.
        let built = build::cargo_build(&build_config, project_root)?.path;
        let binary = PbfhoggBinary::new(&built, project_root)?;
        let invocation = binary.invocation_strs(&["check", "--refs", pbf_str], Writes::Nothing)?;
        let args = invocation.arg_refs();

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
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: features_label,
            external_build: None,
            runs,
            cli_args: Some(invocation.cli_args()),
            metadata: vec![],
        };

        harness
            .run_external(&config, invocation.program(), &args, project_root)
            .map(|_| ())
    })
}
