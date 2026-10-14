//! Benchmark: extract strategies (simple/complete/smart) with a dataset bbox.

use std::path::Path;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::measure::{CommandContext, CommandParams};
use crate::pbfhogg::commands::{ArgMode, ExtractStrategy, PbfhoggCommand, scratch_output_path};
use crate::resolve::Bbox;

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    bbox: &Bbox,
    project_root: &Path,
    scratch_dir: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;
    let command_ctx = CommandContext {
        binary: binary.to_path_buf(),
        pbf_path: Some(pbf_path.to_path_buf()),
        osc_path: None,
        osc_paths: Vec::new(),
        pbf_b_path: None,
        scratch_dir: scratch_dir.to_path_buf(),
        dataset: String::new(),
        bbox: Some(bbox.clone()),
        params: CommandParams::default(),
    };
    let output_path = scratch_output_path(
        &command_ctx,
        &PbfhoggCommand::Extract {
            strategy: ExtractStrategy::Simple,
        },
        ArgMode::Bench,
    )?;

    let (basename, _) = super::path_strs(pbf_path)?;
    let strategy_names: Vec<&str> = ExtractStrategy::all()
        .iter()
        .map(|strategy| strategy.name())
        .collect();

    let result = crate::harness::run_variants("strategy", &strategy_names, |name| {
        let strategy = ExtractStrategy::parse(name)?;
        let command = PbfhoggCommand::Extract { strategy };
        let args = command.build_args(&command_ctx, ArgMode::Bench)?;
        let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let config = BenchConfig {
            command: command.result_command(),
            // Strategy and bbox remain in cli_args, preserving the existing
            // result-row shape for dataset-configured extract measurements.
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            external_build: None,
            runs,
            cli_args: Some(crate::harness::format_cli_args(
                &binary.display().to_string(),
                &args_refs,
            )),
            metadata: vec![],
        };

        harness
            .run_external(&config, binary, &args_refs, project_root)
            .map(|_| ())
    });

    std::fs::remove_file(&output_path).ok();

    result
}
