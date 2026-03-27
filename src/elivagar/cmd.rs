use std::path::Path;

use crate::config;
use crate::context::{BenchContext, HarnessContext, bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::measure::MeasureRequest;
use crate::oom;
use crate::project::{self, Project};
use crate::resolve::{resolve_default_pmtiles_path, resolve_pbf_with_size, resolve_pmtiles_path};

pub(crate) fn bench_node_store(req: &MeasureRequest, nodes: usize) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench node-store",
        req.force,
    )?;
    super::bench_node_store::run(&ctx.harness, req.effective_build_root(), nodes, req.runs)
}

pub(crate) fn bench_pmtiles(req: &MeasureRequest, tiles: usize) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench pmtiles",
        req.force,
    )?;
    super::bench_pmtiles::run(&ctx.harness, req.effective_build_root(), tiles, req.runs)
}

pub(crate) fn bench_self(
    req: &MeasureRequest,
    skip_to: Option<&str>,
    compression_level: Option<u32>,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    let feat_refs = req.feat_refs();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        None,
        &feat_refs,
        true,
        "bench self",
        req.force,
    )?;
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_self::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        file_mb,
        req.runs,
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        req.project_root,
        skip_to,
        compression_level,
        opts,
    )
}

pub(crate) fn bench_planetiler(req: &MeasureRequest) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench planetiler",
        req.force,
    )?;
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_planetiler::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        req.runs,
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
    )?;
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_tilemaker::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        req.runs,
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
    )?;
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_all::run(
        &ctx.harness,
        &ctx.paths,
        req.effective_build_root(),
        &pbf_path,
        file_mb,
        req.runs,
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
) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "verify")?;
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pmtiles_path = match tiles_variant {
        Some(v) => resolve_pmtiles_path(dataset, v, &paths, project_root)?,
        None => resolve_default_pmtiles_path(dataset, &paths, project_root)?,
    };
    let effective = build_root.unwrap_or(project_root);
    super::verify::run(&pmtiles_path, effective, features)
}

pub(crate) fn hotpath(
    req: &MeasureRequest,
    variant: Option<&str>,
    tiles: usize,
    nodes: usize,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    // Micro-benchmark variants: build the example with hotpath and run it.
    if let Some(v) = variant {
        return match v {
            "pmtiles" => {
                let ctx = HarnessContext::new(
                    req.dev_config,
                    req.project,
                    req.project_root,
                    req.build_root,
                    "hotpath pmtiles",
                    req.force,
                )?;
                super::bench_pmtiles::run_hotpath(
                    &ctx.harness,
                    &ctx.paths.scratch_dir,
                    req.effective_build_root(),
                    tiles,
                    req.runs,
                    req.is_alloc(),
                )
            }
            "node-store" => {
                let ctx = HarnessContext::new(
                    req.dev_config,
                    req.project,
                    req.project_root,
                    req.build_root,
                    "hotpath node-store",
                    req.force,
                )?;
                super::bench_node_store::run_hotpath(
                    &ctx.harness,
                    &ctx.paths.scratch_dir,
                    req.effective_build_root(),
                    nodes,
                    req.runs,
                    req.is_alloc(),
                )
            }
            other => Err(DevError::Config(format!(
                "unknown hotpath variant '{other}' for elivagar (expected: pmtiles, node-store)"
            ))),
        };
    }

    let hotpath_features = req.hotpath_features();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        None,
        &hotpath_features,
        true,
        "hotpath",
        req.force,
    )?;
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let risk = if req.is_alloc() {
        oom::MemoryRisk::AllocTracking
    } else {
        oom::MemoryRisk::Normal
    };
    oom::check_memory(file_mb, &risk, req.no_mem_check)?;
    super::hotpath::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        file_mb,
        req.runs,
        req.is_alloc(),
        opts,
        req.project_root,
    )
}
