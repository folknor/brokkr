//! Pbfhogg input resolution and command plans.
use std::path::Path;

use crate::config;
use crate::context::MeasurementSession;
use crate::datasets::resolve::{self, resolve_bbox};
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::runner::{CommandPlan, PlanTarget, PlannedCommand, RowSubject};
use crate::measure::{Executable, Invocation, MeasureMode, MeasureRequest};
use crate::output;
use crate::output::child::Exits;
use crate::pbfhogg::binary::{OVERWRITE_FLAG, PbfhoggBinary, STAGING_DIR_FLAG, TMP_DIR_FLAG};
use crate::pbfhogg::commands::{ArgMode, CommandArgv, OutputKind, PbfhoggCommand, TagsFilterInput};
use crate::pbfhogg::context::{
    BenchSelection, CommandContext, CommandObservations, CommandParams, MergedCacheState,
    ResolvedInputs, SnapshotSide, TwoPbfOrigin,
};
use crate::pbfhogg::expect::{self, Expectation};
use crate::project::Project;

/// Resolve cargo features for the selected I/O mode and run its preflight
/// checks. The command arg builder reads the same parameters to construct argv.
fn resolve_io_features(
    command: &PbfhoggCommand,
    extra_params: &CommandParams,
) -> Result<Vec<&'static str>, DevError> {
    let io = extra_params.io;

    if io.io_uring && !command.supports_io_uring() {
        return Err(DevError::Config(format!(
            "--io-uring is not supported by '{}'",
            command.id(),
        )));
    }

    if io.io_uring {
        crate::preflight::run_preflight(&crate::preflight::uring_checks())?;
    }

    Ok(io.features())
}

pub(crate) struct PbfhoggPlan<'a> {
    pub command: &'a PbfhoggCommand,
    pub osc_seq: Option<&'a str>,
    pub params: &'a CommandParams,
}

/// A planned pbfhogg command: its resolved inputs, its argv with the
/// overwrite decision the building arm made, and - for a real run - the one
/// handle on the built executable that both planning-time setup (the merged
/// diff input) and [`PlannedCommand::materialize`] resolve through. It lives
/// as long as the plan, so through the last sample.
pub(crate) struct PbfhoggState {
    pub ctx: CommandContext,
    handle: Option<PbfhoggBinary>,
    /// The exits a run stores as completed, for this executable and input
    /// ([`expect::completed_exits`]); the command's own set for a dry run.
    exits: Exits,
    /// The answer brokkr.toml declares for this input, if any.
    expectation: Option<Expectation>,
}

impl PlannedCommand for PbfhoggPlan<'_> {
    type State = PbfhoggState;
    type Argv = CommandArgv;
    fn project(&self) -> Project {
        Project::Pbfhogg
    }
    fn id(&self) -> &'static str {
        self.command.id()
    }
    fn package(&self) -> Option<&'static str> {
        Project::Pbfhogg.cli_package()
    }
    fn supports_profiling(&self) -> bool {
        true
    }
    fn exits(&self, plan: &CommandPlan<PbfhoggState, CommandArgv>) -> Exits {
        plan.state.exits
    }
    fn explain(&self, plan: &CommandPlan<PbfhoggState, CommandArgv>, error: DevError) -> DevError {
        expect::explain(self.command, plan.state.expectation.as_ref(), error)
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
        target: PlanTarget<'_>,
    ) -> Result<CommandPlan<PbfhoggState, CommandArgv>, DevError> {
        let binary = match target {
            PlanTarget::DryRun => {
                preflight_as_snapshot(self.command, req, self.params)?;
                resolve_io_features(self.command, self.params)?;
                None
            }
            PlanTarget::Built(executable) => Some(PbfhoggBinary::from_executable(
                executable,
                req.project_root.as_path(),
            )?),
            PlanTarget::Unbound => {
                return Err(DevError::Internal(
                    "a pbfhogg plan needs the built binary".into(),
                ));
            }
        };
        let expectation = expect::resolve(
            self.command,
            req.dataset,
            req.variant,
            self.osc_seq,
            self.params,
            paths,
            req.project_root.as_path(),
        )?;
        // Before the context: a run that could not pass is refused before
        // a diff's merged input is built for it.
        let exits = match &binary {
            Some(binary) => expect::completed_exits(self.command, binary, expectation.as_ref())?,
            None => expect::dry_run_exits(self.command),
        };
        let ctx = build_pbfhogg_context(
            req,
            self.command,
            self.osc_seq,
            binary.as_ref(),
            paths,
            self.params,
            expectation.is_some(),
        )?;
        let mode = arg_mode(req);
        let argv = self.command.build_args(&ctx, mode)?;
        let (basename, mb) = result_input_metadata(&ctx)?;
        Ok(CommandPlan {
            argv,
            scratch_dirs: vec![],
            subject: RowSubject {
                command: self.command.id().into(),
                input_file: Some(basename),
                input_mb: Some(mb),
                cli_args: None,
                metadata: self.command.metadata(&ctx.observations),
                arm_qualifier: self.command.arm_qualifier(&ctx),
            },
            state: PbfhoggState {
                ctx,
                handle: binary,
                exits,
                expectation,
            },
        })
    }
    fn materialize<'p>(
        &self,
        plan: &'p CommandPlan<PbfhoggState, CommandArgv>,
        _executable: Executable<'p>,
    ) -> Result<Invocation<'p>, DevError> {
        // The handle planning built from that same executable resolves the
        // planned argv; the runner's copy of the path is not used.
        plan.state
            .handle
            .as_ref()
            .ok_or_else(|| DevError::Internal("pbfhogg plan was resolved without a binary".into()))?
            .invocation(plan.argv.clone())
    }
    fn describe(
        &self,
        req: &MeasureRequest<'_>,
        plan: &CommandPlan<PbfhoggState, CommandArgv>,
    ) -> Result<Vec<String>, DevError> {
        // The selected argv was built by `plan`; check the alternate shape as
        // well so a dry run catches missing inputs in either supported mode.
        let other_mode = match arg_mode(req) {
            ArgMode::Bench => ArgMode::Hotpath,
            ArgMode::Hotpath => ArgMode::Bench,
        };
        let _ = self.command.build_args(&plan.state.ctx, other_mode)?;

        let mut lines = vec![format!(
            "{} args: {}",
            self.command.id(),
            plan.argv.args().join(" ")
        )];
        if plan.argv.negotiates_overwrite() {
            lines.push(format!(
                "{OVERWRITE_FLAG}: added at run time if the built binary accepts it"
            ));
        }
        if plan.argv.writes_output() || plan.argv.writes_scratch() {
            lines.push(format!(
                "{STAGING_DIR_FLAG}, {TMP_DIR_FLAG}: a private directory per launch, added at run \
                 time where the built binary accepts them"
            ));
        }
        let state = &plan.state.ctx;
        if let Some(path) = state.inputs.primary_pbf() {
            lines.push(format!("pbf: {}", path.display()));
        }
        let osc_paths = state.inputs.osc_paths();
        if let Some(path) = osc_paths.first() {
            lines.push(format!("osc: {}", path.display()));
        }
        if osc_paths.len() > 1 {
            lines.push(format!(
                "osc range: {} files ({} .. {})",
                osc_paths.len(),
                osc_paths
                    .first()
                    .map_or(String::new(), |path| path.display().to_string()),
                osc_paths
                    .last()
                    .map_or(String::new(), |path| path.display().to_string()),
            ));
        }
        if let Some(path) = state.inputs.second_pbf() {
            lines.push(format!("pbf_b: {}", path.display()));
        }
        if let Some(bbox) = state.inputs.bbox() {
            lines.push(format!("bbox: {bbox}"));
        }
        if let Some(expectation) = &plan.state.expectation {
            lines.push(format!(
                "expects: {} ({}); the other answer fails the run",
                expectation.verdict, expectation.source
            ));
        }
        Ok(lines)
    }
    fn announce(
        &self,
        req: &MeasureRequest<'_>,
        plan: &CommandPlan<PbfhoggState, CommandArgv>,
    ) -> Result<(), DevError> {
        let (_, file_mb) = result_input_metadata(&plan.state.ctx)?;
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
        plan: &CommandPlan<PbfhoggState, CommandArgv>,
    ) -> Result<(), DevError> {
        cleanup_output(self.command, &plan.state.ctx, arg_mode(req))?;
        self.command.prepare_scratch(&plan.state.ctx)
    }
    fn finish(
        &self,
        req: &MeasureRequest<'_>,
        ctx: &MeasurementSession,
        plan: &CommandPlan<PbfhoggState, CommandArgv>,
    ) -> Result<(), DevError> {
        promote_artifact(
            self.command,
            &plan.state.ctx,
            arg_mode(req),
            &ctx.paths,
            req.project_root.as_path(),
            req.variant,
        )?;
        cleanup_output(self.command, &plan.state.ctx, arg_mode(req))
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
    binary: &PbfhoggBinary,
    command: &PbfhoggCommand,
    cmd_ctx: &CommandContext,
    runs: usize,
    project_root: &Path,
    announce: bool,
    exits: Exits,
    expectation: Option<&Expectation>,
) -> Result<(), DevError> {
    let invocation = binary.invocation(command.build_args(cmd_ctx, ArgMode::Bench)?)?;

    let (basename, file_mb) = result_input_metadata(cmd_ctx)?;

    let config = BenchConfig {
        command: command.id().into(),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: None,
        runs,
        cli_args: None,
        metadata: command.metadata(&cmd_ctx.observations),
        arm_qualifier: command.arm_qualifier(cmd_ctx),
    };

    if announce {
        output::bench_msg(&format!(
            "{} ({file_mb:.0} MB), {runs} run(s)",
            command.id(),
        ));
    }

    harness
        .run_external_prepared(&config, &invocation, project_root, exits, || {
            cleanup_output(command, cmd_ctx, ArgMode::Bench)?;
            command.prepare_scratch(cmd_ctx)
        })
        .map_err(|error| expect::explain(command, expectation, error))?;
    Ok(())
}

/// Select and account for the physical inputs represented by the result row.
/// Most commands use one PBF; `merge-changes` consumes its resolved OSC file
/// or files.
fn result_input_metadata(cmd_ctx: &CommandContext) -> Result<(String, f64), DevError> {
    if let ResolvedInputs::OscOnly { oscs } = &cmd_ctx.inputs {
        let input_paths: Vec<&Path> = oscs.iter().map(std::path::PathBuf::as_path).collect();
        if input_paths.is_empty() {
            return Err(DevError::Internal(
                "OSC-only pbfhogg input has no resolved files".into(),
            ));
        }
        let mut file_mb = 0.0;
        let mut basenames = Vec::with_capacity(input_paths.len());
        for path in input_paths {
            file_mb += resolve::file_size_mb(path)?;
            basenames.push(crate::utf8_path::file_name(path));
        }
        Ok((basenames.join(","), file_mb))
    } else {
        let pbf_path = cmd_ctx
            .inputs
            .primary_pbf()
            .ok_or_else(|| DevError::Internal("pbfhogg input has no primary PBF".into()))?;
        Ok((cmd_ctx.pbf_basename()?, resolve::file_size_mb(pbf_path)?))
    }
}

/// Build the `CommandContext` for a pbfhogg command, resolving all input paths.
/// `binary` is `None` for a dry run, which builds nothing it could run.
/// `expected` says the input declares its answer, so a `diff`'s merged input
/// is rebuilt by this build rather than taken from the cache.
fn build_pbfhogg_context(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    osc_seq: Option<&str>,
    binary: Option<&PbfhoggBinary>,
    paths: &config::ResolvedPaths,
    extra_params: &CommandParams,
    expected: bool,
) -> Result<CommandContext, DevError> {
    if matches!(command, PbfhoggCommand::DiffSnapshots { .. }) {
        return build_diff_snapshots_context(req, paths, extra_params);
    }

    // Carry the CLI snapshot selection into dataset resolution.
    // None or "base" → SnapshotRef::Base (legacy top-level data, current
    // behavior preserved). Anything else → Named, validated.
    let snapshot_ref = resolve::SnapshotRef::from_opt(extra_params.snapshot.as_ref());

    let pbf_path = if command.needs_pbf() {
        Some(resolve::resolve_snapshot_pbf_path(
            req.dataset,
            &snapshot_ref,
            req.variant,
            paths,
            req.project_root.as_path(),
        )?)
    } else {
        None
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
    let (osc_paths, single_osc_seq) = if command.needs_osc_for_setup() {
        if let Some(range) = &extra_params.osc_range {
            let paths_vec = resolve::resolve_osc_range(
                req.dataset,
                &snapshot_ref,
                range,
                paths,
                req.project_root.as_path(),
            )?;
            (paths_vec, None)
        } else {
            let (path, seq_key) = resolve::resolve_single_osc(
                req.dataset,
                &snapshot_ref,
                osc_seq,
                paths,
                req.project_root.as_path(),
            )?;
            (vec![path], Some(seq_key))
        }
    } else {
        (Vec::new(), None)
    };

    let osc_path = osc_paths.first().map(std::path::PathBuf::as_path);
    let (merged_pbf, merged_cache) = resolve_merged_pbf(
        req,
        command,
        binary,
        pbf_path.as_deref(),
        osc_path,
        single_osc_seq.as_deref(),
        &snapshot_ref,
        paths,
        extra_params,
        expected,
    )?;

    let inputs = match command {
        PbfhoggCommand::MergeChanges { .. }
        | PbfhoggCommand::TagsFilter {
            input: TagsFilterInput::Osc,
            ..
        } => ResolvedInputs::OscOnly { oscs: osc_paths },
        PbfhoggCommand::ApplyChanges => ResolvedInputs::PbfAndOsc {
            pbf: pbf_path.ok_or_else(|| {
                DevError::Internal("apply-changes requires a resolved PBF".into())
            })?,
            osc: osc_paths.into_iter().next().ok_or_else(|| {
                DevError::Internal("apply-changes requires a resolved OSC".into())
            })?,
        },
        PbfhoggCommand::Diff { .. } => ResolvedInputs::TwoPbfs {
            a: pbf_path
                .ok_or_else(|| DevError::Internal("diff requires a resolved primary PBF".into()))?,
            b: merged_pbf
                .ok_or_else(|| DevError::Internal("diff requires a resolved merged PBF".into()))?,
            origin: TwoPbfOrigin::Merged,
        },
        PbfhoggCommand::Extract { .. } | PbfhoggCommand::MultiExtract { .. } => {
            let bbox = resolve_bbox(extra_params.bbox.as_ref(), req.dataset, paths)?;
            ResolvedInputs::PbfAndBbox {
                pbf: pbf_path
                    .ok_or_else(|| DevError::Internal("extract requires a resolved PBF".into()))?,
                bbox,
            }
        }
        _ => ResolvedInputs::Pbf {
            pbf: pbf_path.ok_or_else(|| {
                DevError::Internal("pbfhogg command requires a resolved PBF".into())
            })?,
        },
    };

    let observations = CommandObservations {
        merged_cache,
        snapshot_to: None,
    };

    let mut params = extra_params.clone();
    params.selection = BenchSelection::for_dataset(paths.datasets.get(req.dataset));

    Ok(CommandContext {
        inputs,
        scratch_dir: paths.scratch_dir.clone(),
        dataset: req.dataset.to_owned(),
        params,
        observations,
    })
}

/// Resolve the B-side (merged) PBF path for diff commands.
///
/// In measured modes, cache rebuilding is deterministic unless
/// `--keep-cache` opts into reuse. Dry-run (no `binary`) resolves only the
/// would-be path.
#[allow(clippy::too_many_arguments)]
fn resolve_merged_pbf(
    req: &MeasureRequest,
    command: &PbfhoggCommand,
    binary: Option<&PbfhoggBinary>,
    pbf_path: Option<&Path>,
    osc_path: Option<&Path>,
    single_osc_seq: Option<&str>,
    snapshot_ref: &resolve::SnapshotRef,
    paths: &config::ResolvedPaths,
    extra_params: &CommandParams,
    expected: bool,
) -> Result<(Option<std::path::PathBuf>, Option<MergedCacheState>), DevError> {
    if !command.needs_merged_pbf() {
        return Ok((None, None));
    }
    let pbf_path =
        pbf_path.ok_or_else(|| DevError::Internal("diff requires a primary PBF".into()))?;
    let osc = osc_path.ok_or_else(|| DevError::Internal("diff requires an OSC file".into()))?;
    let osc_seq_str =
        single_osc_seq.ok_or_else(|| DevError::Config("diff requires one OSC sequence".into()))?;
    let snapshot_key = match snapshot_ref {
        resolve::SnapshotRef::Base => "base",
        resolve::SnapshotRef::Named(k) => k.as_str(),
    };

    let Some(binary) = binary else {
        return Ok((
            Some(merged_pbf_path(
                &paths.scratch_dir,
                pbf_path,
                snapshot_key,
                osc_seq_str,
            )),
            None,
        ));
    };

    let reuse = matches!(req.mode, MeasureMode::Run) || extra_params.keep_cache;
    // A declared answer is checked against the merge this build makes: a
    // cached merge is keyed by name, not by the OSC's content or the build
    // that made it, so it could answer for another input.
    if expected && reuse {
        output::bench_msg(
            "rebuilding the merged PBF: this diff's answer is declared, so a cached merge is not used",
        );
    }
    let force_rebuild = expected || !reuse;
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
    Ok((Some(merged), Some(state)))
}

/// Resolve both snapshot paths and retain typed metadata for the `--to` side.
fn build_diff_snapshots_context(
    req: &MeasureRequest,
    paths: &config::ResolvedPaths,
    extra_params: &CommandParams,
) -> Result<CommandContext, DevError> {
    let from_ref = extra_params
        .from_snapshot
        .as_ref()
        .ok_or_else(|| DevError::Config("diff-snapshots requires --from".into()))?;
    let to_ref = extra_params
        .to_snapshot
        .as_ref()
        .ok_or_else(|| DevError::Config("diff-snapshots requires --to".into()))?;

    let from_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        from_ref,
        req.variant,
        paths,
        req.project_root.as_path(),
    )?;
    let to_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        to_ref,
        req.variant,
        paths,
        req.project_root.as_path(),
    )?;
    let snapshot_to = SnapshotSide {
        file_name: crate::utf8_path::file_name_str(&to_path).map(str::to_owned),
        size_mb: Some(resolve::file_size_mb(&to_path)?),
    };

    Ok(CommandContext {
        inputs: ResolvedInputs::TwoPbfs {
            a: from_path,
            b: to_path,
            origin: TwoPbfOrigin::Snapshot,
        },
        scratch_dir: paths.scratch_dir.clone(),
        dataset: req.dataset.to_owned(),
        params: extra_params.clone(),
        observations: CommandObservations {
            merged_cache: None,
            snapshot_to: Some(snapshot_to),
        },
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
    variant: &str,
) -> Result<(), DevError> {
    let Some(snap_key) = ctx.params.as_snapshot.as_deref() else {
        return Ok(());
    };

    require_snapshot_output(command)?;
    let scratch_pbf = crate::pbfhogg::commands::scratch_output_path(ctx, command, mode)?;
    let target_variant = match command.descriptor().promotion_variant {
        crate::pbfhogg::commands::PromotionVariant::Raw => "raw",
        crate::pbfhogg::commands::PromotionVariant::Indexed => "indexed",
        crate::pbfhogg::commands::PromotionVariant::Unsupported => {
            return Err(DevError::Internal(format!(
                "{} cannot be promoted to a snapshot",
                command.id()
            )));
        }
    };
    let dataset = paths.datasets.get(&ctx.dataset);
    // repack round-trips every element (pbfhogg's repack contract), so its
    // output holds the same elements as the entry it read: recorded as a
    // `diff-snapshots` answer of same between the two files, when the
    // source has a registered digest to address it by. Nothing else is
    // derived; cat and degrade change what they copy.
    let same_as = if matches!(command, PbfhoggCommand::Repack { .. }) {
        let source = resolve::SnapshotRef::from_opt(ctx.params.snapshot.as_ref());
        dataset
            .and_then(|ds| match &source {
                resolve::SnapshotRef::Base => ds.pbf.get(variant),
                resolve::SnapshotRef::Named(key) => {
                    ds.snapshot.get(key).and_then(|snap| snap.pbf.get(variant))
                }
            })
            .and_then(|entry| entry.xxhash)
    } else {
        None
    };

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
        same_as,
    )
}

pub(crate) fn cleanup_output(
    command: &PbfhoggCommand,
    ctx: &CommandContext,
    mode: ArgMode,
) -> Result<(), DevError> {
    match command.output_kind() {
        OutputKind::ScratchPbf | OutputKind::ScratchOsc | OutputKind::ScratchText => {
            crate::reservations::remove_file(&crate::pbfhogg::commands::scratch_output_path(
                ctx, command, mode,
            )?)
            .ok();
        }
        OutputKind::ScratchDir { .. } => {
            crate::reservations::remove_dir_all(&crate::pbfhogg::commands::scratch_output_path(
                ctx, command, mode,
            )?)
            .ok();
        }
        OutputKind::None => {}
    }
    if matches!(command, PbfhoggCommand::MultiExtract { .. }) {
        crate::reservations::remove_file(&crate::pbfhogg::commands::multi_extract_config_path(
            &ctx.scratch_dir,
        ))
        .ok();
    }
    Ok(())
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
    binary: &PbfhoggBinary,
    pbf_path: &Path,
    osc_path: &Path,
    snapshot_key: &str,
    osc_seq: &str,
    scratch_dir: &Path,
    project_root: &Path,
    force_rebuild: bool,
) -> Result<(std::path::PathBuf, MergedCacheState), DevError> {
    let merged_path = merged_pbf_path(scratch_dir, pbf_path, snapshot_key, osc_seq);
    let merged_name = crate::utf8_path::file_name_str(&merged_path)
        .unwrap_or("merged.osm.pbf")
        .to_owned();

    if force_rebuild && merged_path.exists() {
        output::bench_msg(&format!(
            "force-rebuilding merged PBF (cache nuked by measured mode): {merged_name}"
        ));
        crate::reservations::remove_file(&merged_path)
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
    // Write under a partial name and rename into the cache slot only on
    // success: the cache test above is bare existence, so an apply-changes
    // killed mid-write must not leave a truncated file for the next
    // non-measured run to accept as a hit. The partial name keeps the
    // `.osm.pbf` suffix so the output format is unchanged. Dropping `staged`
    // on any error removes the partial file.
    let staged = crate::atomic_write::Staged::stable_keeping_for_child(&merged_path, ".osm.pbf")?;

    let start = std::time::Instant::now();
    // The merged cache is the default apply-changes output for this PBF/OSC
    // pair. Its argv shares the command's builder, with no optional transform,
    // job, compression or I/O parameters inherited from the diff invocation.
    // The partial name is owned: a killed run leaves it for the retry to
    // replace.
    let invocation = binary.invocation(PbfhoggCommand::apply_changes_args(
        pbf_path,
        osc_path,
        staged.tmp_path(),
        &CommandParams::default(),
    )?)?;
    let captured = invocation.capture_in(project_root)?;
    invocation.check(&captured)?;
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

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
