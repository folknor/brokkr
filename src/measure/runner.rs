//! Shared execution policy. Dry-run resolves a plan without materializing tools
//! or run artifacts (input verification may refresh its digest cache);
//! run never records, and failed samples retain their artifacts.

use std::path::{Path, PathBuf};

use crate::context::MeasurementSession;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::output;
use crate::project::{self, Project};

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
            cargo_profile: crate::build::CargoProfile::Release,
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

pub(crate) struct CommandPlan<S> {
    pub args: Vec<String>,
    pub scratch_dirs: Vec<PathBuf>,
    pub subject: RowSubject,
    pub state: S,
}

/// Project code supplies resolution, build shape and lifecycle operations;
/// mode, recording and the success boundary belong to this runner.
pub(crate) trait PlannedCommand {
    type State;
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
            req.hotpath_features()
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
        binary: &Path,
    ) -> Result<CommandPlan<Self::State>, DevError>;
    fn prepare(
        &self,
        _req: &MeasureRequest<'_>,
        _ctx: &MeasurementSession,
        _plan: &CommandPlan<Self::State>,
    ) -> Result<(), DevError> {
        Ok(())
    }
    fn finish(
        &self,
        _req: &MeasureRequest<'_>,
        _ctx: &MeasurementSession,
        _plan: &CommandPlan<Self::State>,
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
        _plan: &CommandPlan<Self::State>,
    ) -> Result<(), DevError> {
        Err(DevError::Config("plan has no lifecycle executor".into()))
    }
}

fn profiling(req: &MeasureRequest<'_>) -> bool {
    matches!(
        req.mode,
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
    )
}

pub(crate) fn run(req: &MeasureRequest<'_>, command: &impl PlannedCommand) -> Result<(), DevError> {
    project::require(req.project, command.project(), command.id())?;
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
        let plan = command.plan(req, &paths, Path::new(command.id()))?;
        output::dry_run_msg(&format!("{} args: {}", command.id(), plan.args.join(" ")));
        output::dry_run_msg("ok");
        return Ok(());
    }
    let label = req.mode_label().unwrap_or("run");
    let lock_command = format!("{label} {}", command.id());
    let ctx = match command.build_config(req)? {
        Some(config) => {
            MeasurementSession::with_build_config_for_request(req, &config, &lock_command)?
        }
        None => MeasurementSession::new_for_request(req, &lock_command)?,
    };
    let binary = if matches!(command.kind(), PlanKind::Binary) {
        ctx.binary()?
    } else {
        Path::new(command.id())
    };
    let plan = command.plan(req, &ctx.paths, binary)?;
    let prepare = || -> Result<(), DevError> {
        for dir in &plan.scratch_dirs {
            if dir.exists() {
                std::fs::remove_dir_all(dir)?;
            }
            std::fs::create_dir_all(dir)?;
        }
        command.prepare(req, &ctx, &plan)
    };
    if matches!(command.kind(), PlanKind::External | PlanKind::Lifecycle) {
        prepare()?;
        command.execute(req, &ctx, &plan)?;
    } else {
        let program = binary.display().to_string();
        let args: Vec<&str> = plan.args.iter().map(String::as_str).collect();
        let mut config = plan.subject.config(req.runs());
        config.cli_args = Some(harness::format_cli_args(&program, &args));
        match req.mode {
            MeasureMode::Run => {
                prepare()?;
                output::run_msg(&format!("{program} {}", args.join(" ")));
                let out = output::run_passthrough_timed(&program, &args, Some(ctx.harness.lock()))?;
                if out.code != 0 && !command.ok_codes().contains(&out.code) {
                    return Err(DevError::ExitCode(out.code));
                }
                output::run_msg(&format!("elapsed={}ms", crate::duration_ms(out.elapsed)));
            }
            MeasureMode::Bench { .. } => {
                ctx.harness.run_external_prepared(
                    &config,
                    binary,
                    &args,
                    req.project_root,
                    command.ok_codes(),
                    prepare,
                )?;
            }
            MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
                harness::hotpath_alloc_note(req.is_alloc());
                ctx.harness.run_hotpath(&config, binary, |_| {
                    prepare()?;
                    let (mut result, stderr, sidecar) = harness::run_hotpath_capture(
                        &program,
                        &args,
                        &ctx.paths.scratch_dir,
                        req.project_root,
                        &command.env(),
                        command.ok_codes(),
                        req.stop_marker,
                        Some(ctx.harness.lock()),
                    )?;
                    command.observe(&mut result, &stderr);
                    Ok((result, sidecar))
                })?;
            }
        }
    }
    // Promotion is part of success. If it fails, preserve the current output.
    command.finish(req, &ctx, &plan)?;
    for dir in &plan.scratch_dirs {
        std::fs::remove_dir_all(dir).ok();
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
    pub(crate) fn prepare_sample(&self) -> Result<(), DevError> {
        for file in &self.output_files {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        for dir in &self.scratch_dirs {
            if dir.exists() {
                std::fs::remove_dir_all(dir)?;
            }
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    pub(crate) fn cleanup(&self) {
        for file in &self.output_files {
            std::fs::remove_file(file).ok();
        }
        for dir in &self.scratch_dirs {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

pub(crate) fn execute_external(
    req: &MeasureRequest<'_>,
    ctx: &MeasurementSession,
    invocation: &ExternalInvocation,
) -> Result<(), DevError> {
    let args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
    let prepare = || invocation.prepare_sample();
    match req.mode {
        MeasureMode::Run => {
            prepare()?;
            let program = invocation.program.display().to_string();
            output::run_msg(&format!("{program} {}", args.join(" ")));
            let out = output::run_passthrough_timed(&program, &args, Some(ctx.harness.lock()))?;
            if out.code != 0 {
                return Err(DevError::ExitCode(out.code));
            }
            output::run_msg(&format!("elapsed={}ms", crate::duration_ms(out.elapsed)));
        }
        MeasureMode::Bench { .. } => {
            ctx.harness.run_external_prepared(
                &invocation.config,
                &invocation.program,
                &args,
                req.project_root,
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
    invocation.cleanup();
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
            if out.code != 0 {
                return Err(DevError::ExitCode(out.code));
            }
            output::run_msg(&format!("elapsed={}ms", crate::duration_ms(out.elapsed)));
        }
        MeasureMode::Bench { .. } => {
            ctx.harness.run_internal(config, |_| {
                let captured = output::run_captured(&program, args, req.project_root)?;
                print!("{}", String::from_utf8_lossy(&captured.stdout));
                eprint!("{}", String::from_utf8_lossy(&captured.stderr));
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
            harness::hotpath_alloc_note(req.is_alloc());
            ctx.harness.run_hotpath(config, binary, |_| {
                let (result, _, sidecar) = harness::run_hotpath_capture(
                    &program,
                    args,
                    &ctx.paths.scratch_dir,
                    req.project_root,
                    &[],
                    &[],
                    req.stop_marker,
                    Some(ctx.harness.lock()),
                )?;
                Ok((result, sidecar))
            })?;
        }
    }
    Ok(())
}
