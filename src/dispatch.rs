//! Unified dispatch layer for the `brokkr run <command>` CLI surface.
//!
//! This module replaces the scattered `cmd_bench` / `cmd_hotpath` / `cmd_profile`
//! dispatch for pbfhogg and elivagar with a single entry point per project that
//! handles all measurement modes (wall-clock, hotpath, alloc).

use std::path::Path;

use crate::config;
use crate::context::BenchContext;
use crate::db::KvPair;
use crate::elivagar;
use crate::elivagar::commands::ElivagarCommand;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{CommandContext, CommandParams, MeasureMode, MeasureRequest};
use crate::nidhogg;
use crate::oom;
use crate::output;
use crate::pbfhogg::commands::{ArgMode, InputKind, OutputKind, PbfhoggCommand};
use crate::project::{self, Project};
use crate::resolve::{
    self, resolve_bbox, resolve_pbf_with_size,
};

// ---------------------------------------------------------------------------
// pbfhogg dispatch
// ---------------------------------------------------------------------------

/// Extract I/O mode flags from extra_params, run preflight checks, and return:
/// - extra cargo features to add to the build
/// - extra CLI args to append to the binary invocation
///
/// The chosen I/O flags end up in the subprocess arg string (cli_args) via
/// the caller, so no separate variant-suffix is needed — the result DB's
/// variant column only carries the measurement mode after v13.
fn resolve_io_flags(
    command: &PbfhoggCommand,
    extra_params: &CommandParams,
) -> Result<(Vec<&'static str>, Vec<&'static str>), DevError> {
    let direct_io = extra_params.direct_io;
    let io_uring = extra_params.io_uring;

    if io_uring && !command.supports_io_uring() {
        return Err(DevError::Config(format!(
            "--io-uring is not supported by '{}' (only apply-changes, sort, cat-dedupe, diff-osc)",
            command.id(),
        )));
    }

    if io_uring {
        crate::preflight::run_preflight(&crate::preflight::uring_checks())?;
    }

    let mut features = Vec::new();
    let mut args = Vec::new();

    if direct_io {
        features.push("linux-direct-io");
        args.push("--direct-io");
    }
    if io_uring {
        features.push("linux-io-uring");
        args.push("--io-uring");
    }

    Ok((features, args))
}

/// Run a single pbfhogg command with the specified measurement mode.
///
/// Handles run, bench, hotpath, and alloc for any pbfhogg command.
/// `extra_params` carries command-specific parameters (e.g. `index_type`
/// for `add-locations-to-ways`, `bbox` for extract).
pub fn run_pbfhogg_command_with_params(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &CommandParams,
) -> Result<(), DevError> {
    project::require(req.project, Project::Pbfhogg, command.id())?;

    // --start-stage and --keep-scratch are only meaningful for external join.
    let has_stage_flags = extra_params.start_stage.is_some() || extra_params.keep_scratch;
    if has_stage_flags && extra_params.index_type.as_deref() != Some("external") {
        return Err(DevError::Config(
            "--start-stage and --keep-scratch require --index-type external".into(),
        ));
    }

    if req.dry_run {
        return run_pbfhogg_dry_run(req, command, osc_seq, extra_params);
    }

    match req.mode {
        MeasureMode::Run => run_pbfhogg_run(req, command, osc_seq, extra_params),
        MeasureMode::Bench { .. } => run_pbfhogg_wallclock(req, command, osc_seq, extra_params),
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            run_pbfhogg_hotpath(req, command, osc_seq, extra_params)
        }
    }
}

/// Dry-run mode: validate argv, config, and path resolution without building
/// or running. Short-circuits after `command.build_args` succeeds.
///
/// Does: io-flag validation, project bootstrap, path resolution
/// (pbf/osc/bbox/snapshot/merged-cache-key), arg-vector construction, and
/// hotpath-arg-vector construction for commands that support it.
/// Does NOT: cargo build, lock acquisition, ensure_merged_pbf apply-changes,
/// preflight memory check, or process execution. Hash verification of the
/// input PBF/OSC IS performed (cached in `.brokkr/hash_cache`), because it's
/// part of "would this start up cleanly" and cache hits are fast.
fn run_pbfhogg_dry_run(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &CommandParams,
) -> Result<(), DevError> {
    // Validate I/O flag compatibility (e.g. --io-uring on a command that
    // doesn't support it) and run io_uring preflight if requested.
    let (_io_features, io_args) = resolve_io_flags(command, extra_params)?;

    // Resolve paths without building. bootstrap() reads cargo metadata for
    // target_dir — cheap, doesn't trigger a compile.
    let pi = crate::context::bootstrap(req.build_root)?;
    let paths = crate::context::bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)?;

    // Fake binary path for the CommandContext. `build_args(_, Bench)`
    // doesn't touch it; `build_args(_, Hotpath)` uses it only as a leading
    // argv[0] string. Use the brokkr binary's own path so any accidental
    // stat still succeeds.
    let fake_binary = std::env::current_exe()
        .unwrap_or_else(|_| req.project_root.join("brokkr.toml"));

    let cmd_ctx =
        build_pbfhogg_context(req, command, osc_seq, &fake_binary, &paths, extra_params)?;

    // Construct the wall-clock arg vector (catches any build_args failures,
    // e.g. missing bbox for extract, missing osc for merge).
    let mut args = command.build_args(&cmd_ctx, ArgMode::Bench)?;
    for flag in &io_args {
        args.push((*flag).into());
    }
    if let Some(c) = &extra_params.compression {
        args.push("--compression".into());
        args.push(c.clone());
    }

    // Also validate the hotpath arg construction path when supported, so
    // `--dry-run --hotpath` is meaningful.
    if command.supports_hotpath() {
        let _ = command.build_args(&cmd_ctx, ArgMode::Hotpath)?;
    }

    output::run_msg(&format!(
        "[dry-run] {} args: {}",
        command.id(),
        args.join(" ")
    ));
    output::run_msg(&format!("[dry-run] pbf: {}", cmd_ctx.pbf_path.display()));
    if let Some(ref p) = cmd_ctx.osc_path {
        output::run_msg(&format!("[dry-run] osc: {}", p.display()));
    }
    if cmd_ctx.osc_paths.len() > 1 {
        output::run_msg(&format!(
            "[dry-run] osc range: {} files ({} .. {})",
            cmd_ctx.osc_paths.len(),
            cmd_ctx.osc_paths.first().map_or(String::new(), |p| p.display().to_string()),
            cmd_ctx.osc_paths.last().map_or(String::new(), |p| p.display().to_string()),
        ));
    }
    if let Some(ref p) = cmd_ctx.pbf_b_path {
        output::run_msg(&format!("[dry-run] pbf_b: {}", p.display()));
    }
    if let Some(ref b) = cmd_ctx.bbox {
        output::run_msg(&format!("[dry-run] bbox: {b}"));
    }
    output::run_msg("[dry-run] ok");
    Ok(())
}

/// Default run mode: build, run once, print timing. Acquires lockfile, no DB storage.
fn run_pbfhogg_run(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &CommandParams,
) -> Result<(), DevError> {
    let (io_features, io_args) = resolve_io_flags(command, extra_params)?;

    let mut feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    feat_refs.extend_from_slice(&io_features);

    // Run mode never stores results, so dirty tree is always fine.
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &feat_refs,
        true,
        &format!("run {}", command.id()),
        true,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let cmd_ctx =
        build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;
    let mut args = command.build_args(&cmd_ctx, ArgMode::Bench)?;
    for flag in &io_args {
        args.push((*flag).into());
    }
    if let Some(c) = &extra_params.compression {
        args.push("--compression".into());
        args.push(c.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let binary_str = ctx.binary.display().to_string();
    output::run_msg(&format!("{binary_str} {}", arg_refs.join(" ")));

    let out = output::run_passthrough_timed(&binary_str, &arg_refs)?;

    cleanup_pbfhogg_output(command, &cmd_ctx, ArgMode::Bench);

    if out.code != 0 && !command.ok_exit_codes().contains(&out.code) {
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
    extra_params: &CommandParams,
) -> Result<(), DevError> {
    let (io_features, io_args) = resolve_io_flags(command, extra_params)?;

    let mut feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    feat_refs.extend_from_slice(&io_features);

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
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let cmd_ctx =
        build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;

    run_pbfhogg_wallclock_core(
        &ctx.harness,
        &ctx.binary,
        command,
        &cmd_ctx,
        &io_args,
        extra_params.compression.as_deref(),
        req.runs(),
        req.project_root,
        true,
    )
}

/// Run a single pbfhogg command via the wall-clock harness.
///
/// Shared by `run_pbfhogg_wallclock` (individual `brokkr <cmd>` invocations)
/// and the suite runner in `pbfhogg::bench_commands`. Both paths produce
/// identical DB rows — argv construction, BenchConfig fields, and scratch
/// cleanup are all centralised here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pbfhogg_wallclock_core(
    harness: &harness::BenchHarness,
    binary: &Path,
    command: &PbfhoggCommand,
    cmd_ctx: &CommandContext,
    io_args: &[&'static str],
    compression: Option<&str>,
    runs: usize,
    project_root: &Path,
    announce: bool,
) -> Result<(), DevError> {
    let mut args = command.build_args(cmd_ctx, ArgMode::Bench)?;
    for flag in io_args {
        args.push((*flag).into());
    }
    if let Some(c) = compression {
        args.push("--compression".into());
        args.push(c.to_owned());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let file_mb = resolve::file_size_mb(&cmd_ctx.pbf_path)?;
    let basename = cmd_ctx.pbf_basename();

    let config = BenchConfig {
        command: command.result_command(),
        mode: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(harness::format_cli_args(
            &binary.display().to_string(),
            &arg_refs,
        )),
        brokkr_args: None,
        metadata: command.metadata(cmd_ctx),
    };

    if announce {
        output::bench_msg(&format!(
            "{} ({file_mb:.0} MB), {runs} run(s)",
            command.id(),
        ));
    }

    harness.run_external_ok(
        &config,
        binary,
        &arg_refs,
        project_root,
        command.ok_exit_codes(),
    )?;

    cleanup_pbfhogg_output(command, cmd_ctx, ArgMode::Bench);

    Ok(())
}

/// Hotpath/alloc mode: build with hotpath feature, run via run_hotpath_capture.
fn run_pbfhogg_hotpath(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    extra_params: &CommandParams,
) -> Result<(), DevError> {
    if !command.supports_hotpath() {
        return Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        )));
    }

    let alloc = matches!(req.mode, MeasureMode::Alloc { .. });
    let feature = harness::hotpath_feature(alloc);
    let (io_features, io_args) = resolve_io_flags(command, extra_params)?;

    // Build features: hotpath + user features + I/O features.
    let mut all_features: Vec<&str> = vec![feature];
    all_features.extend(req.features.iter().map(String::as_str));
    all_features.extend_from_slice(&io_features);

    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &all_features,
        true,
        &format!(
            "run {} --{}",
            command.id(),
            if alloc { "alloc" } else { "hotpath" }
        ),
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let cmd_ctx =
        build_pbfhogg_context(req, command, osc_seq, &ctx.binary, &ctx.paths, extra_params)?;

    // Read input file size from the resolved PBF in cmd_ctx — correct for both
    // legacy commands and DiffSnapshots (which resolves a snapshot's PBF).
    let file_mb = resolve::file_size_mb(&cmd_ctx.pbf_path)?;
    let risk = if alloc {
        oom::MemoryRisk::AllocTracking
    } else {
        oom::MemoryRisk::Normal
    };
    // Renumber has a flat ~3-4 GB RAM footprint independent of input size
    // (radix-partitioned tuple files live on disk), so the input×multiplier
    // heuristic over-rejects it on planet. Skip the check.
    let skip_mem_check =
        req.no_mem_check || matches!(command, PbfhoggCommand::Renumber);
    oom::check_memory(file_mb, &risk, skip_mem_check)?;

    let mut hotpath_args = command.build_args(&cmd_ctx, ArgMode::Hotpath)?;
    for flag in &io_args {
        hotpath_args.push((*flag).into());
    }
    if let Some(c) = &extra_params.compression {
        hotpath_args.push("--compression".into());
        hotpath_args.push(c.clone());
    }

    let label = feature;
    output::hotpath_msg(&format!("=== {} {label} ===", command.id()));

    if alloc {
        output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
    }

    let basename = cmd_ctx.pbf_basename();
    let subprocess_args: Vec<&str> = hotpath_args[1..].iter().map(String::as_str).collect();

    // Metadata is reserved for runtime observations post-v13 (cache state,
    // detected features, resolved file paths). The alloc mode and the
    // command id that used to be dumped here are now recorded in the
    // variant and command columns respectively.
    let metadata = command.metadata(&cmd_ctx);

    let config = BenchConfig {
        command: command.result_command(),
        mode: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(
            &ctx.binary.display().to_string(),
            &subprocess_args,
        )),
        brokkr_args: None,
        metadata,
    };

    let binary_str = ctx.binary.display().to_string();
    let scratch_dir = ctx.paths.scratch_dir.clone();
    let project_root = req.project_root.to_path_buf();

    let ok_codes = command.ok_exit_codes();
    ctx.harness.run_internal(&config, |_i| {
        output::hotpath_msg(command.id());
        let (result, _stderr, _sidecar) = harness::run_hotpath_capture(
            &binary_str,
            &subprocess_args,
            &scratch_dir,
            &project_root,
            &[],
            ok_codes,
            req.stop_marker,
        )?;
        Ok(result)
    })?;

    cleanup_pbfhogg_output(command, &cmd_ctx, ArgMode::Hotpath);

    Ok(())
}

/// Build the `CommandContext` for a pbfhogg command, resolving all input paths.
fn build_pbfhogg_context(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    binary: &Path,
    paths: &config::ResolvedPaths,
    extra_params: &CommandParams,
) -> Result<CommandContext, DevError> {
    // DiffSnapshots resolves both PBFs via the snapshot resolver instead of
    // the standard pbf_path/osc/merged flow. Short-circuit here.
    if matches!(command, PbfhoggCommand::DiffSnapshots { .. }) {
        return build_diff_snapshots_context(req, binary, paths, extra_params);
    }

    // Parse the optional `--snapshot` flag from extra_params into a SnapshotRef.
    // None or "base" → SnapshotRef::Base (legacy top-level data, current
    // behavior preserved). Anything else → Named, validated.
    let snapshot_ref = match extra_params.snapshot.as_deref() {
        None => resolve::SnapshotRef::Base,
        Some(s) => resolve::SnapshotRef::parse(s)?,
    };

    // Resolve PBF via the snapshot-aware resolver. For Base this is identical
    // to the legacy `resolve_pbf_path`; for Named it reads the snapshot's
    // pbf table.
    let pbf_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        &snapshot_ref,
        req.variant,
        paths,
        req.project_root,
    )?;

    // Resolve OSC path(s) if needed.
    //
    // If `osc_range` is set in extra_params, expand the LO..HI range into an
    // ordered list of paths. Otherwise fall back to single-seq behavior. The
    // single-seq case captures the resolved seq key so callers (e.g.
    // `ensure_merged_pbf`) can include it in cache keys.
    //
    // Both resolution paths are snapshot-scoped: when --snapshot is set, OSCs
    // come from the snapshot's osc table, not the legacy top-level.
    let (osc_path, osc_paths, single_osc_seq) = if command.needs_osc() {
        if let Some(range) = &extra_params.osc_range {
            let paths_vec = resolve::resolve_osc_range(
                req.dataset,
                &snapshot_ref,
                range,
                paths,
                req.project_root,
            )?;
            (paths_vec.first().cloned(), paths_vec, None)
        } else {
            let (path, seq_key) = resolve::resolve_single_osc(
                req.dataset,
                &snapshot_ref,
                osc_seq,
                paths,
                req.project_root,
            )?;
            (Some(path.clone()), vec![path], Some(seq_key))
        }
    } else {
        (None, Vec::new(), None)
    };

    // Resolve merged PBF if needed (diff/diff-osc commands).
    let mut params = extra_params.clone();
    let pbf_b_path = resolve_merged_pbf(
        req,
        command,
        binary,
        &pbf_path,
        osc_path.as_deref(),
        single_osc_seq.as_deref(),
        &snapshot_ref,
        paths,
        extra_params,
        &mut params,
    )?;

    // Resolve bbox if needed. Check extra_params for a CLI override first.
    let bbox = if command.needs_bbox() {
        let cli_bbox = extra_params.bbox.as_deref();
        Some(resolve_bbox(cli_bbox, req.dataset, paths)?)
    } else {
        None
    };

    Ok(CommandContext {
        binary: binary.to_path_buf(),
        pbf_path,
        osc_path,
        osc_paths,
        pbf_b_path,
        scratch_dir: paths.scratch_dir.clone(),
        dataset: req.dataset.to_owned(),
        bbox,
        params,
    })
}

/// Resolve the B-side (merged) PBF path for diff/diff-osc commands.
///
/// In any measured mode (bench/hotpath/alloc) we force a cache rebuild by
/// default so total brokkr-invocation wall time is deterministic regardless
/// of prior session state. `--keep-cache` opts back into reuse. In dry-run
/// mode, synthesizes the would-be cache path without running apply-changes.
#[allow(clippy::too_many_arguments)]
fn resolve_merged_pbf(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    binary: &Path,
    pbf_path: &Path,
    osc_path: Option<&Path>,
    single_osc_seq: Option<&str>,
    snapshot_ref: &resolve::SnapshotRef,
    paths: &config::ResolvedPaths,
    extra_params: &CommandParams,
    params: &mut CommandParams,
) -> Result<Option<std::path::PathBuf>, DevError> {
    if command.input_kind() != InputKind::PbfAndMerged {
        return Ok(None);
    }
    let osc = osc_path
        .ok_or_else(|| DevError::Config("merged PBF requires an OSC file".into()))?;
    let osc_seq_str = single_osc_seq
        .ok_or_else(|| DevError::Config("merged PBF requires a single OSC seq".into()))?;
    // Snapshot key for the cache key. Different snapshots produce different
    // merged PBFs from the same OSC seq, so the cache must disambiguate.
    let snapshot_key = match snapshot_ref {
        resolve::SnapshotRef::Base => "base",
        resolve::SnapshotRef::Named(k) => k.as_str(),
    };

    if req.dry_run {
        // Synthesize the would-be merged path from the same key format
        // `ensure_merged_pbf` uses, without running apply-changes. The
        // existence of the cache file is irrelevant to dry-run — we just
        // need a populated `pbf_b_path` so `build_args` can reference it.
        let stem = pbf_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("input");
        let merged_name =
            format!("{stem}-snap{snapshot_key}-osc{osc_seq_str}-bench-merged.osm.pbf");
        return Ok(Some(paths.scratch_dir.join(merged_name)));
    }

    let force_rebuild = !matches!(req.mode, MeasureMode::Run) && !extra_params.keep_cache;
    let (merged, state) = ensure_merged_pbf(
        binary,
        pbf_path,
        osc,
        snapshot_key,
        osc_seq_str,
        &paths.scratch_dir,
        req.project_root,
        force_rebuild,
    )?;
    // Stash cache state in params so the command's metadata builder can
    // emit it as KvPairs on the result row.
    match state {
        MergedCacheState::Hit { age_secs } => {
            params.merged_cache_state = Some("hit".into());
            params.merged_cache_age_s = Some(age_secs.to_string());
        }
        MergedCacheState::Miss => {
            params.merged_cache_state = Some("miss".into());
        }
    }
    Ok(Some(merged))
}

/// Build the `CommandContext` for `DiffSnapshots`. Resolves both PBF paths
/// via the snapshot resolver — `pbf_path` is the `--from` side, `pbf_b_path`
/// is the `--to` side. The `--variant` is applied symmetrically to both.
fn build_diff_snapshots_context(
    req: &MeasureRequest,
    binary: &Path,
    paths: &config::ResolvedPaths,
    extra_params: &CommandParams,
) -> Result<CommandContext, DevError> {
    let from_str = extra_params.from_snapshot.as_deref().ok_or_else(|| {
        DevError::Config("diff-snapshots requires --from".into())
    })?;
    let to_str = extra_params.to_snapshot.as_deref().ok_or_else(|| {
        DevError::Config("diff-snapshots requires --to".into())
    })?;

    let from_ref = resolve::SnapshotRef::parse(from_str)?;
    let to_ref = resolve::SnapshotRef::parse(to_str)?;

    let from_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        &from_ref,
        req.variant,
        paths,
        req.project_root,
    )?;
    let to_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        &to_ref,
        req.variant,
        paths,
        req.project_root,
    )?;

    // Stash the to-side file's basename and size in params so the result row
    // metadata can describe both inputs (input_file/input_mb on the row only
    // covers the from side). Lets `brokkr results` queries filter by the
    // B-side via `--meta to_snapshot_file=<name>`.
    let mut params = extra_params.clone();
    if let Some(name) = to_path.file_name().and_then(|s| s.to_str()) {
        params.to_snapshot_file = Some(name.to_owned());
    }
    if let Ok(mb) = resolve::file_size_mb(&to_path) {
        params.to_snapshot_file_mb = Some(format!("{mb:.0}"));
    }

    Ok(CommandContext {
        binary: binary.to_path_buf(),
        pbf_path: from_path,
        osc_path: None,
        osc_paths: Vec::new(),
        pbf_b_path: Some(to_path),
        scratch_dir: paths.scratch_dir.clone(),
        dataset: req.dataset.to_owned(),
        bbox: None,
        params,
    })
}

/// Clean up scratch output files after a benchmark run.
pub(crate) fn cleanup_pbfhogg_output(
    command: &PbfhoggCommand,
    ctx: &CommandContext,
    mode: ArgMode,
) {
    // Multi-extract has custom cleanup: output dir + config JSON.
    if matches!(command, PbfhoggCommand::MultiExtract { .. }) {
        std::fs::remove_dir_all(ctx.scratch_dir.join("multi-extract")).ok();
        std::fs::remove_file(ctx.scratch_dir.join("multi-extract-config.json")).ok();
        return;
    }

    match command.output_kind() {
        OutputKind::ScratchPbf | OutputKind::ScratchOsc => {
            std::fs::remove_file(crate::pbfhogg::commands::scratch_output_path(
                ctx, command, mode,
            ))
            .ok();
        }
        OutputKind::ScratchDir(dir_name) => {
            let path = ctx.scratch_dir.join(format!("{dir_name}-{}", ctx.dataset));
            std::fs::remove_dir_all(path).ok();
        }
        OutputKind::None => {}
    }
}

/// State of the merged-PBF cache after `ensure_merged_pbf` returns.
///
/// Carried into the result row's metadata so `brokkr results <uuid>` can
/// distinguish runs that paid the apply-changes setup cost from runs that
/// reused a cached file.
#[derive(Debug, Clone, Copy)]
enum MergedCacheState {
    /// The merged PBF was reused from a prior run.
    Hit { age_secs: u64 },
    /// The merged PBF was generated by this invocation.
    Miss,
}

/// Ensure a merged PBF exists in the scratch directory. Returns the path and
/// the cache state (hit/miss + age on hit).
///
/// The cache key includes the snapshot key AND the OSC seq so neither
/// `--snapshot` nor `--osc-seq` invocations can silently reuse each other's
/// merged files. If `force_rebuild` is set, any existing cached file is
/// deleted before checking — used by measured modes (bench/hotpath/alloc) to
/// make total invocation wall time deterministic regardless of prior session
/// state.
#[allow(clippy::too_many_arguments)]
fn ensure_merged_pbf(
    binary: &Path,
    pbf_path: &Path,
    osc_path: &Path,
    snapshot_key: &str,
    osc_seq: &str,
    scratch_dir: &Path,
    project_root: &Path,
    force_rebuild: bool,
) -> Result<(std::path::PathBuf, MergedCacheState), DevError> {
    let stem = pbf_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("input");
    // Cache key: <pbf-stem>-snap<key>-osc<seq>-bench-merged.osm.pbf
    // Including snapshot + seq prevents silent wrong-file reuse across runs
    // that target different snapshots or different osc seqs.
    let merged_name =
        format!("{stem}-snap{snapshot_key}-osc{osc_seq}-bench-merged.osm.pbf");
    let merged_path = scratch_dir.join(&merged_name);

    if force_rebuild && merged_path.exists() {
        output::bench_msg(&format!(
            "force-rebuilding merged PBF (cache nuked by measured mode): {merged_name}"
        ));
        std::fs::remove_file(&merged_path).map_err(|e| {
            DevError::Config(format!("failed to remove cached merged PBF: {e}"))
        })?;
    }

    if merged_path.exists() {
        let age_secs = std::fs::metadata(&merged_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        output::bench_msg(&format!(
            "using cached merged PBF: {merged_name} (age: {age_secs}s)"
        ));
        return Ok((merged_path, MergedCacheState::Hit { age_secs }));
    }

    std::fs::create_dir_all(scratch_dir)
        .map_err(|e| DevError::Config(format!("failed to create scratch dir: {e}")))?;

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

    let start = std::time::Instant::now();
    let captured = output::run_captured(
        &binary_str,
        &["apply-changes", pbf_str, osc_str, "-o", merged_str],
        project_root,
    )?;

    captured.check_success(&binary_str)?;
    let elapsed_s = start.elapsed().as_secs();
    output::bench_msg(&format!("merged PBF ready in {elapsed_s}s"));

    Ok((merged_path, MergedCacheState::Miss))
}

// ---------------------------------------------------------------------------
// elivagar dispatch
// ---------------------------------------------------------------------------

/// Run an elivagar command with the specified measurement mode.
///
/// Handles run, bench, hotpath, and alloc for any elivagar command.
/// External commands (Planetiler, Tilemaker) delegate to old handlers for
/// bench mode but do not support run/hotpath/alloc.
pub fn run_elivagar_command(
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
        cargo_profile: "release".into(),
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
        cargo_profile: "release".into(),
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
                cargo_profile: "release".into(),
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
                cargo_profile: "release".into(),
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

// ---------------------------------------------------------------------------
// nidhogg dispatch
// ---------------------------------------------------------------------------

/// Run a nidhogg command with the specified measurement mode.
///
/// Ingest follows the standard build+run_external pattern (like pbfhogg).
/// Api and Tiles have custom lifecycles and delegate to per-module functions.
pub fn run_nidhogg_command(
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
        cargo_profile: "release".into(),
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
        cargo_profile: "release".into(),
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
        let (result, _stderr, _sidecar) =
            harness::run_hotpath_capture(&binary_str, &args, &ctx.paths.scratch_dir, req.project_root, &[], &[], req.stop_marker)?;
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
