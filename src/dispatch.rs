//! Unified dispatch layer for the `brokkr run <command>` CLI surface.
//!
//! This module replaces the scattered `cmd_bench` / `cmd_hotpath` / `cmd_profile`
//! dispatch for pbfhogg and elivagar with a single entry point per project that
//! handles all measurement modes (wall-clock, hotpath, alloc).

use std::collections::HashMap;
use std::path::Path;

use crate::config;
use crate::context::BenchContext;
use crate::db::KvPair;
use crate::error::DevError;
use crate::elivagar;
use crate::elivagar::commands::ElivagarCommand;
use crate::harness::{self, BenchConfig};
use crate::measure::{CommandContext, MeasureMode, MeasureRequest};
use crate::oom;
use crate::output;
use crate::pbfhogg::commands::{InputKind, OutputKind, PbfhoggCommand};
use crate::project::{self, Project};
use crate::resolve::{
    self, resolve_bbox, resolve_default_osc_path, resolve_pbf_path, resolve_pbf_with_size,
};

// ---------------------------------------------------------------------------
// pbfhogg dispatch
// ---------------------------------------------------------------------------

/// Run a single pbfhogg command with the specified measurement mode.
///
/// Handles run, bench, hotpath, and alloc for any pbfhogg command.
/// `extra_params` carries command-specific parameters (e.g. `index_type`
/// for `add-locations-to-ways`, `bbox` for extract).
pub fn run_pbfhogg_command_with_params(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &HashMap<String, String>,
) -> Result<(), DevError> {
    project::require(req.project, Project::Pbfhogg, &format!("run {}", command.id()))?;

    match req.mode {
        MeasureMode::Run => run_pbfhogg_run(req, command, osc_seq, extra_params),
        MeasureMode::Bench { .. } => run_pbfhogg_wallclock(req, command, osc_seq, extra_params),
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => run_pbfhogg_hotpath(req, command, osc_seq, extra_params),
    }
}

/// Default run mode: build, run once, print timing. Acquires lockfile, no DB storage.
fn run_pbfhogg_run(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &HashMap<String, String>,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &feat_refs,
        true,
        &format!("run {}", command.id()),
        req.force,
    )?;

    let cmd_ctx = build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;
    let args = command.build_args(&cmd_ctx)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let binary_str = ctx.binary.display().to_string();
    output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

    let out = output::run_passthrough_timed(&binary_str, &arg_refs)?;

    cleanup_pbfhogg_output(command, &cmd_ctx);

    if out.code != 0 {
        return Err(DevError::ExitCode(out.code));
    }

    let ms = crate::duration_ms(out.elapsed);
    output::run_msg(&format!("elapsed={ms}ms"));

    Ok(())
}

/// Wall-clock benchmark: build release binary, run externally, record timing.
fn run_pbfhogg_wallclock(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &HashMap<String, String>,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &feat_refs,
        true,
        &format!("run {}", command.id()),
        req.force,
    )?;

    let cmd_ctx = build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;
    let args = command.build_args(&cmd_ctx)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (_, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let basename = cmd_ctx.pbf_basename();

    let config = BenchConfig {
        command: command.result_command().into(),
        variant: command.result_variant(),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs,
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &arg_refs,
        )),
        metadata: command.metadata(&cmd_ctx),
    };

    output::bench_msg(&format!(
        "{} ({file_mb:.0} MB), {} run(s)",
        command.id(),
        req.runs,
    ));

    ctx.harness
        .run_external(&config, &ctx.binary, &arg_refs, req.project_root)?;

    // Clean up scratch output files.
    cleanup_pbfhogg_output(command, &cmd_ctx);

    Ok(())
}

/// Hotpath/alloc mode: build with hotpath feature, run via run_hotpath_capture.
fn run_pbfhogg_hotpath(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &HashMap<String, String>,
) -> Result<(), DevError> {
    if !command.supports_hotpath() {
        return Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        )));
    }

    let alloc = matches!(req.mode, MeasureMode::Alloc { .. });
    let feature = harness::hotpath_feature(alloc);

    // Build features: hotpath + user features.
    let mut all_features: Vec<&str> = vec![feature];
    all_features.extend(req.features.iter().map(String::as_str));

    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &all_features,
        true,
        &format!("run {} --{}", command.id(), if alloc { "alloc" } else { "hotpath" }),
        req.force,
    )?;

    // OOM check.
    let (_, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let risk = if alloc {
        oom::MemoryRisk::AllocTracking
    } else {
        oom::MemoryRisk::Normal
    };
    oom::check_memory(file_mb, &risk, req.no_mem_check)?;

    let cmd_ctx = build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;
    let hotpath_args = command.build_hotpath_args(&cmd_ctx)?;

    let label = feature;
    output::hotpath_msg(&format!("=== {} {label} ===", command.id()));

    if alloc {
        output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
    }

    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("{}{variant_suffix}", command.id());

    let basename = cmd_ctx.pbf_basename();
    let subprocess_args: Vec<&str> = hotpath_args[1..].iter().map(String::as_str).collect();

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs,
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &subprocess_args,
        )),
        metadata: vec![
            KvPair::text("meta.alloc", alloc.to_string()),
            KvPair::text("meta.test", command.id()),
        ],
    };

    let binary_str = ctx.binary.display().to_string();
    let scratch_dir = ctx.paths.scratch_dir.clone();
    let project_root = req.project_root.to_path_buf();

    ctx.harness.run_internal(&config, |_i| {
        output::hotpath_msg(command.id());
        let (result, _stderr) = harness::run_hotpath_capture(
            &binary_str,
            &subprocess_args,
            &scratch_dir,
            &project_root,
            &[],
        )?;
        Ok(result)
    })?;

    cleanup_pbfhogg_output(command, &cmd_ctx);

    Ok(())
}

/// Build the `CommandContext` for a pbfhogg command, resolving all input paths.
fn build_pbfhogg_context(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    binary: &Path,
    paths: &config::ResolvedPaths,
    extra_params: &HashMap<String, String>,
) -> Result<CommandContext, DevError> {
    let pbf_path = resolve_pbf_path(req.dataset, req.variant, paths, req.project_root)?;

    // Resolve OSC path if needed.
    let osc_path = if command.needs_osc() {
        let osc = match osc_seq {
            Some(seq) => resolve::resolve_osc_path(req.dataset, seq, paths, req.project_root)?,
            None => resolve_default_osc_path(req.dataset, paths, req.project_root)?,
        };
        Some(osc)
    } else {
        None
    };

    // Resolve merged PBF if needed (diff/diff-osc commands).
    let merged_pbf_path = if command.input_kind() == InputKind::PbfAndMerged {
        let osc = osc_path.as_ref().ok_or_else(|| {
            DevError::Config("merged PBF requires an OSC file".into())
        })?;
        Some(ensure_merged_pbf(binary, &pbf_path, osc, &paths.scratch_dir, req.project_root)?)
    } else {
        None
    };

    // Resolve bbox if needed. Check extra_params for a CLI override first.
    let bbox = if command.needs_bbox() {
        let cli_bbox = extra_params.get("bbox").map(String::as_str);
        Some(resolve_bbox(cli_bbox, req.dataset, paths)?)
    } else {
        None
    };

    Ok(CommandContext {
        binary: binary.to_path_buf(),
        pbf_path,
        osc_path,
        merged_pbf_path,
        scratch_dir: paths.scratch_dir.clone(),
        dataset: req.dataset.to_owned(),
        bbox,
        params: extra_params.clone(),
    })
}

/// Clean up scratch output files after a benchmark run.
fn cleanup_pbfhogg_output(command: &PbfhoggCommand, ctx: &CommandContext) {
    match command.output_kind() {
        OutputKind::ScratchPbf(_) => {
            let name = command.id();
            let path = ctx.scratch_dir.join(format!("bench-{name}-output.osm.pbf"));
            std::fs::remove_file(path).ok();
        }
        OutputKind::ScratchOsc(_) => {
            let name = command.id();
            let path = ctx.scratch_dir.join(format!("bench-{name}-output.osc.gz"));
            std::fs::remove_file(path).ok();
        }
        OutputKind::ScratchDir(dir_name) => {
            let path = ctx.scratch_dir.join(format!("{dir_name}-{}", ctx.dataset));
            std::fs::remove_dir_all(path).ok();
        }
        OutputKind::None => {}
    }
}

/// Ensure a merged PBF exists in the scratch directory. Returns the path.
///
/// Ported from `bench_commands::ensure_merged_pbf`.
fn ensure_merged_pbf(
    binary: &Path,
    pbf_path: &Path,
    osc_path: &Path,
    scratch_dir: &Path,
    project_root: &Path,
) -> Result<std::path::PathBuf, DevError> {
    let stem = pbf_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("input");
    let merged_name = format!("{stem}-bench-merged.osm.pbf");
    let merged_path = scratch_dir.join(&merged_name);

    if merged_path.exists() {
        output::bench_msg(&format!("using cached merged PBF: {merged_name}"));
        return Ok(merged_path);
    }

    std::fs::create_dir_all(scratch_dir).map_err(|e| {
        DevError::Config(format!("failed to create scratch dir: {e}"))
    })?;

    output::bench_msg(&format!("generating merged PBF: {merged_name}"));
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path not UTF-8".into()))?;
    let osc_str = osc_path
        .to_str()
        .ok_or_else(|| DevError::Config("OSC path not UTF-8".into()))?;
    let merged_str = merged_path
        .to_str()
        .ok_or_else(|| DevError::Config("merged path not UTF-8".into()))?;
    let binary_str = binary.display().to_string();

    let captured = output::run_captured(
        &binary_str,
        &["apply-changes", pbf_str, osc_str, "-o", merged_str],
        project_root,
    )?;

    captured.check_success(&binary_str)?;

    Ok(merged_path)
}

// ---------------------------------------------------------------------------
// elivagar dispatch
// ---------------------------------------------------------------------------

/// Run an elivagar command with the specified measurement mode.
#[allow(clippy::too_many_lines)]
pub fn run_elivagar_command(
    req: &MeasureRequest,
    command: &ElivagarCommand,
) -> Result<(), DevError> {
    project::require(req.project, Project::Elivagar, &format!("run {}", command.id()))?;

    match req.mode {
        MeasureMode::Run | MeasureMode::Bench { .. } => run_elivagar_wallclock(req, command),
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => run_elivagar_hotpath(req, command),
    }
}

/// Elivagar wall-clock: delegates to existing bench modules based on command variant.
fn run_elivagar_wallclock(
    req: &MeasureRequest,
    command: &ElivagarCommand,
) -> Result<(), DevError> {
    let bench_req = crate::request::BenchRequest {
        dev_config: req.dev_config,
        project: req.project,
        project_root: req.project_root,
        build_root: req.build_root,
        dataset: req.dataset,
        variant: req.variant,
        runs: req.runs,
        features: req.features,
        force: req.force,
    };

    match command {
        ElivagarCommand::Tilegen {
            opts,
            skip_to,
            compression_level,
        } => {
            elivagar::cmd::bench_self(&bench_req, *skip_to, *compression_level, opts)
        }
        ElivagarCommand::PmtilesWriter { tiles } => {
            elivagar::cmd::bench_pmtiles(
                req.dev_config,
                req.project,
                req.project_root,
                req.build_root,
                *tiles,
                req.runs,
                req.force,
            )
        }
        ElivagarCommand::NodeStore { nodes } => {
            elivagar::cmd::bench_node_store(
                req.dev_config,
                req.project,
                req.project_root,
                req.build_root,
                *nodes,
                req.runs,
                req.force,
            )
        }
        ElivagarCommand::Planetiler => elivagar::cmd::bench_planetiler(&bench_req),
        ElivagarCommand::Tilemaker => elivagar::cmd::bench_tilemaker(&bench_req),
    }
}

/// Elivagar hotpath/alloc: build with hotpath feature, run with instrumentation.
fn run_elivagar_hotpath(
    req: &MeasureRequest,
    command: &ElivagarCommand,
) -> Result<(), DevError> {
    if !command.supports_hotpath() {
        return Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        )));
    }

    let alloc = matches!(req.mode, MeasureMode::Alloc { .. });
    let feature = harness::hotpath_feature(alloc);

    let mut all_features: Vec<&str> = vec![feature];
    all_features.extend(req.features.iter().map(String::as_str));

    // Build the HotpathRequest to delegate to existing elivagar hotpath code.
    let hotpath_req = crate::request::HotpathRequest {
        dev_config: req.dev_config,
        project: req.project,
        project_root: req.project_root,
        build_root: req.build_root,
        dataset: req.dataset,
        variant: req.variant,
        runs: req.runs,
        all_features: &all_features,
        alloc,
        no_mem_check: req.no_mem_check,
        force: req.force,
    };

    match command {
        ElivagarCommand::Tilegen { opts, .. } => {
            elivagar::cmd::hotpath(&hotpath_req, None, 0, 0, opts)
        }
        ElivagarCommand::PmtilesWriter { tiles } => {
            elivagar::cmd::hotpath(&hotpath_req, Some("pmtiles"), *tiles, 0, &default_pipeline_opts())
        }
        ElivagarCommand::NodeStore { nodes } => {
            elivagar::cmd::hotpath(&hotpath_req, Some("node-store"), 0, *nodes, &default_pipeline_opts())
        }
        ElivagarCommand::Planetiler | ElivagarCommand::Tilemaker => {
            Err(DevError::Config(format!(
                "command '{}' does not support hotpath/alloc profiling",
                command.id(),
            )))
        }
    }
}

/// Default pipeline opts for hotpath calls that don't need pipeline flags.
fn default_pipeline_opts<'a>() -> elivagar::PipelineOpts<'a> {
    elivagar::PipelineOpts {
        no_ocean: false,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        tile_format: None,
        tile_compression: None,
        compress_sort_chunks: None,
        in_memory: false,
        locations_on_ways: false,
        fanout_cap_default: None,
        fanout_cap: None,
        polygon_simplify_factor: None,
    }
}
