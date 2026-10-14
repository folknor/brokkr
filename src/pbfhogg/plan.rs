//! Pbfhogg input resolution and command plans.
use std::path::Path;

use crate::config;
use crate::context::MeasurementSession;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::runner::{CommandPlan, PlannedCommand, RowSubject};
use crate::measure::{CommandContext, CommandParams, MeasureMode, MeasureRequest};
use crate::output;
use crate::pbfhogg::commands::{ArgMode, InputKind, OutputKind, PbfhoggCommand};
use crate::project::Project;
use crate::resolve::{self, resolve_bbox};

/// Resolve cargo features for the selected I/O mode and run its preflight
/// checks. The command arg builder reads the same parameters to construct argv.
fn resolve_io_features(
    command: &PbfhoggCommand,
    extra_params: &CommandParams,
) -> Result<Vec<&'static str>, DevError> {
    let direct_io = extra_params.direct_io;
    let io_uring = extra_params.io_uring;

    if io_uring && !command.supports_io_uring() {
        return Err(DevError::Config(format!(
            "--io-uring is not supported by '{}'",
            command.id(),
        )));
    }

    if io_uring {
        crate::preflight::run_preflight(&crate::preflight::uring_checks())?;
    }

    let mut features = Vec::new();
    if direct_io {
        features.push("linux-direct-io");
    }
    if io_uring {
        features.push("linux-io-uring");
    }

    Ok(features)
}

pub(crate) struct PbfhoggPlan<'a> {
    pub command: &'a PbfhoggCommand,
    pub osc_seq: Option<&'a str>,
    pub params: &'a CommandParams,
}

impl PlannedCommand for PbfhoggPlan<'_> {
    type State = CommandContext;
    fn project(&self) -> Project {
        Project::Pbfhogg
    }
    fn id(&self) -> &'static str {
        self.command.id()
    }
    fn package(&self) -> Option<&'static str> {
        Some("pbfhogg-cli")
    }
    fn supports_profiling(&self) -> bool {
        self.command.supports_hotpath()
    }
    fn ok_codes(&self) -> &[i32] {
        self.command.ok_exit_codes()
    }
    fn build_config(
        &self,
        req: &MeasureRequest<'_>,
    ) -> Result<Option<crate::build::BuildConfig>, DevError> {
        preflight_as_snapshot(self.command, req, self.params)?;
        let io = resolve_io_features(self.command, self.params)?;
        let mut features = if matches!(
            req.mode,
            MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
        ) {
            req.build_features()
        } else {
            req.feat_refs()
        };
        features.extend(io);
        Ok(Some(crate::build::BuildConfig::release_with_features(
            self.package(),
            &features,
        )))
    }
    fn plan(
        &self,
        req: &MeasureRequest<'_>,
        paths: &config::ResolvedPaths,
        binary: &Path,
    ) -> Result<CommandPlan<CommandContext>, DevError> {
        if req.dry_run {
            preflight_as_snapshot(self.command, req, self.params)?;
            resolve_io_features(self.command, self.params)?;
        }
        let state =
            build_pbfhogg_context(req, self.command, self.osc_seq, binary, paths, self.params)?;
        let mode = arg_mode(req);
        let mut args = self.command.build_args(&state, mode)?;
        if mode == ArgMode::Hotpath {
            args.remove(0);
        }
        let (basename, mb) = result_input_metadata(self.command, &state)?;
        Ok(CommandPlan {
            args,
            scratch_dirs: vec![],
            subject: RowSubject {
                command: self.command.id().into(),
                input_file: Some(basename),
                input_mb: Some(mb),
                cli_args: None,
                metadata: self.command.metadata(&state),
            },
            state,
        })
    }
    fn describe(
        &self,
        req: &MeasureRequest<'_>,
        plan: &CommandPlan<CommandContext>,
    ) -> Result<Vec<String>, DevError> {
        // The selected argv was built by `plan`; check the alternate shape as
        // well so a dry run catches missing inputs in either supported mode.
        if self.command.supports_hotpath() {
            let other_mode = match arg_mode(req) {
                ArgMode::Bench => ArgMode::Hotpath,
                ArgMode::Hotpath => ArgMode::Bench,
            };
            let _ = self.command.build_args(&plan.state, other_mode)?;
        }

        let mut lines = vec![format!(
            "{} args: {}",
            self.command.id(),
            plan.args.join(" ")
        )];
        if let Some(path) = &plan.state.pbf_path {
            lines.push(format!("pbf: {}", path.display()));
        }
        if let Some(path) = &plan.state.osc_path {
            lines.push(format!("osc: {}", path.display()));
        }
        if plan.state.osc_paths.len() > 1 {
            lines.push(format!(
                "osc range: {} files ({} .. {})",
                plan.state.osc_paths.len(),
                plan.state
                    .osc_paths
                    .first()
                    .map_or(String::new(), |path| path.display().to_string()),
                plan.state
                    .osc_paths
                    .last()
                    .map_or(String::new(), |path| path.display().to_string()),
            ));
        }
        if let Some(path) = &plan.state.pbf_b_path {
            lines.push(format!("pbf_b: {}", path.display()));
        }
        if let Some(bbox) = &plan.state.bbox {
            lines.push(format!("bbox: {bbox}"));
        }
        Ok(lines)
    }
    fn announce(
        &self,
        req: &MeasureRequest<'_>,
        plan: &CommandPlan<CommandContext>,
    ) -> Result<(), DevError> {
        let (_, file_mb) = result_input_metadata(self.command, &plan.state)?;
        output::bench_msg(&format!(
            "{} ({file_mb:.0} MB), {} run(s)",
            self.command.id(),
            req.runs(),
        ));
        Ok(())
    }
    fn prepare(
        &self,
        req: &MeasureRequest<'_>,
        _ctx: &MeasurementSession,
        plan: &CommandPlan<CommandContext>,
    ) -> Result<(), DevError> {
        cleanup_output(self.command, &plan.state, arg_mode(req))?;
        self.command.prepare_scratch(&plan.state)
    }
    fn finish(
        &self,
        req: &MeasureRequest<'_>,
        ctx: &MeasurementSession,
        plan: &CommandPlan<CommandContext>,
    ) -> Result<(), DevError> {
        promote_artifact(
            self.command,
            &plan.state,
            arg_mode(req),
            &ctx.paths,
            req.project_root.as_path(),
        )?;
        cleanup_output(self.command, &plan.state, arg_mode(req))
    }
}

fn arg_mode(req: &MeasureRequest<'_>) -> ArgMode {
    if matches!(
        req.mode,
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
    ) {
        ArgMode::Hotpath
    } else {
        ArgMode::Bench
    }
}

/// Pre-flight validation for `--as-snapshot KEY`: bootstrap config (cheap;
/// cargo metadata + brokkr.toml parse, no compile) and check that KEY is a
/// valid snapshot key and either unused or `--replace-snapshot` was passed.
/// No-op when `as_snapshot` is unset.
fn preflight_as_snapshot(
    command: &PbfhoggCommand,
    req: &MeasureRequest,
    extra_params: &CommandParams,
) -> Result<(), DevError> {
    let Some(snap_key) = extra_params.as_snapshot.as_deref() else {
        return Ok(());
    };

    require_snapshot_output(command)?;

    let pi = crate::context::bootstrap(req.build_root)?;
    let paths = crate::context::bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)?;

    crate::pbfhogg::download::preflight_snapshot_collision(
        snap_key,
        extra_params.replace_snapshot,
        req.dataset,
        paths.datasets.get(req.dataset),
    )
}

/// Snapshot registration accepts a produced PBF file, not an empty path, an
/// OSC stream, or a directory output.
fn require_snapshot_output(command: &PbfhoggCommand) -> Result<(), DevError> {
    if !matches!(command.output_kind(), OutputKind::ScratchPbf) {
        return Err(DevError::Config(format!(
            "--as-snapshot requires a PBF-producing command; '{}' does not produce a PBF file",
            command.id(),
        )));
    }
    Ok(())
}

/// Run a single pbfhogg command via the wall-clock harness.
///
/// The suite reuses its already-built session. It produces the same row
/// identity and argv as individual command plans; the suite owns promotion
/// after the final sample.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_wallclock_core(
    harness: &harness::BenchHarness,
    binary: &Path,
    command: &PbfhoggCommand,
    cmd_ctx: &CommandContext,
    runs: usize,
    project_root: &Path,
    announce: bool,
) -> Result<(), DevError> {
    let args = command.build_args(cmd_ctx, ArgMode::Bench)?;
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let (basename, file_mb) = result_input_metadata(command, cmd_ctx)?;

    let config = BenchConfig {
        command: command.id().into(),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: None,
        runs,
        cli_args: Some(harness::format_cli_args(
            &binary.display().to_string(),
            &arg_refs,
        )),
        metadata: command.metadata(cmd_ctx),
    };

    if announce {
        output::bench_msg(&format!(
            "{} ({file_mb:.0} MB), {runs} run(s)",
            command.id(),
        ));
    }

    harness.run_external_prepared(
        &config,
        binary,
        &arg_refs,
        project_root,
        command.ok_exit_codes(),
        || {
            cleanup_output(command, cmd_ctx, ArgMode::Bench)?;
            command.prepare_scratch(cmd_ctx)
        },
    )?;
    Ok(())
}

/// Select and account for the physical inputs represented by the result row.
/// Most commands use one PBF; `merge-changes` consumes its resolved OSC file
/// or files.
fn result_input_metadata(
    command: &PbfhoggCommand,
    cmd_ctx: &CommandContext,
) -> Result<(String, f64), DevError> {
    if command.input_kind() == InputKind::OscOnly {
        let input_paths: Vec<&Path> = if cmd_ctx.osc_paths.is_empty() {
            vec![
                cmd_ctx
                    .osc_path
                    .as_deref()
                    .ok_or_else(|| DevError::Config("OSC path is required but not set".into()))?,
            ]
        } else {
            cmd_ctx
                .osc_paths
                .iter()
                .map(std::path::PathBuf::as_path)
                .collect()
        };
        let mut file_mb = 0.0;
        let mut basenames = Vec::with_capacity(input_paths.len());
        for path in input_paths {
            file_mb += resolve::file_size_mb(path)?;
            basenames.push(path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ));
        }
        Ok((basenames.join(","), file_mb))
    } else {
        let pbf_path = cmd_ctx
            .pbf_path
            .as_deref()
            .ok_or_else(|| DevError::Config("PBF path is required but not set".into()))?;
        Ok((cmd_ctx.pbf_basename()?, resolve::file_size_mb(pbf_path)?))
    }
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
    let snapshot_ref = resolve::SnapshotRef::from_opt(extra_params.snapshot.as_deref())?;

    // Resolve only inputs this command consumes. In particular, merge-changes
    // takes OSC files only; an absent or unregistered PBF must not block it.
    let pbf_path = if command.input_kind() == InputKind::OscOnly {
        None
    } else {
        Some(resolve::resolve_snapshot_pbf_path(
            req.dataset,
            &snapshot_ref,
            req.variant,
            paths,
            req.project_root.as_path(),
        )?)
    };

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
                req.project_root.as_path(),
            )?;
            (paths_vec.first().cloned(), paths_vec, None)
        } else {
            let (path, seq_key) = resolve::resolve_single_osc(
                req.dataset,
                &snapshot_ref,
                osc_seq,
                paths,
                req.project_root.as_path(),
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
        pbf_path.as_deref(),
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
    pbf_path: Option<&Path>,
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
    let pbf_path = pbf_path
        .ok_or_else(|| DevError::Config("merged PBF requires a primary PBF file".into()))?;
    let osc = osc_path.ok_or_else(|| DevError::Config("merged PBF requires an OSC file".into()))?;
    let osc_seq_str = single_osc_seq
        .ok_or_else(|| DevError::Config("merged PBF requires a single OSC seq".into()))?;
    // Snapshot key for the cache key. Different snapshots produce different
    // merged PBFs from the same OSC seq, so the cache must disambiguate.
    let snapshot_key = match snapshot_ref {
        resolve::SnapshotRef::Base => "base",
        resolve::SnapshotRef::Named(k) => k.as_str(),
    };

    if req.dry_run {
        // Synthesize the would-be merged path through the same key function
        // `ensure_merged_pbf` uses, without running apply-changes. The
        // existence of the cache file is irrelevant to dry-run - we just
        // need a populated `pbf_b_path` so `build_args` can reference it.
        return Ok(Some(merged_pbf_path(
            &paths.scratch_dir,
            pbf_path,
            snapshot_key,
            osc_seq_str,
        )));
    }

    let force_rebuild = !matches!(req.mode, MeasureMode::Run) && !extra_params.keep_cache;
    let (merged, state) = ensure_merged_pbf(
        binary,
        pbf_path,
        osc,
        snapshot_key,
        osc_seq_str,
        &paths.scratch_dir,
        req.project_root.as_path(),
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
/// via the snapshot resolver - `pbf_path` is the `--from` side, `pbf_b_path`
/// is the `--to` side. The `--variant` is applied symmetrically to both.
fn build_diff_snapshots_context(
    req: &MeasureRequest,
    binary: &Path,
    paths: &config::ResolvedPaths,
    extra_params: &CommandParams,
) -> Result<CommandContext, DevError> {
    let from_str = extra_params
        .from_snapshot
        .as_deref()
        .ok_or_else(|| DevError::Config("diff-snapshots requires --from".into()))?;
    let to_str = extra_params
        .to_snapshot
        .as_deref()
        .ok_or_else(|| DevError::Config("diff-snapshots requires --to".into()))?;

    let from_ref = resolve::SnapshotRef::parse(from_str)?;
    let to_ref = resolve::SnapshotRef::parse(to_str)?;

    let from_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        &from_ref,
        req.variant,
        paths,
        req.project_root.as_path(),
    )?;
    let to_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        &to_ref,
        req.variant,
        paths,
        req.project_root.as_path(),
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
        pbf_path: Some(from_path),
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
/// If `--as-snapshot KEY` was passed, move the scratch artifact into the
/// dataset's data dir and register a `[..snapshot.<KEY>...]` block in
/// `brokkr.toml`. Called after a successful measured/run completes; no-op
/// when `as_snapshot` is unset.
///
/// Variant routing: `degrade --strip-indexdata` writes under `pbf.raw` (the
/// output has no indexdata); everything else writes under `pbf.indexed`.
fn promote_artifact(
    command: &PbfhoggCommand,
    ctx: &CommandContext,
    mode: ArgMode,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(), DevError> {
    let Some(snap_key) = ctx.params.as_snapshot.as_deref() else {
        return Ok(());
    };

    require_snapshot_output(command)?;
    let scratch_pbf = crate::pbfhogg::commands::scratch_output_path(ctx, command, mode)?;
    let target_variant = match command {
        PbfhoggCommand::Degrade {
            strip_indexdata: true,
            ..
        } => "raw",
        _ => "indexed",
    };
    let dataset = paths.datasets.get(&ctx.dataset);

    crate::pbfhogg::download::promote_snapshot(
        project_root,
        &paths.hostname,
        &ctx.dataset,
        snap_key,
        ctx.params.replace_snapshot,
        &scratch_pbf,
        target_variant,
        dataset,
        &paths.data_dir,
    )
}

pub(crate) fn cleanup_output(
    command: &PbfhoggCommand,
    ctx: &CommandContext,
    mode: ArgMode,
) -> Result<(), DevError> {
    match command.output_kind() {
        OutputKind::ScratchPbf | OutputKind::ScratchOsc => {
            std::fs::remove_file(crate::pbfhogg::commands::scratch_output_path(
                ctx, command, mode,
            )?)
            .ok();
        }
        OutputKind::ScratchDir { .. } => {
            std::fs::remove_dir_all(crate::pbfhogg::commands::scratch_output_path(
                ctx, command, mode,
            )?)
            .ok();
        }
        OutputKind::None => {}
    }
    if matches!(command, PbfhoggCommand::MultiExtract { .. }) {
        std::fs::remove_file(crate::pbfhogg::commands::multi_extract_config_path(
            &ctx.scratch_dir,
        ))
        .ok();
    }
    Ok(())
}

/// State of the merged-PBF cache after `ensure_merged_pbf` returns.
///
/// Carried into the result row's metadata so `brokkr results <uuid>` can
/// distinguish runs that paid the apply-changes setup cost from runs that
/// reused a cached file.
#[derive(Debug, Clone, Copy)]
pub(crate) enum MergedCacheState {
    /// The merged PBF was reused from a prior run.
    Hit { age_secs: u64 },
    /// The merged PBF was generated by this invocation.
    Miss,
}

/// The merged-PBF cache path: the one spelling of its key.
///
/// `<pbf-stem>-snap<key>-osc<seq>-bench-merged.osm.pbf`. Including snapshot +
/// seq prevents silent wrong-file reuse across runs that target different
/// snapshots or different osc seqs. Dry-run, the single-command path and the
/// suite all name the file through here - a second spelling is how the suite
/// once cached under `<stem>-bench-merged` and reused a merge of a different
/// OSC.
pub(crate) fn merged_pbf_path(
    scratch_dir: &Path,
    pbf_path: &Path,
    snapshot_key: &str,
    osc_seq: &str,
) -> std::path::PathBuf {
    // The layout owns the spelling, so clean and the writers agree on it.
    crate::clean::layout::PbfhoggLayout::merged_pbf_cache(
        scratch_dir,
        pbf_path,
        snapshot_key,
        osc_seq,
    )
}

/// Ensure a merged PBF exists in the scratch directory. Returns the path and
/// the cache state (hit/miss + age on hit).
///
/// Keyed by [`merged_pbf_path`]. If `force_rebuild` is set, any existing
/// cached file is deleted before checking - used by measured modes
/// (bench/hotpath/alloc, and the suite) to make total invocation wall time
/// deterministic regardless of prior session state. It is also the only
/// defence against a primary PBF refreshed in place under the same stem,
/// which the key cannot see.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_merged_pbf(
    binary: &Path,
    pbf_path: &Path,
    osc_path: &Path,
    snapshot_key: &str,
    osc_seq: &str,
    scratch_dir: &Path,
    project_root: &Path,
    force_rebuild: bool,
) -> Result<(std::path::PathBuf, MergedCacheState), DevError> {
    let merged_path = merged_pbf_path(scratch_dir, pbf_path, snapshot_key, osc_seq);
    let merged_name = merged_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("merged.osm.pbf")
        .to_owned();

    if force_rebuild && merged_path.exists() {
        output::bench_msg(&format!(
            "force-rebuilding merged PBF (cache nuked by measured mode): {merged_name}"
        ));
        std::fs::remove_file(&merged_path)
            .map_err(|e| DevError::Config(format!("failed to remove cached merged PBF: {e}")))?;
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
    // Write under a partial name and rename into the cache slot only on
    // success: the cache test above is bare existence, so an apply-changes
    // killed mid-write must not leave a truncated file for the next
    // non-measured run to accept as a hit. The partial name keeps the
    // `.osm.pbf` suffix so the output format is unchanged. Dropping `staged`
    // on any error removes the partial file.
    let staged = crate::atomic_write::Staged::stable_keeping(&merged_path, ".osm.pbf")?;
    let partial_str = staged
        .tmp_path()
        .to_str()
        .ok_or_else(|| DevError::Config("merged path not UTF-8".into()))?
        .to_owned();
    let binary_str = binary.display().to_string();

    let start = std::time::Instant::now();
    // The merged cache is the default apply-changes output for this PBF/OSC
    // pair. Its argv shares the command's builder, with no optional transform,
    // job, compression or I/O parameters inherited from the diff invocation.
    let apply_args = PbfhoggCommand::apply_changes_args(
        pbf_str,
        osc_str,
        &partial_str,
        &CommandParams::default(),
        false,
    );
    let apply_arg_refs: Vec<&str> = apply_args.iter().map(String::as_str).collect();
    let captured = output::run_captured(&binary_str, &apply_arg_refs, project_root)?;

    captured.check_success(&binary_str)?;
    staged.commit().map_err(|e| {
        DevError::Config(format!(
            "failed to move merged PBF into place ({}): {e}",
            merged_path.display()
        ))
    })?;
    let elapsed_s = start.elapsed().as_secs();
    output::bench_msg(&format!("merged PBF ready in {elapsed_s}s"));

    Ok((merged_path, MergedCacheState::Miss))
}
