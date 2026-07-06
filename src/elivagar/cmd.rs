use std::path::Path;

use crate::config;
use crate::context::{HarnessContext, bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::measure::MeasureRequest;
use crate::project::{self, Project};
use crate::resolve::{
    resolve_default_pmtiles_path, resolve_pbf_with_size, resolve_pmtiles_by_commit,
    resolve_pmtiles_path,
};

pub(crate) fn bench_planetiler(req: &MeasureRequest) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench planetiler",
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_planetiler::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        req.runs(),
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        req.project_root,
    )
}

pub(crate) fn bench_tilemaker(req: &MeasureRequest) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench tilemaker",
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_tilemaker::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        req.runs(),
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        req.project_root,
    )
}

pub(crate) fn bench_all(req: &MeasureRequest) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench all",
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_all::run(
        &ctx.harness,
        &ctx.paths,
        req.effective_build_root(),
        &pbf_path,
        file_mb,
        req.runs(),
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
    )
}

pub(crate) fn compare_tiles(
    project: Project,
    project_root: &Path,
    file_a: &str,
    file_b: &str,
    sample: Option<usize>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "compare-tiles")?;
    let pi = bootstrap(None)?;
    super::compare_tiles::run(&pi.target_dir, project_root, file_a, file_b, sample)
}

pub(crate) fn download_ocean(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-ocean")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    super::download_ocean::run(&paths.data_dir)
}

pub(crate) fn download_natural_earth(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-natural-earth")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    super::download_natural_earth::run(&paths.data_dir)
}

pub(crate) fn verify(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    tiles_variant: Option<&str>,
    features: &[String],
    geometry_stats: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "verify")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = match tiles_variant {
        Some(v) => resolve_pmtiles_path(dataset, v, &paths, project_root)?,
        None => resolve_default_pmtiles_path(dataset, &paths, project_root)?,
    };
    let effective = build_root.unwrap_or(project_root);
    super::verify::run(&pmtiles_path, effective, features, geometry_stats)
}

pub(crate) fn inspect(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    commit: Option<&str>,
    file: Option<&str>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "pmtiles-inspect")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = resolve_pmtiles_by_commit(dataset, commit, file, &paths, project_root)?;
    super::inspect::run(&pmtiles_path, project_root)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn diag(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    commit: Option<&str>,
    file: Option<&str>,
    z: u8,
    x: u32,
    y: u32,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "diag")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = resolve_pmtiles_by_commit(dataset, commit, file, &paths, project_root)?;
    super::diag::run(&pmtiles_path, project_root, z, x, y)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn svg(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    dataset: &str,
    commit: Option<&str>,
    file: Option<&str>,
    z: u8,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    layers: Option<&str>,
    output_path: Option<&Path>,
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "svg")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = resolve_pmtiles_by_commit(dataset, commit, file, &paths, project_root)?;
    super::svg::run(
        &pmtiles_path,
        project_root,
        z,
        x,
        y,
        width,
        height,
        layers,
        output_path,
    )
}
