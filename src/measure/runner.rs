//! Execution policy for standalone measured binaries. Planning is read-only;
//! scratch is materialized only after dry-run has returned.

use std::path::PathBuf;

use crate::context::BenchContext;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchResult};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::output;
use crate::project::{self, Project};

/// Identity and observations of one measured subject, separate from the
/// request's loop count and the session's build/invocation provenance.
pub(crate) struct RowSubject {
    pub command: String,
    pub input_file: Option<String>,
    pub input_mb: Option<f64>,
    pub cli_args: Option<String>,
    pub metadata: Vec<KvPair>,
}

impl RowSubject {
    fn config(self, req: &MeasureRequest<'_>) -> BenchConfig {
        BenchConfig {
            command: self.command,
            mode: None,
            input_file: self.input_file,
            input_mb: self.input_mb,
            cargo_features: None,
            cargo_profile: crate::build::CargoProfile::Release,
            runs: req.runs(),
            cli_args: self.cli_args,
            brokkr_args: None,
            metadata: self.metadata,
        }
    }
}

/// A resolved standalone invocation. Directories belong to this invocation:
/// reset before each sample, remove after success, preserve after failure.
pub(crate) struct CommandPlan {
    pub args: Vec<String>,
    pub scratch_dirs: Vec<PathBuf>,
    pub subject: RowSubject,
}

/// The project supplies facts and pure input/argv planning. The runner owns
/// mode selection, dry-run, build, sampling and scratch lifecycle.
pub(crate) trait PlannedCommand {
    fn project(&self) -> Project;
    fn id(&self) -> &'static str;
    fn package(&self) -> Option<&'static str>;
    fn plan(
        &self,
        req: &MeasureRequest<'_>,
        paths: &crate::config::ResolvedPaths,
    ) -> Result<CommandPlan, DevError>;
}

pub(crate) fn run(req: &MeasureRequest<'_>, command: &impl PlannedCommand) -> Result<(), DevError> {
    project::require(req.project, command.project(), command.id())?;
    if req.dry_run {
        let pi = crate::context::bootstrap(req.build_root)?;
        let paths =
            crate::context::bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)?;
        let plan = command.plan(req, &paths)?;
        output::dry_run_msg(&format!("{} args: {}", command.id(), plan.args.join(" ")));
        output::dry_run_msg("ok");
        return Ok(());
    }

    let profiling = matches!(
        req.mode,
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
    );
    let features = if profiling {
        req.hotpath_features()
    } else {
        req.feat_refs()
    };
    if profiling {
        harness::hotpath_alloc_note(req.is_alloc());
    }
    let label = if profiling {
        "hotpath"
    } else if matches!(req.mode, MeasureMode::Run) {
        "run"
    } else {
        "bench"
    };
    let ctx = BenchContext::new_for_request(
        req,
        command.package(),
        &features,
        true,
        &format!("{label} {}", command.id()),
    )?;
    let mut plan = command.plan(req, &ctx.paths)?;
    let binary = ctx.binary.display().to_string();
    let args: Vec<&str> = plan.args.iter().map(String::as_str).collect();
    plan.subject.cli_args = Some(harness::format_cli_args(&binary, &args));
    let config = plan.subject.config(req);
    let prepare = || -> Result<(), DevError> {
        for dir in &plan.scratch_dirs {
            if dir.exists() {
                std::fs::remove_dir_all(dir)?;
            }
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    };
    // No cleanup on any error, including nonzero exits and capture failures.
    // This preserves diagnostic artifacts consistently across all modes.
    match req.mode {
        MeasureMode::Run => {
            prepare()?;
            output::run_msg(&format!("{binary} {}", args.join(" ")));
            let out = output::run_passthrough_timed(&binary, &args, Some(ctx.harness.lock()))?;
            if out.code != 0 {
                return Err(DevError::ExitCode(out.code));
            }
            output::run_msg(&format!("elapsed={}ms", crate::duration_ms(out.elapsed)));
        }
        MeasureMode::Bench { .. } => {
            ctx.harness.run_internal(&config, |_| {
                prepare()?;
                let captured = output::run_captured(&binary, &args, req.project_root)?;
                captured.check_success(&binary)?;
                Ok(BenchResult {
                    elapsed_ms: harness::elapsed_to_ms(&captured.elapsed),
                    elapsed_us: None,
                    kv: vec![],
                    iterations: Vec::new(),
                    distribution: None,
                    hotpath: None,
                })
            })?;
        }
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
            ctx.harness.run_hotpath(&config, &ctx.binary, |_| {
                prepare()?;
                let (result, _, sidecar) = harness::run_hotpath_capture(
                    &binary,
                    &args,
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
    for dir in &plan.scratch_dirs {
        std::fs::remove_dir_all(dir).ok();
    }
    Ok(())
}
