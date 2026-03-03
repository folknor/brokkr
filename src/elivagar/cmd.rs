use std::path::Path;

use crate::config;
use crate::context::{bootstrap, bootstrap_config, BenchContext, HarnessContext};
use crate::error::DevError;
use crate::oom;
use crate::preflight;
use crate::project::{self, Project};
use crate::request::{BenchRequest, HotpathRequest, ProfileRequest};
use crate::resolve::resolve_pbf_with_size;

pub(crate) fn bench_node_store(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    nodes: usize,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let db_root = build_root.map(|_| project_root);
    let effective = build_root.unwrap_or(project_root);
    let harness = crate::harness::BenchHarness::new(&paths, effective, db_root, project, "bench node-store")?;
    super::bench_node_store::run(&harness, effective, nodes, runs)
}

pub(crate) fn bench_pmtiles(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    tiles: usize,
    runs: usize,
) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let db_root = build_root.map(|_| project_root);
    let effective = build_root.unwrap_or(project_root);
    let harness = crate::harness::BenchHarness::new(&paths, effective, db_root, project, "bench pmtiles")?;
    super::bench_pmtiles::run(&harness, effective, tiles, runs)
}

pub(crate) fn bench_self(
    req: &BenchRequest,
    skip_to: Option<&str>,
    no_ocean: bool,
    force_sorted: bool,
    compression_level: Option<u32>,
    allow_unsafe_flat_index: bool,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, None, &feat_refs, true, "bench self")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
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
        no_ocean,
        force_sorted,
        compression_level,
        allow_unsafe_flat_index,
    )
}

pub(crate) fn bench_planetiler(
    req: &BenchRequest,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench planetiler")?;
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
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench tilemaker")?;
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
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench all")?;
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

pub(crate) fn hotpath(
    req: &HotpathRequest,
    variant: Option<&str>,
    tiles: usize,
    nodes: usize,
    no_ocean: bool,
    force_sorted: bool,
    allow_unsafe_flat_index: bool,
) -> Result<(), DevError> {
    // Micro-benchmark variants: build the example with hotpath and run it.
    if let Some(v) = variant {
        return match v {
            "pmtiles" => {
                let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "hotpath pmtiles")?;
                super::bench_pmtiles::run_hotpath(&ctx.harness, &ctx.paths.scratch_dir, req.build_root.unwrap_or(req.project_root), tiles, req.runs, req.alloc)
            }
            "node-store" => {
                let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "hotpath node-store")?;
                super::bench_node_store::run_hotpath(&ctx.harness, &ctx.paths.scratch_dir, req.build_root.unwrap_or(req.project_root), nodes, req.runs, req.alloc)
            }
            other => Err(DevError::Config(format!(
                "unknown hotpath variant '{other}' for elivagar (expected: pmtiles, node-store)"
            ))),
        };
    }

    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, None, req.all_features, true, "hotpath")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let risk = if req.alloc { oom::MemoryRisk::AllocTracking } else { oom::MemoryRisk::Normal };
    oom::check_memory(file_mb, &risk, req.no_mem_check)?;
    super::hotpath::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        file_mb,
        req.runs,
        req.alloc,
        no_ocean,
        force_sorted,
        allow_unsafe_flat_index,
        req.project_root,
    )
}

pub(crate) fn profile(
    req: &ProfileRequest,
    tool: Option<&str>,
    no_ocean: bool,
    force_sorted: bool,
    allow_unsafe_flat_index: bool,
) -> Result<(), DevError> {
    let tool_name = tool.unwrap_or("perf");
    preflight::run_preflight(&preflight::profile_checks(tool_name))?;
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "profile")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    oom::check_memory(file_mb, &oom::MemoryRisk::AllocTracking, req.no_mem_check)?;
    let effective = req.build_root.unwrap_or(req.project_root);
    super::profile::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        &ctx.paths.data_dir,
        &ctx.paths.scratch_dir,
        tool_name,
        no_ocean,
        force_sorted,
        allow_unsafe_flat_index,
        req.features,
        effective,
    )
}
