//! Nidhogg dispatch: standalone ingest uses the shared plan runner;
//! API and tiles retain their specialized server lifecycles.

use crate::error::DevError;
use crate::measure::runner::{self, CommandPlan, PlannedCommand, RowSubject};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::nidhogg::commands::NidhoggCommand;
use crate::project::{self, Project};

pub(crate) fn run_command(
    req: &MeasureRequest<'_>,
    command: &NidhoggCommand,
) -> Result<(), DevError> {
    project::require(req.project, Project::Nidhogg, command.id())?;
    match command {
        NidhoggCommand::Ingest => runner::run(req, &IngestPlan(command)),
        NidhoggCommand::Api { query } => {
            validate_server_mode(req, command)?;
            crate::nidhogg::cmd::bench_api(req, query.as_deref())
        }
        NidhoggCommand::Tiles {
            tiles_variant,
            uring,
        } => {
            validate_server_mode(req, command)?;
            crate::nidhogg::cmd::bench_tiles(req, tiles_variant.as_deref(), *uring)
        }
    }
}

fn validate_server_mode(
    req: &MeasureRequest<'_>,
    command: &NidhoggCommand,
) -> Result<(), DevError> {
    if req.dry_run {
        return Err(DevError::Config(
            "--dry-run is not yet supported for nidhogg server commands".into(),
        ));
    }
    if matches!(
        req.mode,
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
    ) {
        return Err(DevError::Config(format!(
            "command '{}' does not support hotpath/alloc profiling",
            command.id(),
        )));
    }
    Ok(())
}

/// Only ingest has a standalone argv contract. Server commands deliberately
/// cannot be passed to the runner without a lifecycle-aware plan.
struct IngestPlan<'a>(&'a NidhoggCommand);

impl PlannedCommand for IngestPlan<'_> {
    fn project(&self) -> Project {
        Project::Nidhogg
    }
    fn id(&self) -> &'static str {
        self.0.id()
    }
    fn package(&self) -> Option<&'static str> {
        self.0.package()
    }

    fn plan(
        &self,
        req: &MeasureRequest<'_>,
        paths: &crate::config::ResolvedPaths,
    ) -> Result<CommandPlan, DevError> {
        let (pbf, file_mb) = crate::resolve::resolve_pbf_with_size(
            req.dataset,
            req.variant,
            paths,
            req.project_root,
        )?;
        let pbf_str = pbf
            .to_str()
            .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
        // Preserve each mode's historic scratch name: it is part of the
        // subprocess cli_args stored on existing measurement rows.
        let output_name = match req.mode {
            MeasureMode::Run => "run-ingest-output",
            MeasureMode::Bench { .. } => self
                .0
                .scratch_output_dir()
                .ok_or_else(|| DevError::Config("ingest has no scratch output".into()))?,
            MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => "hotpath-ingest-output",
        };
        let output_dir = paths.scratch_dir.join(output_name);
        let args = self
            .0
            .build_args(pbf_str, &output_dir.display().to_string())?;
        Ok(CommandPlan {
            args,
            scratch_dirs: vec![output_dir],
            subject: RowSubject {
                command: self.0.id().into(),
                input_file: Some(
                    pbf.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_owned(),
                ),
                input_mb: Some(file_mb),
                cli_args: None,
                metadata: self.0.metadata(),
            },
        })
    }
}
