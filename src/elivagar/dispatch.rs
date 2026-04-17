//! Unified dispatch layer for elivagar commands.

use crate::context::BenchContext;
use crate::db::KvPair;
use crate::elivagar;
use crate::elivagar::commands::ElivagarCommand;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::oom;
use crate::output;
use crate::project::{self, Project};
use crate::resolve::resolve_pbf_with_size;

/// Run an elivagar command with the specified measurement mode.
///
/// Handles run, bench, hotpath, and alloc for any elivagar command.
/// External commands (Planetiler, Tilemaker) delegate to old handlers for
/// bench mode but do not support run/hotpath/alloc.
pub(crate) fn run_command(
    req: &MeasureRequest,
    command: &ElivagarCommand,
) -> Result<(), DevError> {
    project::require(req.project, Project::Elivagar, command.id())?;

    if req.dry_run {
        return Err(DevError::Config(
            "--dry-run is not yet supported for elivagar commands".into(),
        ));
    }

    // External tools (Planetiler, Tilemaker) keep their old dispatch path.
    if command.is_external() {
        return run_elivagar_external(req, command);
    }

    match req.mode {
        MeasureMode::Run => run_elivagar_run(req, command),
        MeasureMode::Bench { .. } => run_elivagar_bench(req, command),
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            run_elivagar_hotpath(req, command)
        }
    }
}

/// Elivagar run mode: build, run once, print timing. No DB storage.
///
/// Uses `ElivagarCommand::build_config()` to determine build type and
/// `ElivagarCommand::build_args()` for argument construction.
fn run_elivagar_run(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    use crate::elivagar::commands::BuildKind;

    match command.build_config() {
        BuildKind::MainBinary => {
            let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
            // Run mode never stores results, so dirty tree is always fine.
            let ctx = BenchContext::new(
                req.dev_config,
                req.project,
                req.project_root,
                req.build_root,
                command.package(),
                &feat_refs,
                true,
                &format!("run {}", command.id()),
                true,
                req.wait,
                req.stop_marker.map(str::to_owned),
            )?
            .with_request(req);

            let pbf_str = if command.needs_pbf() {
                let (p, _) =
                    resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
                p.to_str()
                    .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?
                    .to_owned()
            } else {
                String::new()
            };

            let args = command.build_args(&pbf_str, &ctx.paths.scratch_dir, &ctx.paths.data_dir)?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let binary_str = ctx.binary.display().to_string();
            output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

            let out = output::run_passthrough_timed(&binary_str, &arg_refs)?;

            if out.code != 0 {
                for path in command.output_files(&ctx.paths.scratch_dir) {
                    std::fs::remove_file(path).ok();
                }
                return Err(DevError::ExitCode(out.code));
            }

            rename_elivagar_output(command, &ctx.paths.scratch_dir, req.dataset, req.project_root);

            let ms = crate::duration_ms(out.elapsed);
            output::run_msg(&format!("elapsed={ms}ms"));
            Ok(())
        }
        BuildKind::Example(example) => {
            let build_root = req.build_root.unwrap_or(req.project_root);
            let binary = crate::build::cargo_build(
                &crate::build::BuildConfig {
                    package: None,
                    bin: None,
                    example: Some(example.into()),
                    features: vec![],
                    default_features: true,
                    profile: "release",
                },
                build_root,
            )?;
            let binary_str = binary.display().to_string();

            let args =
                command.build_args("", std::path::Path::new(""), std::path::Path::new(""))?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

            let out = output::run_passthrough_timed(&binary_str, &arg_refs)?;

            if out.code != 0 {
                return Err(DevError::ExitCode(out.code));
            }

            let ms = crate::duration_ms(out.elapsed);
            output::run_msg(&format!("elapsed={ms}ms"));
            Ok(())
        }
        BuildKind::NoBuild => Err(DevError::Config(format!(
            "command '{}' does not support run mode",
            command.id(),
        ))),
    }
}

/// Elivagar bench mode: full harness with DB storage.
///
/// Uses `ElivagarCommand::build_config()` to determine how to build and
/// `ElivagarCommand::build_args()` to get the argument vector. Tilegen uses
/// `run_external_with_kv` (parses kv from stderr); micro-benchmarks use
/// `run_internal` (examples handle their own iteration).
fn run_elivagar_bench(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    use crate::elivagar::commands::BuildKind;

    match command.build_config() {
        BuildKind::MainBinary => run_elivagar_wallclock(req, command),
        BuildKind::Example(_) => run_elivagar_internal(req, command),
        BuildKind::NoBuild => Err(DevError::Config(format!(
            "command '{}' does not support bench mode via this path",
            command.id(),
        ))),
    }
}

/// Elivagar wallclock benchmark for the main binary (Tilegen).
///
/// Builds the main binary, runs via `run_external_with_kv`, parses kv metrics
/// from stderr, detects `locations_on_ways`, and stores results.
fn run_elivagar_wallclock(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        command.package(),
        &feat_refs,
        true,
        &format!("bench {}", command.id()),
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let (pbf_path, file_mb) = if command.needs_pbf() {
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?
    } else {
        (std::path::PathBuf::new(), 0.0)
    };
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let args = command.build_args(pbf_str, &ctx.paths.scratch_dir, &ctx.paths.data_dir)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!(
        "{} ({file_mb:.0} MB), {} run(s)",
        command.id(),
        req.runs(),
    ));

    let mut bench_config = BenchConfig {
        command: command.result_command().into(),
        mode: None,
        input_file: if command.needs_pbf() {
            Some(basename)
        } else {
            None
        },
        input_mb: if command.needs_pbf() {
            Some(file_mb)
        } else {
            None
        },
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &arg_refs,
        )),
        brokkr_args: None,
        metadata: command.metadata(),
    };

    // Use kv parsing: elivagar emits timing/metrics to stderr.
    // Sidecar monitoring runs automatically via run_external_with_kv_raw.
    let (result, stderr) = ctx.harness.run_external_with_kv_raw(
        &bench_config,
        &ctx.binary,
        &arg_refs,
        req.project_root,
    )?;
    let detected = elivagar::detect_locations_on_ways_stderr(&stderr);
    bench_config.metadata.push(KvPair::text(
        "meta.locations_on_ways_detected",
        detected.to_string(),
    ));
    ctx.harness.record_result(&bench_config, &result)?;

    rename_elivagar_output(command, &ctx.paths.scratch_dir, req.dataset, req.project_root);

    Ok(())
}

/// Elivagar internal benchmark for cargo examples (PmtilesWriter, NodeStore).
///
/// Builds the example binary, runs via `run_internal` (the example handles
/// its own iteration), and stores results. The harness does 1 external run
/// while the example does N internal runs.
fn run_elivagar_internal(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    use crate::context::HarnessContext;

    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        &format!("bench {}", command.id()),
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let example = command.example().ok_or_else(|| {
        DevError::Config(format!("command '{}' has no cargo example", command.id()))
    })?;

    let build_root = req.build_root.unwrap_or(req.project_root);
    let binary = crate::build::cargo_build(
        &crate::build::BuildConfig {
            package: None,
            bin: None,
            example: Some(example.into()),
            features: vec![],
            default_features: true,
            profile: "release",
        },
        build_root,
    )?;
    let binary_str = binary.display().to_string();

    // build_args returns ["--tiles", "500000", "--runs", "1"] or similar.
    // We pass an empty pbf_str and dummy paths since examples don't need them.
    let args = command.build_args("", std::path::Path::new(""), std::path::Path::new(""))?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!("{}, {} run(s)", command.id(), req.runs()));

    let config = BenchConfig {
        command: command.result_command().into(),
        mode: None,
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs: 1, // example handles its own iterations
        cli_args: None,
        brokkr_args: None,
        metadata: command.metadata(),
    };

    ctx.harness.run_internal(&config, |_i| {
        let captured = output::run_captured(&binary_str, &arg_refs, build_root)?;
        captured.check_success(&binary_str)?;
        let ms = harness::elapsed_to_ms(&captured.elapsed);
        Ok(crate::harness::BenchResult {
            elapsed_ms: ms,
            kv: vec![],
            distribution: None,
            hotpath: None,
        })
    })?;

    Ok(())
}

/// Elivagar hotpath/alloc: build with hotpath feature, run with instrumentation.
///
/// Uses `ElivagarCommand::build_config()` to determine build type:
/// - MainBinary (Tilegen): `BenchContext::new` with hotpath features, `build_args()`,
///   OOM check, locations_on_ways detection, output cleanup.
/// - Example (PmtilesWriter, NodeStore): `cargo_build` with example + hotpath feature,
///   `build_args()`, `run_hotpath_capture`.
///
/// Both paths go through `run_hotpath_capture` for JSON report collection.
#[allow(clippy::too_many_lines)]
fn run_elivagar_hotpath(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    use crate::elivagar::commands::BuildKind;

    if !command.supports_hotpath() {
        return Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        )));
    }

    let alloc = req.is_alloc();
    let feature = harness::hotpath_feature(alloc);

    output::hotpath_msg(&format!("=== {} {feature} ===", command.id()));
    if alloc {
        output::hotpath_msg("NOTE: alloc profiling — wall-clock times are not meaningful");
    }

    match command.build_config() {
        BuildKind::MainBinary => {
            // Tilegen: build main binary with hotpath features, resolve PBF, OOM check.
            let hotpath_features = req.hotpath_features();
            let ctx = BenchContext::new(
                req.dev_config,
                req.project,
                req.project_root,
                req.build_root,
                command.package(),
                &hotpath_features,
                true,
                &format!("hotpath {}", command.id()),
                req.force,
                req.wait,
                req.stop_marker.map(str::to_owned),
            )?
            .with_request(req);

            let (pbf_path, file_mb) =
                resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
            let risk = if alloc {
                oom::MemoryRisk::AllocTracking
            } else {
                oom::MemoryRisk::Normal
            };
            oom::check_memory(file_mb, &risk, req.no_mem_check)?;

            let pbf_str = pbf_path
                .to_str()
                .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

            let basename = pbf_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();

            let args = command.build_args(pbf_str, &ctx.paths.scratch_dir, &ctx.paths.data_dir)?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let binary_str = ctx.binary.display().to_string();

            let metadata = command.metadata();

            let config = BenchConfig {
                command: command.result_command().into(),
                mode: None,
                input_file: Some(basename),
                input_mb: Some(file_mb),
                cargo_features: None,
                cargo_profile: crate::build::CargoProfile::Release,
                runs: req.runs(),
                cli_args: Some(harness::format_cli_args(&binary_str, &arg_refs)),
                brokkr_args: None,
                metadata,
            };

            ctx.harness.run_internal(&config, |_i| {
                let (mut result, stderr, _sidecar) = harness::run_hotpath_capture(
                    &binary_str,
                    &arg_refs,
                    &ctx.paths.scratch_dir,
                    req.project_root,
                    &[("ELIVAGAR_NODE_STATS", "1")],
                    &[],
                    req.stop_marker,
                    Some(ctx.harness.lock()),
                )?;
                result.kv.push(KvPair::text(
                    "meta.locations_on_ways_detected",
                    elivagar::detect_locations_on_ways_stderr(&stderr).to_string(),
                ));
                Ok(result)
            })?;

            // Clean up output files.
            for path in command.output_files(&ctx.paths.scratch_dir) {
                std::fs::remove_file(path).ok();
            }
            // Also clean hotpath-specific output.
            let suffix = if alloc { "alloc-" } else { "" };
            let hp_output = ctx
                .paths
                .scratch_dir
                .join(format!("hotpath-{suffix}output.pmtiles"));
            std::fs::remove_file(hp_output).ok();

            Ok(())
        }
        BuildKind::Example(example) => {
            // Micro-benchmarks: build example with hotpath feature.
            let ctx = crate::context::HarnessContext::new(
                req.dev_config,
                req.project,
                req.project_root,
                req.build_root,
                &format!("hotpath {}", command.id()),
                req.force,
                req.wait,
                req.stop_marker.map(str::to_owned),
            )?
            .with_request(req);

            let binary = crate::build::cargo_build(
                &crate::build::BuildConfig {
                    package: None,
                    bin: None,
                    example: Some(example.into()),
                    features: vec![feature.into()],
                    default_features: true,
                    profile: "release",
                },
                req.effective_build_root(),
            )?;
            let binary_str = binary.display().to_string();

            let args =
                command.build_args("", std::path::Path::new(""), std::path::Path::new(""))?;
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

            let metadata = command.metadata();

            let config = BenchConfig {
                command: command.result_command().into(),
                mode: None,
                input_file: None,
                input_mb: None,
                cargo_features: Some(feature.into()),
                cargo_profile: crate::build::CargoProfile::Release,
                runs: 1,
                cli_args: None,
                brokkr_args: None,
                metadata,
            };

            ctx.harness.run_internal(&config, |_i| {
                let (result, _stderr, _sidecar) = harness::run_hotpath_capture(
                    &binary_str,
                    &arg_refs,
                    &ctx.paths.scratch_dir,
                    req.project_root,
                    &[],
                    &[],
                    req.stop_marker,
                    Some(ctx.harness.lock()),
                )?;
                Ok(result)
            })?;

            Ok(())
        }
        BuildKind::NoBuild => Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        ))),
    }
}

/// After a successful tilegen run, rename the output PMTiles to
/// `<scratch>/<dataset>-<commit>.pmtiles` and print the path.
///
/// For non-Tilegen commands, cleans up output files as before.
fn rename_elivagar_output(
    command: &ElivagarCommand,
    scratch_dir: &std::path::Path,
    dataset: &str,
    project_root: &std::path::Path,
) {
    let output_files = command.output_files(scratch_dir);
    if output_files.is_empty() {
        return;
    }

    // Only Tilegen produces output to rename.
    if !matches!(command, ElivagarCommand::Tilegen { .. }) {
        for path in &output_files {
            std::fs::remove_file(path).ok();
        }
        return;
    }

    let commit = crate::git::collect(project_root)
        .map(|g| g.commit)
        .unwrap_or_else(|_| "unknown".into());

    for path in &output_files {
        if path.exists() {
            let dest = scratch_dir.join(format!("{dataset}-{commit}.pmtiles"));
            match std::fs::rename(path, &dest) {
                Ok(()) => {
                    output::run_msg(&format!("output: {}", dest.display()));
                }
                Err(e) => {
                    output::error(&format!("failed to rename output: {e}"));
                    // Leave the original in place.
                }
            }
        }
    }
}

/// External tool dispatch (Planetiler, Tilemaker). Only bench mode is supported.
fn run_elivagar_external(req: &MeasureRequest, command: &ElivagarCommand) -> Result<(), DevError> {
    match req.mode {
        MeasureMode::Run | MeasureMode::Bench { .. } => match command {
            ElivagarCommand::Planetiler => elivagar::cmd::bench_planetiler(req),
            ElivagarCommand::Tilemaker => elivagar::cmd::bench_tilemaker(req),
            _ => unreachable!(),
        },
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        ))),
    }
}
