//! Build, tool and example plans for elivagar.

use super::commands::{BuiltCommand, ElivagarCommand, ExampleCommand, ExternalTool};
use crate::clean::layout::ElivagarLayout;
use crate::context::MeasurementSession;
use crate::error::DevError;
use crate::measure::runner::{self, CommandPlan, PlanKind, PlannedCommand, RowSubject};
use crate::measure::{Executable, Invocation, MeasureMode, MeasureRequest};
use crate::project::Project;
use std::path::Path;

pub(crate) struct ElivagarPlan<'a, 'b>(pub &'a ElivagarCommand<'b>);

impl PlannedCommand for ElivagarPlan<'_, '_> {
    type State = (std::path::PathBuf, f64);
    type Argv = Vec<String>;
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
        if matches!(
            self.0,
            ElivagarCommand::Built(BuiltCommand::MainBinary { .. })
        ) {
            super::resolve_tilegen(req.dev_config, super::DEFAULT_TILEGEN)?;
        }
        let features = if matches!(
            req.mode,
            MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
        ) {
            req.build_features()
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
        _target: runner::PlanTarget<'_>,
    ) -> Result<CommandPlan<Self::State>, DevError> {
        if matches!(
            self.0,
            ElivagarCommand::Built(BuiltCommand::MainBinary { .. })
        ) {
            super::resolve_tilegen(req.dev_config, super::DEFAULT_TILEGEN)?;
        }
        let layout = ElivagarLayout::new(paths);
        let mut state = (std::path::PathBuf::new(), 0.0);
        let mut input_file = None;
        let mut input_mb = None;
        let args = if self.0.needs_pbf() {
            let pbf = crate::datasets::resolve::resolve_pbf_entry(
                req.dataset,
                req.variant,
                paths,
                req.project_root.as_path(),
            )?;
            state = (
                pbf.path.clone(),
                crate::datasets::resolve::file_size_mb(&pbf.path)?,
            );
            input_file = pbf
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            input_mb = Some(state.1);
            match self.0 {
                ElivagarCommand::Built(command) => command.build_args(
                    &pbf.path.display().to_string(),
                    &layout,
                    Some(&super::PipelineOpts::from_pbf_entry(pbf.entry)),
                )?,
                ElivagarCommand::External(tool) => external(tool, req, &layout, &state, true)?.args,
            }
        } else {
            match self.0 {
                ElivagarCommand::Built(command) => command.build_args("", &layout, None)?,
                ElivagarCommand::External(_) => {
                    return Err(DevError::Internal(
                        "external elivagar command unexpectedly has no PBF input".into(),
                    ));
                }
            }
        };
        Ok(CommandPlan {
            argv: args,
            scratch_dirs: vec![],
            state,
            subject: RowSubject {
                command: self.id().into(),
                input_file,
                input_mb,
                cli_args: None,
                metadata: vec![],
                arm_qualifier: None,
            },
        })
    }
    fn materialize<'p>(
        &self,
        plan: &'p CommandPlan<Self::State>,
        executable: Executable<'p>,
    ) -> Result<Invocation<'p>, DevError> {
        match self.kind() {
            PlanKind::Binary => Invocation::built(executable, plan.argv.clone()),
            PlanKind::External | PlanKind::Lifecycle => Err(runner::not_a_binary_plan(self.id())),
        }
    }
    fn prepare(
        &self,
        _req: &MeasureRequest<'_>,
        ctx: &MeasurementSession,
        _plan: &CommandPlan<Self::State>,
    ) -> Result<(), DevError> {
        if let ElivagarCommand::Built(command @ BuiltCommand::MainBinary { .. }) = self.0 {
            let layout = ElivagarLayout::new(&ctx.paths);
            std::fs::create_dir_all(layout.scratch())?;
            std::fs::create_dir_all(layout.tilegen_tmp())?;
            for file in command.output_files(&layout) {
                remove_if_present(&file)?;
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
        // This is the success path: Run/Bench promote their archive, while
        // Hotpath/Alloc remove scratch outputs. Failed executions preserve
        // scratch output for diagnosis, as the shared runner documents.
        if let ElivagarCommand::Built(command @ BuiltCommand::MainBinary { .. }) = self.0 {
            let layout = ElivagarLayout::new(&ctx.paths);
            if matches!(req.mode, MeasureMode::Run | MeasureMode::Bench { .. }) {
                super::artifacts::rename_elivagar_output(
                    command,
                    &layout,
                    req.dataset,
                    req.variant,
                    req.build_root.as_path(),
                )?;
            } else {
                for file in command.output_files(&layout) {
                    remove_if_present(&file)?;
                }
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
                let layout = ElivagarLayout::new(&ctx.paths);
                let invocation = external(tool, req, &layout, &plan.state, false)?;
                runner::execute_external(req, ctx, &invocation)
            }
            ElivagarCommand::Built(BuiltCommand::Example(example)) => {
                run_example(req, ctx, example, plan)
            }
            ElivagarCommand::Built(BuiltCommand::MainBinary { .. }) => Err(DevError::Internal(
                "tilegen execution must use the binary plan".into(),
            )),
        }
    }
}

fn remove_if_present(path: &Path) -> Result<(), DevError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn external(
    tool: &ExternalTool,
    req: &MeasureRequest<'_>,
    layout: &ElivagarLayout<'_>,
    state: &(std::path::PathBuf, f64),
    dry_run: bool,
) -> Result<runner::ExternalInvocation, DevError> {
    match tool {
        ExternalTool::Planetiler => super::bench_planetiler::prepare(
            &state.0,
            state.1,
            req.runs(),
            layout,
            req.project_root.as_path(),
            dry_run,
        ),
        ExternalTool::Tilemaker => {
            super::bench_tilemaker::prepare(&state.0, state.1, req.runs(), layout, dry_run)
        }
    }
}

fn run_example(
    req: &MeasureRequest<'_>,
    ctx: &MeasurementSession,
    example: &ExampleCommand,
    plan: &CommandPlan<(std::path::PathBuf, f64)>,
) -> Result<(), DevError> {
    let binary = ctx.binary()?;
    let mut args = plan.argv.clone();
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
        external_build: None,
        runs: 1,
        cli_args: None,
        metadata: vec![],
        arm_qualifier: None,
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
