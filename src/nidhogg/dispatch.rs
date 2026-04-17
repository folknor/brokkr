//! Unified dispatch layer for nidhogg commands.

use crate::context::BenchContext;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::nidhogg;
use crate::oom;
use crate::output;
use crate::project::{self, Project};
use crate::resolve::resolve_pbf_with_size;

/// Run a nidhogg command with the specified measurement mode.
///
/// Ingest follows the standard build+run_external pattern (like pbfhogg).
/// Api and Tiles have custom lifecycles and delegate to per-module functions.
pub(crate) fn run_command(
    req: &MeasureRequest,
    command: &crate::nidhogg::commands::NidhoggCommand,
) -> Result<(), DevError> {
    use crate::nidhogg::commands::NidhoggCommand;

    project::require(req.project, Project::Nidhogg, command.id())?;

    if req.dry_run {
        return Err(DevError::Config(
            "--dry-run is not yet supported for nidhogg commands".into(),
        ));
    }

    match req.mode {
        MeasureMode::Run => match command {
            NidhoggCommand::Ingest => run_nidhogg_ingest_run(req),
            // Api/Tiles have no lightweight run mode — fall through to bench.
            NidhoggCommand::Api { query } => nidhogg::cmd::bench_api(req, query.as_deref()),
            NidhoggCommand::Tiles { tiles_variant, uring } => {
                nidhogg::cmd::bench_tiles(req, tiles_variant.as_deref(), *uring)
            }
        },
        MeasureMode::Bench { .. } => match command {
            NidhoggCommand::Ingest => run_nidhogg_ingest_bench(req, command),
            NidhoggCommand::Api { query } => nidhogg::cmd::bench_api(req, query.as_deref()),
            NidhoggCommand::Tiles { tiles_variant, uring } => {
                nidhogg::cmd::bench_tiles(req, tiles_variant.as_deref(), *uring)
            }
        },
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            if !command.supports_hotpath() {
                return Err(DevError::Config(format!(
                    "command '{}' does not support hotpath/alloc profiling",
                    command.id(),
                )));
            }
            run_nidhogg_hotpath(req, command)
        }
    }
}

/// Nidhogg ingest: lightweight run mode (build, run once, print timing, no DB).
fn run_nidhogg_ingest_run(req: &MeasureRequest) -> Result<(), DevError> {
    let feat_refs = req.feat_refs();
    // Run mode never stores results, so dirty tree is always fine.
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
        &feat_refs,
        true,
        &format!("run {}", "nid-ingest"),
        true,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let (pbf_path, _) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let output_dir = ctx.paths.scratch_dir.join("run-ingest-output");
    std::fs::create_dir_all(&output_dir)?;
    let output_str = output_dir.display().to_string();

    let binary_str = ctx.binary.display().to_string();
    let args = ["ingest", pbf_str, &output_str];
    output::run_msg(&format!("{binary_str} {}", args.join(" ")));

    let out = output::run_passthrough_timed(&binary_str, &args)?;

    // Clean up ingest output.
    std::fs::remove_dir_all(&output_dir).ok();

    if out.code != 0 {
        return Err(DevError::ExitCode(out.code));
    }

    let ms = crate::duration_ms(out.elapsed);
    output::run_msg(&format!("elapsed={ms}ms"));
    Ok(())
}

/// Nidhogg ingest: bench mode via BenchContext + run_external.
///
/// Uses `run_internal` with per-run scratch cleanup (ingest produces a
/// data directory that must be cleaned between runs for accurate timing).
fn run_nidhogg_ingest_bench(
    req: &MeasureRequest,
    command: &crate::nidhogg::commands::NidhoggCommand,
) -> Result<(), DevError> {
    let feat_refs = req.feat_refs();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
        &feat_refs,
        true,
        &format!("bench {}", command.id()),
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let output_dir = ctx.paths.scratch_dir.join("bench-ingest-output");
    let output_str = output_dir.display().to_string();

    output::bench_msg(&format!(
        "nidhogg ingest: {basename} ({file_mb:.0} MB), {} run(s)",
        req.runs(),
    ));

    let args: Vec<&str> = vec!["ingest", pbf_str, &output_str];

    let config = BenchConfig {
        command: command.result_command().into(),
        mode: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &args,
        )),
        brokkr_args: None,
        metadata: command.metadata(),
    };

    // Clean scratch before first run.
    clean_nidhogg_scratch(&output_dir)?;

    // Per-run cleanup: ingest produces a data directory that must be
    // cleaned between runs for accurate timing.
    ctx.harness.run_internal(&config, |_i| {
        clean_nidhogg_scratch(&output_dir)?;

        let captured =
            output::run_captured(&ctx.binary.display().to_string(), &args, req.project_root)?;
        let ms = harness::elapsed_to_ms(&captured.elapsed);
        captured.check_success(&ctx.binary.display().to_string())?;

        Ok(harness::BenchResult {
            elapsed_ms: ms,
            kv: vec![],
            distribution: None,
            hotpath: None,
        })
    })?;

    std::fs::remove_dir_all(&output_dir).ok();
    Ok(())
}

/// Nidhogg hotpath/alloc via the dispatch layer.
///
/// Builds the nidhogg binary with hotpath features, resolves PBF,
/// runs via `run_hotpath_capture`, same pattern as pbfhogg and elivagar.
fn run_nidhogg_hotpath(
    req: &MeasureRequest,
    command: &crate::nidhogg::commands::NidhoggCommand,
) -> Result<(), DevError> {
    let alloc = req.is_alloc();
    let feature = harness::hotpath_feature(alloc);

    output::hotpath_msg(&format!("=== nidhogg {} {feature} ===", command.id()));
    if alloc {
        output::hotpath_msg("NOTE: alloc profiling — wall-clock times are not meaningful");
    }

    let hotpath_features = req.hotpath_features();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
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

    let output_dir = ctx.paths.scratch_dir.join("hotpath-ingest-output");
    let output_str = output_dir.display().to_string();
    std::fs::create_dir_all(&output_dir)?;

    let args: Vec<&str> = vec!["ingest", pbf_str, &output_str];

    let config = BenchConfig {
        command: command.result_command().into(),
        mode: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &args,
        )),
        brokkr_args: None,
        metadata: vec![],
    };

    let binary_str = ctx.binary.display().to_string();

    ctx.harness.run_internal(&config, |_i| {
        let (result, _stderr, _sidecar) = harness::run_hotpath_capture(
            &binary_str,
            &args,
            &ctx.paths.scratch_dir,
            req.project_root,
            &[],
            &[],
            req.stop_marker,
            Some(ctx.harness.lock()),
        )?;
        Ok(result)
    })?;

    std::fs::remove_dir_all(&output_dir).ok();
    Ok(())
}

/// Remove and recreate a scratch directory (used by nidhogg ingest between runs).
fn clean_nidhogg_scratch(dir: &std::path::Path) -> Result<(), DevError> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}
