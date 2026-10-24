//! Combined elivagar benchmark suite: self, planetiler, node-store, pmtiles, tilemaker.

use std::path::Path;

use crate::build;
use crate::clean::layout::ElivagarLayout;
use crate::config::ResolvedPaths;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult, WallSource};
use crate::output;

use super::commands::{BuiltCommand, ExampleCommand};
use super::{bench_planetiler, bench_tilemaker};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    paths: &ResolvedPaths,
    features: &[String],
    project_root: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    tilegen: &crate::config::project_config::elivagar::TilegenConfig,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    let layout = ElivagarLayout::new(paths);
    // 1. bench self -- full elivagar pipeline
    output::bench_msg("=== bench self ===");
    let built = build::cargo_build(&super::build_config(features), project_root)?;
    harness.note_build(crate::harness::BuildIdentity::of(&built));
    let binary = built.path;
    run_tilegen(
        harness,
        &binary,
        pbf_path,
        file_mb,
        runs,
        &layout,
        project_root,
        tilegen,
        opts,
    )?;

    // The two external baselines no longer downgrade a failure to "skipped".
    // A missing JDK and a crash mid-measurement both used to print one line
    // and let the suite exit 0, so a suite run could silently lack its
    // comparison rows. The remaining arms still run (one broken external tool
    // should not cost the others' numbers), and the suite fails at the end.
    let mut failures: Vec<String> = Vec::new();

    // 2. bench planetiler -- comparison baseline
    output::bench_msg("=== bench planetiler ===");
    if let Err(e) = bench_planetiler::run(harness, pbf_path, file_mb, runs, &layout, project_root) {
        if matches!(e, DevError::Interrupted) {
            return Err(e);
        }
        output::error(&format!("planetiler failed: {e}"));
        failures.push("planetiler".into());
    }

    // 3. bench node-store -- micro-benchmark
    output::bench_msg("=== bench node-store ===");
    run_example(
        harness,
        project_root,
        &layout,
        features,
        ExampleCommand::NodeStore {
            nodes: super::constants::DEFAULT_NODE_STORE_MILLIONS,
        },
        runs,
    )?;

    // 4. bench pmtiles -- micro-benchmark
    output::bench_msg("=== bench pmtiles ===");
    run_example(
        harness,
        project_root,
        &layout,
        features,
        ExampleCommand::PmtilesWriter {
            tiles: super::constants::DEFAULT_PMTILES_BENCH_TILES,
        },
        runs,
    )?;

    // 5. bench tilemaker -- comparison baseline
    output::bench_msg("=== bench tilemaker ===");
    if let Err(e) = bench_tilemaker::run(harness, pbf_path, file_mb, runs, &layout, project_root) {
        if matches!(e, DevError::Interrupted) {
            return Err(e);
        }
        output::error(&format!("tilemaker failed: {e}"));
        failures.push("tilemaker".into());
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(DevError::Reported(format!(
            "bench all: {} failed",
            failures.join(", ")
        )))
    }
}

#[allow(clippy::too_many_arguments)]
fn run_tilegen(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    layout: &ElivagarLayout<'_>,
    project_root: &Path,
    tilegen: &crate::config::project_config::elivagar::TilegenConfig,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
    let basename = pbf_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned();
    let command = BuiltCommand::MainBinary {
        tilegen,
        skip_to: None,
    };
    let args = command.build_args(pbf_str, layout, Some(opts))?;
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    output::bench_msg(&format!(
        "elivagar pipeline: {basename} ({file_mb:.0} MB), {runs} run(s)"
    ));

    let config = BenchConfig {
        command: command.id().into(),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: None,
        runs,
        cli_args: Some(crate::harness::format_cli_args(
            &binary.display().to_string(),
            &refs,
        )),
        metadata: vec![],
    };
    // `bench all`'s self arm only measures output; clear any artifact left by
    // an earlier interrupted run before starting this sample.
    cleanup_outputs(&command, layout)?;
    let measurement = harness.measure_external(
        &config,
        binary,
        &refs,
        project_root,
        &[],
        WallSource::External,
        || Ok(()),
    );
    let result = match measurement {
        Ok(mut measurement) => {
            for iteration in &mut measurement.iterations {
                if let Some(result) = &mut iteration.result {
                    result.kv.push(KvPair::text(
                        "meta.locations_on_ways_detected",
                        super::detect_locations_on_ways_stderr(&iteration.stderr).to_string(),
                    ));
                }
            }
            harness.record(&config, &mut measurement, |_, _| Ok(()))
        }
        Err(error) => Err(error),
    };

    // Remove partial output after either success or failure. This arm never
    // promotes an archive into the durable store.
    let cleanup = cleanup_outputs(&command, layout);
    match (result, cleanup) {
        (Ok(_), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(run_error), Err(cleanup_error)) => Err(DevError::Reported(format!(
            "{run_error}; output cleanup failed: {cleanup_error}"
        ))),
    }
}

fn cleanup_outputs(
    command: &BuiltCommand<'_>,
    layout: &ElivagarLayout<'_>,
) -> Result<(), DevError> {
    for path in command.output_files(layout) {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn run_example(
    harness: &BenchHarness,
    project_root: &Path,
    layout: &ElivagarLayout<'_>,
    features: &[String],
    example: ExampleCommand,
    runs: usize,
) -> Result<(), DevError> {
    let command = BuiltCommand::Example(example);
    let mut build = super::build_config(features);
    build.example = Some(example.cargo_example().into());
    let built = build::cargo_build(&build, project_root)?;
    harness.note_build(crate::harness::BuildIdentity::of(&built));
    let binary = built.path;
    let mut args = command.build_args("", layout, None)?;
    let run_arg = args
        .last_mut()
        .ok_or_else(|| DevError::Internal("example command has no --runs argument".into()))?;
    *run_arg = runs.to_string();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let program = binary.display().to_string();
    let config = BenchConfig {
        command: example.id().into(),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        external_build: None,
        runs: 1,
        cli_args: None,
        metadata: vec![],
    };

    harness.run_internal(&config, |_| {
        let captured = output::run_captured(&program, &refs, project_root)?;
        if !captured.stdout.is_empty() {
            output::plain(&String::from_utf8_lossy(&captured.stdout));
        }
        if !captured.stderr.is_empty() {
            output::plain_stderr(&String::from_utf8_lossy(&captured.stderr));
        }
        captured.check_success(&program)?;

        let (key, value) = match example {
            ExampleCommand::PmtilesWriter { tiles } => ("tiles", tiles),
            ExampleCommand::NodeStore { nodes } => ("nodes_millions", nodes),
        };
        Ok(BenchResult {
            elapsed_ms: crate::harness::elapsed_to_ms(&captured.elapsed),
            elapsed_us: None,
            kv: vec![
                KvPair::int(key, i64::try_from(value).unwrap_or(i64::MAX)),
                KvPair::int("internal_runs", i64::try_from(runs).unwrap_or(i64::MAX)),
            ],
            iterations: Vec::new(),
            distribution: None,
            hotpath: None,
        })
    })?;
    Ok(())
}
