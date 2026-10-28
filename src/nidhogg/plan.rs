//! Nidhogg binary and HTTP/server lifecycle plans.
use crate::context::MeasurementSession;
use crate::error::DevError;
use crate::measure::runner::{
    CommandPlan, PlanKind, PlannedCommand, RowSubject, not_a_binary_plan,
};
use crate::measure::{Executable, Invocation, MeasureMode, MeasureRequest};
use crate::nidhogg::commands::NidhoggCommand;
use crate::project::Project;

pub(crate) struct NidhoggPlan<'a>(pub &'a NidhoggCommand);

pub(crate) enum NidhoggState {
    Ingest,
    Api {
        port: u16,
        bbox: crate::datasets::Bbox,
        input_file: Option<String>,
        input_mb: Option<f64>,
    },
    Tiles {
        port: u16,
        data: String,
        tiles: String,
        file: String,
        mb: f64,
        hash: Option<String>,
    },
}

impl PlannedCommand for NidhoggPlan<'_> {
    type State = NidhoggState;
    type Argv = Vec<String>;
    fn project(&self) -> Project {
        Project::Nidhogg
    }
    fn id(&self) -> &'static str {
        self.0.id()
    }
    fn package(&self) -> Option<&'static str> {
        self.0.package()
    }

    fn kind(&self) -> PlanKind {
        if matches!(self.0, NidhoggCommand::Ingest) {
            PlanKind::Binary
        } else {
            PlanKind::Lifecycle
        }
    }
    fn supports_profiling(&self) -> bool {
        matches!(self.0, NidhoggCommand::Ingest)
    }
    fn build_config(
        &self,
        req: &MeasureRequest<'_>,
    ) -> Result<Option<crate::build::BuildConfig>, DevError> {
        if matches!(self.0, NidhoggCommand::Api { .. }) {
            return Ok(None);
        }
        let mut features = if matches!(
            req.mode,
            MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
        ) {
            req.build_features()
        } else {
            req.feat_refs()
        };
        if matches!(self.0, NidhoggCommand::Tiles { uring: true, .. }) {
            crate::preflight::run_preflight(&crate::preflight::uring_checks())?;
            features.push("linux-io-uring");
        }
        Ok(Some(crate::build::BuildConfig::release_with_features(
            self.package(),
            &features,
        )))
    }
    fn materialize<'p>(
        &self,
        plan: &'p CommandPlan<NidhoggState>,
        executable: Executable<'p>,
    ) -> Result<Invocation<'p>, DevError> {
        match self.kind() {
            PlanKind::Binary => Invocation::built(executable, plan.argv.clone()),
            PlanKind::External | PlanKind::Lifecycle => Err(not_a_binary_plan(self.id())),
        }
    }
    fn execute(
        &self,
        req: &MeasureRequest<'_>,
        ctx: &MeasurementSession,
        plan: &CommandPlan<NidhoggState>,
    ) -> Result<(), DevError> {
        match &plan.state {
            NidhoggState::Api {
                port,
                bbox,
                input_file,
                input_mb,
            } => {
                let query = match self.0 {
                    NidhoggCommand::Api { query } => query.as_deref(),
                    _ => None,
                };
                super::bench_api::run(
                    &ctx.harness,
                    *port,
                    req.runs(),
                    query,
                    input_file.as_deref(),
                    *input_mb,
                    bbox,
                )
            }
            NidhoggState::Tiles {
                port,
                data,
                tiles,
                file,
                mb,
                hash,
            } => super::bench_tiles::run(
                &ctx.harness,
                ctx.binary()?,
                data,
                tiles,
                *port,
                file,
                hash.as_deref(),
                *mb,
                req.runs(),
                req.project_root.as_path(),
            ),
            NidhoggState::Ingest => unreachable!(),
        }
    }

    fn plan(
        &self,
        req: &MeasureRequest<'_>,
        paths: &crate::config::ResolvedPaths,
        _target: crate::measure::runner::PlanTarget<'_>,
    ) -> Result<CommandPlan<NidhoggState>, DevError> {
        if !matches!(self.0, NidhoggCommand::Ingest) {
            return server_plan(self.0, req, paths);
        }
        let (pbf, file_mb) = crate::datasets::resolve::resolve_pbf_with_size(
            req.dataset,
            req.variant,
            paths,
            req.project_root.as_path(),
        )?;
        let pbf_str = crate::utf8_path::path_str(&pbf)?;
        // Preserve each mode's historic scratch name: it is part of the
        // subprocess cli_args stored on existing measurement rows.
        let layout = crate::clean::layout::NidhoggLayout::new(paths);
        let output_dir = match req.mode {
            MeasureMode::Run => layout.run_ingest_output(),
            MeasureMode::Bench { .. } => layout.bench_ingest_output(),
            MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. } => {
                layout.hotpath_ingest_output()
            }
        };
        let output_dir_str = crate::utf8_path::path_str(&output_dir)?;
        let args = self.0.build_args(pbf_str, output_dir_str)?;
        Ok(CommandPlan {
            state: NidhoggState::Ingest,
            argv: args,
            scratch_dirs: vec![output_dir],
            subject: RowSubject {
                command: self.0.id().into(),
                input_file: Some(crate::utf8_path::file_name(&pbf)),
                input_mb: Some(file_mb),
                cli_args: None,
                metadata: vec![],
                arm_qualifier: None,
            },
        })
    }
}

fn api_plan(
    query: Option<&String>,
    req: &MeasureRequest<'_>,
    paths: &crate::config::ResolvedPaths,
    port: u16,
) -> Result<(Vec<String>, NidhoggState), DevError> {
    let bbox = crate::datasets::resolve::resolve_bbox(None, req.dataset, paths)?;
    let queries = super::client::build_api_queries(&bbox)?;
    let names: Vec<String> = queries
        .into_iter()
        .filter(|(n, _)| query.is_none_or(|q| q == n))
        .map(|(n, _)| n)
        .collect();
    let has_configured_pbf = paths
        .datasets
        .get(req.dataset)
        .is_some_and(|dataset| !dataset.pbf.is_empty());
    let pbf = has_configured_pbf
        .then(|| {
            crate::datasets::resolve::resolve_pbf_path(
                req.dataset,
                req.variant,
                paths,
                req.project_root.as_path(),
            )
        })
        .transpose()?;
    let input_file = pbf
        .as_ref()
        .and_then(|p| crate::utf8_path::file_name_str(p))
        .map(str::to_owned);
    let input_mb = pbf
        .as_ref()
        .map(|p| crate::datasets::resolve::file_size_mb(p))
        .transpose()?;
    Ok((
        vec![super::client::query_url(port), names.join(",")],
        NidhoggState::Api {
            port,
            bbox,
            input_file,
            input_mb,
        },
    ))
}

fn tiles_plan(
    tiles_variant: Option<&String>,
    uring: bool,
    req: &MeasureRequest<'_>,
    paths: &crate::config::ResolvedPaths,
    port: u16,
) -> Result<(Vec<String>, NidhoggState), DevError> {
    if uring {
        crate::preflight::run_preflight(&crate::preflight::uring_checks())?;
    }
    let data_path = crate::nidhogg::data_dir::resolve_nidhogg_data_dir(req.dataset, paths)?;
    let data = crate::utf8_path::path_str(&data_path)?.to_owned();
    let (tiles, mb) = match tiles_variant {
        Some(v) => crate::datasets::resolve::resolve_pmtiles_with_size(
            req.dataset,
            v,
            paths,
            req.project_root.as_path(),
        )?,
        None => crate::datasets::resolve::resolve_default_pmtiles_with_size(
            req.dataset,
            paths,
            req.project_root.as_path(),
        )?,
    };
    let coords = crate::pmtiles::sample_tile_coords(&tiles)?;
    if coords.is_empty() {
        return Err(DevError::Config(
            "PMTiles archive contains no tile entries".into(),
        ));
    }
    let hash = paths
        .datasets
        .get(req.dataset)
        .and_then(|d| {
            if let Some(v) = tiles_variant {
                d.pmtiles.get(v)
            } else if d.pmtiles.len() == 1 {
                d.pmtiles.values().next()
            } else {
                None
            }
        })
        .and_then(|e| e.xxhash.as_ref())
        .map(ToString::to_string);
    let file = crate::utf8_path::file_name(&tiles);
    let tiles = crate::utf8_path::path_str(&tiles)?.to_owned();
    Ok((
        vec![
            "serve".into(),
            data.clone(),
            tiles.clone(),
            format!("PORT={port}"),
        ],
        NidhoggState::Tiles {
            port,
            data,
            tiles,
            file,
            mb,
            hash,
        },
    ))
}

fn server_plan(
    command: &NidhoggCommand,
    req: &MeasureRequest<'_>,
    paths: &crate::config::ResolvedPaths,
) -> Result<CommandPlan<NidhoggState>, DevError> {
    let port = super::cmd::resolve_port(req.dev_config)?;
    let (args, state) = match command {
        NidhoggCommand::Api { query } => api_plan(query.as_ref(), req, paths, port)?,
        NidhoggCommand::Tiles {
            tiles_variant,
            uring,
        } => tiles_plan(tiles_variant.as_ref(), *uring, req, paths, port)?,
        _ => unreachable!(),
    };
    Ok(CommandPlan {
        argv: args,
        scratch_dirs: vec![],
        state,
        subject: RowSubject {
            command: command.id().into(),
            input_file: None,
            input_mb: None,
            cli_args: None,
            metadata: vec![],
            arm_qualifier: None,
        },
    })
}
