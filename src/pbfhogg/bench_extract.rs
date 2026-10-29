//! Benchmark: extract strategies (simple/complete/smart) with a dataset bbox.

use std::path::Path;

use crate::datasets::Bbox;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::pbfhogg::binary::PbfhoggBinary;
use crate::pbfhogg::commands::{ArgMode, ExtractStrategy, PbfhoggCommand, scratch_output_path};
use crate::pbfhogg::context::{CommandContext, CommandObservations, CommandParams, ResolvedInputs};

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &PbfhoggBinary,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    bbox: &Bbox,
    project_root: &Path,
    scratch_dir: &Path,
) -> Result<(), DevError> {
    std::fs::create_dir_all(scratch_dir)?;
    let command_ctx = CommandContext {
        inputs: ResolvedInputs::PbfAndBbox {
            pbf: pbf_path.to_path_buf(),
            bbox: bbox.clone(),
        },
        scratch_dir: scratch_dir.to_path_buf(),
        dataset: String::new(),
        // The bbox comes from the dataset (no explicit `params.bbox`) and one
        // suite argv covers every strategy, so both ride in the qualifier.
        params: CommandParams {
            strategy_fanout: true,
            ..CommandParams::default()
        },
        observations: CommandObservations::default(),
    };
    let output_path = scratch_output_path(
        &command_ctx,
        &PbfhoggCommand::Extract {
            strategy: ExtractStrategy::Simple,
        },
        ArgMode::Bench,
    )?;

    let (basename, _) = crate::utf8_path::path_strings(pbf_path)?;
    let strategy_names: Vec<&str> = ExtractStrategy::all()
        .iter()
        .map(|strategy| strategy.name())
        .collect();

    let result = crate::bench_variants::run_variants("strategy", &strategy_names, |name| {
        let strategy = ExtractStrategy::parse(name)?;
        let command = PbfhoggCommand::Extract { strategy };
        // Every strategy and every iteration writes the same output path, so
        // the argv carries the negotiated overwrite flag.
        let invocation = binary.invocation(command.build_args(&command_ctx, ArgMode::Bench)?)?;

        let config = BenchConfig {
            command: command.id().to_owned(),
            // Strategy and bbox reach the row's cli_args through the
            // invocation; pairing never reads cli_args, so they also ride in
            // the arm qualifier.
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            external_build: None,
            runs,
            cli_args: None,
            metadata: vec![],
            arm_qualifier: command.arm_qualifier(&command_ctx),
        };

        harness
            .run_external(&config, &invocation, project_root)
            .map(|_| ())
    });

    crate::reservations::remove_file(&output_path).ok();

    result
}
