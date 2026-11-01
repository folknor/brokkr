//! Shared execution policy. Dry-run resolves a plan without materializing tools
//! or run artifacts (input verification may refresh its digest cache);
//! run never records, and failed samples retain their artifacts.

use crate::context::MeasurementSession;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{Executable, Invocation, MeasureMode, MeasureRequest};
use crate::output;
use crate::output::child::Exits;
use crate::project::{self, Project};
use std::path::{Path, PathBuf};

pub(crate) struct RowSubject {
    pub command: String,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub cli_args: Option<String>,
    pub metadata: Vec<KvPair>,
    /// See [`BenchConfig::arm_qualifier`].
    pub arm_qualifier: Option<String>,
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
            arm_qualifier: self.arm_qualifier.clone(),
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
        Ok(Some(crate::build::BuildConfig::release_with_features(
            self.package(),
            &req.build_features(),
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
    /// The exits a run of this plan stores as completed. Takes the plan so a
    /// project can answer for the executable it was built against and the
    /// input it resolved.
    fn exits(&self, _plan: &CommandPlan<Self::State, Self::Argv>) -> Exits {
        Exits::SUCCESS
    }
    /// Restate a failed run's error in the plan's terms, such as the answer
    /// its input declared. The error as it is by default.
    fn explain(&self, _plan: &CommandPlan<Self::State, Self::Argv>, error: DevError) -> DevError {
        error
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
        execute_invocation(req, &ctx, command, &plan, &invocation, &prepare)
            .map_err(|error| command.explain(&plan, error))?;
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

/// Run a binary plan's materialised invocation in the request's mode, as one
/// [`Cell`] timed by the external wall. Only the invocation is spawned, never
/// the bare binary: its program and argv are what the row records and what
/// runs.
fn execute_invocation<C: PlannedCommand, P: Fn() -> Result<(), DevError>>(
    req: &MeasureRequest<'_>,
    ctx: &MeasurementSession,
    command: &C,
    plan: &CommandPlan<C::State, C::Argv>,
    invocation: &Invocation<'_>,
    prepare: &P,
) -> Result<(), DevError> {
    let cell = Cell {
        config: plan.subject.config(req.runs()),
        invocation,
        exits: command.exits(plan),
        wall: crate::harness::WallSource::External,
    };
    execute_cell(
        &req.mode,
        &ctx.harness,
        req.project_root.as_path(),
        &cell,
        &CellHooks {
            prepare,
            announce: &|| command.announce(req, plan),
            observe: &|result, stderr| command.observe(result, stderr),
        },
    )
}

/// One measured invocation and how its row is formed: what the row says,
/// which exits complete it, and which wall it records and ranks on.
pub(crate) struct Cell<'i> {
    /// The row's subject. Its `cli_args` is replaced by the invocation's own
    /// rendering, so the recorded command is the one that ran.
    pub config: BenchConfig,
    pub invocation: &'i Invocation<'i>,
    pub exits: Exits,
    /// [`WallSource::External`](crate::harness::WallSource::External) for a
    /// plan; a child that times its own measured region reports it, and
    /// keeps reporting it under `--hotpath` and `--alloc`.
    pub wall: crate::harness::WallSource,
}

/// What a cell's caller runs around its launches: `prepare` before every
/// launch in every mode, `announce` once before a benchmark, `observe` on
/// each successful profiled iteration's result.
pub(crate) struct CellHooks<'h> {
    pub prepare: &'h dyn Fn() -> Result<(), DevError>,
    pub announce: &'h dyn Fn() -> Result<(), DevError>,
    pub observe: &'h dyn Fn(&mut harness::BenchResult, &[u8]),
}

impl CellHooks<'static> {
    /// No preparation, announcement or observation.
    pub(crate) const NONE: Self = Self {
        prepare: &|| Ok(()),
        announce: &|| Ok(()),
        observe: &|_, _| {},
    };
}

/// The one place a measured invocation's mode is decided: run once (output
/// passed through for an externally timed cell; one unstored measured launch
/// for a self-timed one), benchmark, or profile under `--hotpath` and
/// `--alloc`. Every measured command reaches its child through here, the
/// planned runner's plans and pbfhogg's read, write and merge cells alike, so
/// no command can accept a profiled mode and run it as a plain one.
pub(crate) fn execute_cell(
    mode: &MeasureMode,
    harness: &harness::BenchHarness,
    cwd: &Path,
    cell: &Cell<'_>,
    hooks: &CellHooks<'_>,
) -> Result<(), DevError> {
    let invocation = cell.invocation;
    invocation.require_no_env("the measure runner")?;
    let binary = invocation.program();
    let program = invocation.program_string();
    let mut config = cell.config.clone();
    config.cli_args = Some(invocation.cli_args());
    let prepare = hooks.prepare;
    match mode {
        // A child that times itself reports that wall in run mode too: one
        // measured launch, its result line printed and nothing stored (a run
        // mode harness stores no row), the FIFO and `--stop` attached.
        MeasureMode::Run if !matches!(cell.wall, crate::harness::WallSource::External) => {
            harness.run_invocation_timed(&config, invocation, cwd, cell.exits, cell.wall, prepare)?;
        }
        MeasureMode::Run => {
            prepare()?;
            let launched = invocation.launch(cwd)?;
            let args = launched.arg_refs();
            output::run_msg(&format!("{program} {}", args.join(" ")));
            let out = output::run_passthrough_timed(&program, &args, Some(harness.lock()));
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
            if let Some(code) = out.failed_code(cell.exits) {
                // An answer command expected to answer no fails on exit 0,
                // which must not reach the process exit as success.
                if code == 0 {
                    return Err(DevError::Subprocess {
                        program: program.clone(),
                        ending: crate::error::ProcessEnding::Code(0),
                        stderr: String::new(),
                    });
                }
                return Err(DevError::ExitCode(code));
            }
            output::run_msg(&format!(
                "elapsed={}ms",
                crate::passthrough_cmd::duration_ms(out.elapsed)
            ));
        }
        MeasureMode::Bench { .. } => {
            (hooks.announce)()?;
            harness.run_invocation_timed(&config, invocation, cwd, cell.exits, cell.wall, prepare)?;
        }
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            if let Some(profiling) = mode.profiled() {
                profiling.print_note();
            }
            harness.run_hotpath(&config, binary, cell.wall, |i, fifo| {
                prepare()?;
                // The report is this launch's disposable output: reserved
                // with it and removed with its lease, here or, if the child
                // outlives this run, by a later scan. A launch with no
                // reservation (an unreserved invocation) is removed here.
                let report = harness.new_hotpath_report_path()?;
                let launched = invocation.launch_with(cwd, std::slice::from_ref(&report))?;
                let iteration = harness.capture_hotpath_into(
                    binary,
                    &launched.arg_refs(),
                    cwd,
                    cell.exits,
                    fifo,
                    i,
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
                    (hooks.observe)(result, &iteration.stderr);
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
            if let Some(code) = out.failed_code(Exits::SUCCESS) {
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
                Exits::SUCCESS,
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
            if let Some(code) = out.failed_code(Exits::SUCCESS) {
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
            let wall = crate::harness::WallSource::External;
            ctx.harness.run_hotpath(config, binary, wall, |i, fifo| {
                ctx.harness
                    .capture_hotpath(binary, args, req.project_root.as_path(), fifo, i)
            })?;
        }
    }
    Ok(())
}
