//! Build, tool and example plans for elivagar.

use super::commands::{BuiltCommand, ElivagarCommand, ExampleCommand, ExternalTool};
use crate::context::MeasurementSession;
use crate::error::DevError;
use crate::measure::runner::{self, CommandPlan, PlanKind, PlannedCommand, RowSubject};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::project::Project;
use std::path::Path;

pub(crate) struct ElivagarPlan<'a, 'b>(pub &'a ElivagarCommand<'b>);

impl PlannedCommand for ElivagarPlan<'_, '_> {
    type State = (std::path::PathBuf, f64);
    fn project(&self) -> Project {
        Project::Elivagar
    }
    fn id(&self) -> &'static str {
        self.0.id()
    }
    fn package(&self) -> Option<&'static str> {
        None
    }
    fn kind(&self) -> PlanKind {
        match self.0 {
            ElivagarCommand::External(_) => PlanKind::External,
            ElivagarCommand::Built(BuiltCommand::Example(_)) => PlanKind::Lifecycle,
            _ => PlanKind::Binary,
        }
    }
    fn supports_profiling(&self) -> bool {
        !matches!(self.0, ElivagarCommand::External(_))
    }
    fn build_config(
        &self,
        req: &MeasureRequest<'_>,
    ) -> Result<Option<crate::build::BuildConfig>, DevError> {
        if matches!(self.0, ElivagarCommand::External(_)) {
            return Ok(None);
        }
        let features = if matches!(
            req.mode,
            MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
        ) {
            req.hotpath_features()
        } else {
            req.feat_refs()
        };
        let mut config = crate::build::BuildConfig::release_with_features(None, &features);
        if let ElivagarCommand::Built(BuiltCommand::Example(example)) = self.0 {
            config.example = Some(example.cargo_example().into());
        }
        Ok(Some(config))
    }
    fn plan(
        &self,
        req: &MeasureRequest<'_>,
        paths: &crate::config::ResolvedPaths,
        _binary: &Path,
    ) -> Result<CommandPlan<Self::State>, DevError> {
        let mut state = (std::path::PathBuf::new(), 0.0);
        let mut input_file = None;
        let mut input_mb = None;
        let args = if self.0.needs_pbf() {
            let pbf = crate::resolve::resolve_pbf_entry(
                req.dataset,
                req.variant,
                paths,
                req.project_root,
            )?;
            state = (pbf.path.clone(), crate::resolve::file_size_mb(&pbf.path)?);
            input_file = pbf
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            input_mb = Some(state.1);
            match self.0 {
                ElivagarCommand::Built(command) => command.build_args(
                    &pbf.path.display().to_string(),
                    &paths.scratch_dir,
                    &paths.data_dir,
                    Some(&super::PipelineOpts::from_pbf_entry(pbf.entry)),
                )?,
                ElivagarCommand::External(tool) => external(tool, req, paths, &state, true)?.args,
            }
        } else {
            match self.0 {
                ElivagarCommand::Built(command) => {
                    command.build_args("", &paths.scratch_dir, &paths.data_dir, None)?
                }
                _ => unreachable!(),
            }
        };
        Ok(CommandPlan {
            args,
            scratch_dirs: vec![],
            state,
            subject: RowSubject {
                command: self.id().into(),
                input_file,
                input_mb,
                cli_args: None,
                metadata: vec![],
            },
        })
    }
    fn prepare(
        &self,
        _req: &MeasureRequest<'_>,
        ctx: &MeasurementSession,
        _plan: &CommandPlan<Self::State>,
    ) -> Result<(), DevError> {
        if let ElivagarCommand::Built(BuiltCommand::MainBinary { .. }) = self.0 {
            std::fs::create_dir_all(&ctx.paths.scratch_dir)?;
            std::fs::create_dir_all(ctx.paths.data_dir.join("tilegen_tmp"))?;
            if let ElivagarCommand::Built(command) = self.0 {
                for file in command.output_files(&ctx.paths.scratch_dir) {
                    std::fs::remove_file(file).ok();
                }
            }
        }
        Ok(())
    }
    fn env(&self) -> Vec<(&str, &str)> {
        vec![("ELIVAGAR_NODE_STATS", "1")]
    }
    fn observe(&self, result: &mut crate::harness::BenchResult, stderr: &[u8]) {
        result.kv.push(crate::db::KvPair::text(
            "meta.locations_on_ways_detected",
            super::detect_locations_on_ways_stderr(stderr).to_string(),
        ));
    }
    fn finish(
        &self,
        req: &MeasureRequest<'_>,
        ctx: &MeasurementSession,
        _plan: &CommandPlan<Self::State>,
    ) -> Result<(), DevError> {
        if let ElivagarCommand::Built(command @ BuiltCommand::MainBinary { .. }) = self.0 {
            if matches!(req.mode, MeasureMode::Run | MeasureMode::Bench { .. }) {
                super::artifacts::rename_elivagar_output(
                    command,
                    &ctx.paths.scratch_dir,
                    &ctx.paths.output_dir,
                    &ctx.paths.data_dir,
                    req.dataset,
                    req.variant,
                    req.effective_build_root(),
                )?;
            } else {
                for file in command.output_files(&ctx.paths.scratch_dir) {
                    std::fs::remove_file(file).ok();
                }
                let suffix = if req.is_alloc() { "alloc-" } else { "" };
                std::fs::remove_file(
                    ctx.paths
                        .scratch_dir
                        .join(format!("hotpath-{suffix}output.pmtiles")),
                )
                .ok();
            }
        }
        Ok(())
    }
    fn execute(
        &self,
        req: &MeasureRequest<'_>,
        ctx: &MeasurementSession,
        plan: &CommandPlan<Self::State>,
    ) -> Result<(), DevError> {
        match self.0 {
            ElivagarCommand::External(tool) => {
                let invocation = external(tool, req, &ctx.paths, &plan.state, false)?;
                runner::execute_external(req, ctx, &invocation)
            }
            ElivagarCommand::Built(BuiltCommand::Example(example)) => {
                run_example(req, ctx, example, plan)
            }
            _ => unreachable!(),
        }
    }
}

fn external(
    tool: &ExternalTool,
    req: &MeasureRequest<'_>,
    paths: &crate::config::ResolvedPaths,
    state: &(std::path::PathBuf, f64),
    dry_run: bool,
) -> Result<runner::ExternalInvocation, DevError> {
    match tool {
        ExternalTool::Planetiler => super::bench_planetiler::prepare(
            &state.0,
            state.1,
            req.runs(),
            &paths.data_dir,
            &paths.scratch_dir,
            req.project_root,
            dry_run,
        ),
        ExternalTool::Tilemaker => super::bench_tilemaker::prepare(
            &state.0,
            state.1,
            req.runs(),
            &paths.data_dir,
            &paths.scratch_dir,
            dry_run,
        ),
    }
}

fn run_example(
    req: &MeasureRequest<'_>,
    ctx: &MeasurementSession,
    example: &ExampleCommand,
    plan: &CommandPlan<(std::path::PathBuf, f64)>,
) -> Result<(), DevError> {
    let binary = ctx.binary()?;
    let mut args = plan.args.clone();
    let bench = matches!(req.mode, MeasureMode::Bench { .. });
    if bench {
        let runs = args
            .last_mut()
            .ok_or_else(|| DevError::Config("example has no --runs argument".into()))?;
        *runs = req.runs().to_string();
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // Examples historically have no cli_args; retain that row shape.
    let config = crate::harness::BenchConfig {
        command: example.id().into(),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs: 1,
        cli_args: None,
        metadata: vec![],
    };
    runner::execute_example(req, ctx, binary, &refs, &config, || {
        use crate::db::KvPair;
        let (key, value) = match example {
            ExampleCommand::PmtilesWriter { tiles } => ("tiles", *tiles),
            ExampleCommand::NodeStore { nodes } => ("nodes_millions", *nodes),
        };
        vec![
            KvPair::int(key, i64::try_from(value).unwrap_or(i64::MAX)),
            KvPair::int(
                "internal_runs",
                i64::try_from(req.runs()).unwrap_or(i64::MAX),
            ),
        ]
    })
}
