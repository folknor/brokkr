//! Shared execution policy. Dry-run resolves a plan without materializing tools
//! or run artifacts (input verification may refresh its digest cache);
//! run never records, and failed samples retain their artifacts.

use crate::context::MeasurementSession;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{Executable, Invocation, MeasureMode, MeasureRequest};
use crate::output;
use crate::project::{self, Project};
use std::path::{Path, PathBuf};

pub(crate) struct RowSubject {
    pub command: String,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub cli_args: Option<String>,
    pub metadata: Vec<KvPair>,
}

impl RowSubject {
    pub(crate) fn config(&self, runs: usize) -> BenchConfig {
        BenchConfig {
            command: self.command.clone(),
            input_file: self.input_file.clone(),
            input_mb: self.input_mb,
            cargo_features: None,
            external_build: None,
            runs,
            cli_args: self.cli_args.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

pub(crate) enum PlanKind {
    Binary,
    External,
    Lifecycle,
}

/// What a plan is resolved against. The runner is the only place that picks
/// it, from `req.dry_run`, so a plan never consults a second dry-run flag.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PlanTarget<'a> {
    /// `--dry-run`: nothing was built. Resolve and validate only; a plan must
    /// not spawn, probe or materialise anything against an executable.
    DryRun,
    /// A real run of a [`PlanKind::Binary`] plan, against the executable the
    /// runner just built, handed over opaque. The runner builds nothing else
    /// until the plan's last sample has run.
    Built(Executable<'a>),
    /// A real run of an external or lifecycle plan, which spawns its own
    /// programs from [`PlannedCommand::execute`].
    Unbound,
}

/// A resolved plan. `argv` is the one planned argument value: dry run
/// prints its strings, and [`PlannedCommand::materialize`] resolves that
/// same value, so the two cannot drift. A project whose argv carries more
/// than strings (pbfhogg's overwrite decision) uses its own type.
pub(crate) struct CommandPlan<S, A = Vec<String>> {
    pub argv: A,
    pub scratch_dirs: Vec<PathBuf>,
    pub subject: RowSubject,
    pub state: S,
}

/// Project code supplies resolution, build shape and lifecycle operations;
/// mode, recording and the success boundary belong to this runner.
pub(crate) trait PlannedCommand {
    type State;
    /// The planned argv. `AsRef` is for observation (dry run) only; turning
    /// it into something that runs is [`materialize`](Self::materialize)'s
    /// job, which every plan implements explicitly.
    type Argv: AsRef<[String]>;
    fn project(&self) -> Project;
    fn id(&self) -> &'static str;
    fn package(&self) -> Option<&'static str>;
    fn kind(&self) -> PlanKind {
        PlanKind::Binary
    }
    fn supports_profiling(&self) -> bool {
        true
    }
    fn build_config(
        &self,
        req: &MeasureRequest<'_>,
    ) -> Result<Option<crate::build::BuildConfig>, DevError> {
        let features = if profiling(req) {
            req.build_features()
        } else {
            req.feat_refs()
        };
        Ok(Some(crate::build::BuildConfig::release_with_features(
            self.package(),
            &features,
        )))
    }
    fn plan(
        &self,
        req: &MeasureRequest<'_>,
        paths: &crate::config::ResolvedPaths,
        target: PlanTarget<'_>,
    ) -> Result<CommandPlan<Self::State, Self::Argv>, DevError>;
    /// Resolve a binary plan into what the runner executes. Called only for
    /// a [`PlanTarget::Built`] plan, with that same executable. No default:
    /// a plan pairs its argv with the executable itself
    /// ([`Invocation::built`]), or resolves flags the executable negotiates
    /// through its own handle; a non-binary plan refuses.
    fn materialize<'p>(
        &self,
        plan: &'p CommandPlan<Self::State, Self::Argv>,
        executable: Executable<'p>,
    ) -> Result<Invocation<'p>, DevError>;
    /// Return the resolved command description shown by `--dry-run`.
    /// Projects can add resolved inputs after validating any alternate argv
    /// shapes they support.
    fn describe(
        &self,
        _req: &MeasureRequest<'_>,
        plan: &CommandPlan<Self::State, Self::Argv>,
    ) -> Result<Vec<String>, DevError> {
        Ok(vec![format!(
            "{} args: {}",
            self.id(),
            plan.argv.as_ref().join(" ")
        )])
    }
    /// Announce an individual command immediately before its benchmark runs.
    fn announce(
        &self,
        _req: &MeasureRequest<'_>,
        _plan: &CommandPlan<Self::State, Self::Argv>,
    ) -> Result<(), DevError> {
        Ok(())
    }
    fn prepare(
        &self,
        _req: &MeasureRequest<'_>,
        _ctx: &MeasurementSession,
        _plan: &CommandPlan<Self::State, Self::Argv>,
    ) -> Result<(), DevError> {
        Ok(())
    }
    fn finish(
        &self,
        _req: &MeasureRequest<'_>,
        _ctx: &MeasurementSession,
        _plan: &CommandPlan<Self::State, Self::Argv>,
    ) -> Result<(), DevError> {
        Ok(())
    }
    fn ok_codes(&self) -> &[i32] {
        &[]
    }
    fn env(&self) -> Vec<(&str, &str)> {
        vec![]
    }
    fn observe(&self, _result: &mut harness::BenchResult, _stderr: &[u8]) {}
    fn execute(
        &self,
        _req: &MeasureRequest<'_>,
        _ctx: &MeasurementSession,
        _plan: &CommandPlan<Self::State, Self::Argv>,
    ) -> Result<(), DevError> {
        Err(DevError::Internal("plan has no lifecycle executor".into()))
    }
}

/// The `materialize` a non-binary plan kind gives: the runner never calls it
/// for one, so reaching it is a runner bug, refused rather than answered
/// with a made-up invocation.
pub(crate) fn not_a_binary_plan(id: &str) -> DevError {
    DevError::Internal(format!(
        "{id} is not a binary plan; the runner executes it, not an invocation"
    ))
}

fn profiling(req: &MeasureRequest<'_>) -> bool {
    matches!(
        req.mode,
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
    )
}

pub(crate) fn run(req: &MeasureRequest<'_>, command: &impl PlannedCommand) -> Result<(), DevError> {
    project::require(&req.project, &command.project(), command.id())?;
    if profiling(req) && !command.supports_profiling() {
        return Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id()
        )));
    }
    if req.dry_run {
        let pi = crate::context::bootstrap(req.build_root)?;
        let paths =
            crate::context::bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)?;
        let plan = command.plan(req, &paths, PlanTarget::DryRun)?;
        for line in command.describe(req, &plan)? {
            output::dry_run_msg(&line);
        }
        output::dry_run_msg("ok");
        return Ok(());
    }
    let ctx = match command.build_config(req)? {
        Some(config) => MeasurementSession::with_build_config_for_request(req, &config)?,
        None => MeasurementSession::new_for_request(req)?,
    };
    let binary = if matches!(command.kind(), PlanKind::Binary) {
        Some(ctx.executable()?)
    } else {
        None
    };
    let plan = command.plan(
        req,
        &ctx.paths,
        binary.map_or(PlanTarget::Unbound, PlanTarget::Built),
    )?;
    let gated = gated_for(req);
    let prepare = || -> Result<(), DevError> {
        for dir in &plan.scratch_dirs {
            if dir.exists() {
                remove_tree(gated, dir)?;
            }
            std::fs::create_dir_all(dir)?;
        }
        command.prepare(req, &ctx, &plan)
    };
    if let Some(binary) = binary {
        let invocation = command.materialize(&plan, binary)?;
        execute_invocation(req, &ctx, command, &plan, &invocation, &prepare)?;
    } else {
        prepare()?;
        command.execute(req, &ctx, &plan)?;
    }
    // Promotion is part of success. If it fails, preserve the current output.
    command.finish(req, &ctx, &plan)?;
    for dir in &plan.scratch_dirs {
        remove_tree(gated, dir).ok();
    }
    Ok(())
}

/// Whether a measured run's scratch removals go through the reservation gate
/// (`crate::reservations`): for pbfhogg, whose launches they can reach; any
/// other project's are its own and stay outside the lease store.
pub(crate) fn gated_for(req: &MeasureRequest<'_>) -> bool {
    req.project == crate::project::Project::Pbfhogg
}

fn remove_tree(gated: bool, dir: &Path) -> std::io::Result<()> {
    if gated {
        crate::reservations::remove_dir_all(dir)
    } else {
        std::fs::remove_dir_all(dir) // gate: another project's own scratch
    }
}

fn remove_one_file(gated: bool, file: &Path) -> std::io::Result<()> {
    if gated {
        crate::reservations::remove_file(file)
    } else {
        std::fs::remove_file(file) // gate: another project's own scratch
    }
}

/// Run a binary plan's materialised invocation in the request's mode. Only
/// the invocation is spawned, never the bare binary: its program and argv are
/// what the row records and what runs.
fn execute_invocation<C: PlannedCommand, P: Fn() -> Result<(), DevError>>(
    req: &MeasureRequest<'_>,
    ctx: &MeasurementSession,
    command: &C,
    plan: &CommandPlan<C::State, C::Argv>,
    invocation: &Invocation<'_>,
    prepare: &P,
) -> Result<(), DevError> {
    invocation.require_no_env("the measure runner")?;
    let binary = invocation.program();
    let program = invocation.program_string();
    let cwd = req.project_root.as_path();
    let mut config = plan.subject.config(req.runs());
    config.cli_args = Some(invocation.cli_args());
    match req.mode {
        MeasureMode::Run => {
            prepare()?;
            let launched = invocation.launch(cwd)?;
            let args = launched.arg_refs();
            output::run_msg(&format!("{program} {}", args.join(" ")));
            let out = output::run_passthrough_timed(&program, &args, Some(ctx.harness.lock()));
            let finished = launched.finish();
            let out = match out {
                Ok(out) => out,
                Err(e) => {
                    if let Err(release) = finished {
                        output::warn(&release.to_string());
                    }
                    return Err(e);
                }
            };
            finished?;
            if let Some(code) = out.failed_code(command.ok_codes()) {
                return Err(DevError::ExitCode(code));
            }
            output::run_msg(&format!(
                "elapsed={}ms",
                crate::passthrough_cmd::duration_ms(out.elapsed)
            ));
        }
        MeasureMode::Bench { .. } => {
            command.announce(req, plan)?;
            ctx.harness.run_external_prepared(
                &config,
                invocation,
                req.project_root.as_path(),
                command.ok_codes(),
                prepare,
            )?;
        }
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            if let Some(profiling) = req.mode.profiled() {
                profiling.print_note();
            }
            ctx.harness.run_hotpath(&config, binary, |i, fifo| {
                prepare()?;
                // The report is this launch's disposable output: reserved
                // with it and removed with its lease, here or, if the child
                // outlives this run, by a later scan. A launch with no
                // reservation (an unreserved invocation) is removed here.
                let report = ctx.harness.new_hotpath_report_path()?;
                let launched = invocation.launch_with(cwd, std::slice::from_ref(&report))?;
                let iteration = ctx.harness.capture_hotpath_into(
                    binary,
                    &launched.arg_refs(),
                    cwd,
                    &command.env(),
                    command.ok_codes(),
                    fifo,
                    i,
                    None,
                    &report,
                );
                let finished = launched.finish();
                if finished.is_ok() {
                    crate::reservations::remove_file(&report).ok();
                }
                let mut iteration = iteration?;
                if let Err(error) = finished {
                    iteration.fail(error);
                }
                if iteration.error.is_none()
                    && let Some(result) = &mut iteration.result
                {
                    command.observe(result, &iteration.stderr);
                }
                Ok(iteration)
            })?;
        }
    }
    Ok(())
}

/// A tool installation may resolve the executable only during materialization.
/// Its argv and row identity are still planned without installing anything.
pub(crate) struct ExternalInvocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub config: BenchConfig,
    pub output_files: Vec<PathBuf>,
    pub scratch_dirs: Vec<PathBuf>,
}

impl ExternalInvocation {
    /// The program and argv as one [`Invocation`] for the harness.
    pub(crate) fn invocation(&self) -> Invocation<'_> {
        Invocation::new(&self.program, self.args.clone())
    }

    /// Clear the outputs and scratch before a sample; `gated` routes the
    /// removals through the reservation gate ([`gated_for`]).
    pub(crate) fn prepare_sample(&self, gated: bool) -> Result<(), DevError> {
        for file in &self.output_files {
            match remove_one_file(gated, file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        for dir in &self.scratch_dirs {
            if dir.exists() {
                remove_tree(gated, dir)?;
            }
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    pub(crate) fn cleanup(&self, gated: bool) {
        for file in &self.output_files {
            remove_one_file(gated, file).ok();
        }
        for dir in &self.scratch_dirs {
            remove_tree(gated, dir).ok();
        }
    }
}

pub(crate) fn execute_external(
    req: &MeasureRequest<'_>,
    ctx: &MeasurementSession,
    invocation: &ExternalInvocation,
) -> Result<(), DevError> {
    let args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
    let run = invocation.invocation();
    let gated = gated_for(req);
    let prepare = || invocation.prepare_sample(gated);
    match req.mode {
        MeasureMode::Run => {
            prepare()?;
            let program = invocation.program.display().to_string();
            output::run_msg(&format!("{program} {}", args.join(" ")));
            let out = output::run_passthrough_timed(&program, &args, Some(ctx.harness.lock()))?;
            if let Some(code) = out.failed_code(&[]) {
                return Err(DevError::ExitCode(code));
            }
            output::run_msg(&format!(
                "elapsed={}ms",
                crate::passthrough_cmd::duration_ms(out.elapsed)
            ));
        }
        MeasureMode::Bench { .. } => {
            ctx.harness.run_external_prepared(
                &invocation.config,
                &run,
                req.project_root.as_path(),
                &[],
                prepare,
            )?;
        }
        _ => {
            return Err(DevError::Config(
                "external tool does not support profiling".into(),
            ));
        }
    }
    invocation.cleanup(gated);
    Ok(())
}

/// Self-iterating cargo examples keep one aggregate row and their existing
/// stdout/stderr presentation, while sharing the runner's mode policy.
pub(crate) fn execute_example(
    req: &MeasureRequest<'_>,
    ctx: &MeasurementSession,
    binary: &Path,
    args: &[&str],
    config: &BenchConfig,
    observations: impl Fn() -> Vec<KvPair>,
) -> Result<(), DevError> {
    let program = binary.display().to_string();
    match req.mode {
        MeasureMode::Run => {
            output::run_msg(&format!("{program} {}", args.join(" ")));
            let out = output::run_passthrough_timed(&program, args, Some(ctx.harness.lock()))?;
            if let Some(code) = out.failed_code(&[]) {
                return Err(DevError::ExitCode(code));
            }
            output::run_msg(&format!(
                "elapsed={}ms",
                crate::passthrough_cmd::duration_ms(out.elapsed)
            ));
        }
        MeasureMode::Bench { .. } => {
            ctx.harness.run_internal(config, |_| {
                let captured = output::run_captured(&program, args, req.project_root.as_path())?;
                output::raw_stdout(&String::from_utf8_lossy(&captured.stdout));
                output::raw_stderr(&String::from_utf8_lossy(&captured.stderr));
                captured.check_success(&program)?;
                Ok(harness::BenchResult {
                    elapsed_ms: harness::elapsed_to_ms(&captured.elapsed),
                    elapsed_us: None,
                    kv: observations(),
                    iterations: vec![],
                    distribution: None,
                    hotpath: None,
                })
            })?;
        }
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            if let Some(profiling) = req.mode.profiled() {
                profiling.print_note();
            }
            ctx.harness.run_hotpath(config, binary, |i, fifo| {
                ctx.harness.capture_hotpath(
                    binary,
                    args,
                    req.project_root.as_path(),
                    &[],
                    &[],
                    fifo,
                    i,
                    None,
                )
            })?;
        }
    }
    Ok(())
}
