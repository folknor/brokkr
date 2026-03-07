use std::path::Path;

use crate::config;
use crate::context::{bootstrap, bootstrap_config, BenchContext, HarnessContext};
use crate::error::DevError;
use crate::oom;
use crate::preflight;
use crate::project::{self, Project};
use crate::request::{BenchRequest, HotpathRequest, ProfileRequest};
use crate::resolve::{resolve_pbf_with_size, resolve_default_pmtiles_path, resolve_pmtiles_path};

pub(crate) fn bench_node_store(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    nodes: usize,
    runs: usize,
    force: bool,
) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let db_root = build_root.map(|_| project_root);
    let effective = build_root.unwrap_or(project_root);
    let harness = crate::harness::BenchHarness::new(&paths, effective, db_root, project, "bench node-store", force)?;
    super::bench_node_store::run(&harness, effective, nodes, runs)
}

pub(crate) fn bench_pmtiles(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    tiles: usize,
    runs: usize,
    force: bool,
) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let db_root = build_root.map(|_| project_root);
    let effective = build_root.unwrap_or(project_root);
    let harness = crate::harness::BenchHarness::new(&paths, effective, db_root, project, "bench pmtiles", force)?;
    super::bench_pmtiles::run(&harness, effective, tiles, runs)
}

pub(crate) fn bench_self(
    req: &BenchRequest,
    skip_to: Option<&str>,
    compression_level: Option<u32>,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, None, &feat_refs, true, "bench self", req.force)?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_self::run(
        &ctx.harness, &ctx.binary, &pbf_path, file_mb, req.runs,
        &ctx.paths.data_dir, &ctx.paths.scratch_dir, req.project_root,
        skip_to, compression_level, opts,
    )
}

pub(crate) fn bench_planetiler(
    req: &BenchRequest,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench planetiler", req.force)?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
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

pub(crate) fn bench_tilemaker(
    req: &BenchRequest,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench tilemaker", req.force)?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
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

pub(crate) fn bench_all(
    req: &BenchRequest,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench all", req.force)?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let effective = req.build_root.unwrap_or(req.project_root);
    super::bench_all::run(
        &ctx.harness,
        &ctx.paths,
        effective,
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

pub(crate) fn download_ocean(dev_config: &config::DevConfig, project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Elivagar, "download-ocean")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    super::download_ocean::run(&paths.data_dir)
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
    req: &HotpathRequest,
    variant: Option<&str>,
    tiles: usize,
    nodes: usize,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    // Micro-benchmark variants: build the example with hotpath and run it.
    if let Some(v) = variant {
        return match v {
            "pmtiles" => {
                let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "hotpath pmtiles", req.force)?;
                super::bench_pmtiles::run_hotpath(&ctx.harness, &ctx.paths.scratch_dir, req.build_root.unwrap_or(req.project_root), tiles, req.runs, req.alloc)
            }
            "node-store" => {
                let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "hotpath node-store", req.force)?;
                super::bench_node_store::run_hotpath(&ctx.harness, &ctx.paths.scratch_dir, req.build_root.unwrap_or(req.project_root), nodes, req.runs, req.alloc)
            }
            other => Err(DevError::Config(format!(
                "unknown hotpath variant '{other}' for elivagar (expected: pmtiles, node-store)"
            ))),
        };
    }

    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, None, req.all_features, true, "hotpath", req.force)?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let risk = if req.alloc { oom::MemoryRisk::AllocTracking } else { oom::MemoryRisk::Normal };
    oom::check_memory(file_mb, &risk, req.no_mem_check)?;
    super::hotpath::run(
        &ctx.harness, &ctx.binary, &pbf_path, &ctx.paths.data_dir, &ctx.paths.scratch_dir,
        file_mb, req.runs, req.alloc, opts, req.project_root,
    )
}

pub(crate) fn profile(
    req: &ProfileRequest,
    tool: Option<&str>,
    opts: &super::PipelineOpts,
) -> Result<(), DevError> {
    let tool_name = tool.unwrap_or("perf");
    preflight::run_preflight(&preflight::profile_checks(tool_name))?;
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "profile", false)?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    oom::check_memory(file_mb, &oom::MemoryRisk::AllocTracking, req.no_mem_check)?;
    let effective = req.build_root.unwrap_or(req.project_root);
    super::profile::run(
        &ctx.harness, &pbf_path, file_mb, &ctx.paths.data_dir, &ctx.paths.scratch_dir,
        tool_name, opts, req.features, effective,
    )
}
