use std::path::Path;

use crate::build;
use crate::config;
use crate::context::{bootstrap, bootstrap_config, BenchContext, HarnessContext};
use crate::error::DevError;
use crate::oom;
use crate::output;
use crate::preflight;
use crate::project::{self, Project};
use crate::request::{BenchRequest, HotpathRequest, ProfileRequest};
use crate::resolve::{file_size_mb, resolve_nidhogg_data_dir, resolve_pbf_path, resolve_pbf_with_size};

fn resolve_port(dev_config: &config::DevConfig) -> u16 {
    // Check PORT env var first
    if let Ok(port_str) = std::env::var("PORT")
        && let Ok(port) = port_str.parse::<u16>() {
            return port;
        }
    // Try brokkr.toml host config
    if let Ok(hostname) = config::hostname()
        && let Some(host) = dev_config.hosts.get(&hostname)
            && let Some(port) = host.port {
                return port;
            }
    super::server::DEFAULT_PORT
}

pub(crate) fn serve(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    data_dir: Option<&str>,
    dataset: &str,
    tiles: Option<&str>,
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "serve")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let data_dir_str = match data_dir {
        Some(d) => d.to_owned(),
        None => resolve_nidhogg_data_dir(dataset, &paths)?.display().to_string(),
    };

    let port = resolve_port(dev_config);
    let binary = build::cargo_build(&build::BuildConfig::release(Some("nidhogg")), project_root)?;
    super::server::serve(&binary, &data_dir_str, tiles, port, project_root)
}

pub(crate) fn stop(project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "stop")?;
    super::server::stop(project_root)
}

pub(crate) fn status(dev_config: &config::DevConfig, project: Project, _project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "status")?;
    let port = resolve_port(dev_config);
    let running = super::server::status(port)?;
    if running {
        output::run_msg(&format!("server running on port {port}"));
    } else {
        output::run_msg(&format!("server not running on port {port}"));
    }
    Ok(())
}

pub(crate) fn ingest(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    pbf: Option<&str>,
    dataset: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "ingest")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(pbf, dataset, &paths, project_root)?;

    let data_dir = resolve_nidhogg_data_dir(dataset, &paths)?;

    let binary = build::cargo_build(&build::BuildConfig::release(Some("nidhogg")), project_root)?;
    super::ingest::run(&binary, &pbf_path, &data_dir, project_root)
}

pub(crate) fn update(project: Project, project_root: &Path, args: &[String]) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "update")?;
    let mut config = build::BuildConfig::release(Some("nidhogg"));
    config.bin = Some("nidhogg-update".into());
    let binary = build::cargo_build(&config, project_root)?;
    super::update::run(&binary, args, project_root)
}

pub(crate) fn query(dev_config: &config::DevConfig, project: Project, _project_root: &Path, json: Option<&str>) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "query")?;
    let port = resolve_port(dev_config);
    super::query::run(port, json)
}

pub(crate) fn geocode(dev_config: &config::DevConfig, project: Project, _project_root: &Path, term: &str) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "geocode")?;
    let port = resolve_port(dev_config);
    super::geocode::run(port, term)
}

pub(crate) fn bench_api(
    req: &BenchRequest,
    query: Option<&str>,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench api")?;
    let port = resolve_port(req.dev_config);

    // Resolve dataset PBF for metadata recording.
    let pbf_path = resolve_pbf_path(None, req.dataset, &ctx.paths, req.project_root).ok();
    let input_file = pbf_path.as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let input_mb = pbf_path.as_ref().map(|p| file_size_mb(p)).transpose()?;

    super::bench_api::run(&ctx.harness, port, req.runs, query, input_file, input_mb)
}

pub(crate) fn bench_ingest(
    req: &BenchRequest,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("nidhogg"), &feat_refs, true, "bench ingest")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.pbf, req.dataset, &ctx.paths, req.project_root)?;
    super::bench_ingest::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, req.runs, &ctx.paths.scratch_dir, req.project_root)
}

pub(crate) fn verify_batch(dev_config: &config::DevConfig, _project: Project, _project_root: &Path) -> Result<(), DevError> {
    let port = resolve_port(dev_config);
    super::verify_batch::run(port)
}

pub(crate) fn verify_geocode(dev_config: &config::DevConfig, _project: Project, _project_root: &Path, queries: &[String]) -> Result<(), DevError> {
    let port = resolve_port(dev_config);
    let default_queries = ["Kobenhavn", "Aarhus", "Odense"];
    let query_refs: Vec<&str> = if queries.is_empty() {
        default_queries.to_vec()
    } else {
        queries.iter().map(String::as_str).collect()
    };
    super::verify_geocode::run(port, &query_refs)
}

pub(crate) fn verify_readonly(dev_config: &config::DevConfig, _project: Project, project_root: &Path, dataset: &str) -> Result<(), DevError> {
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let port = resolve_port(dev_config);

    let data_dir_str = resolve_nidhogg_data_dir(dataset, &paths)?.display().to_string();

    let binary = build::cargo_build(&build::BuildConfig::release(Some("nidhogg")), project_root)?;
    super::verify_readonly::run(&binary, &data_dir_str, port, project_root)
}

pub(crate) fn hotpath(
    req: &HotpathRequest,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("nidhogg"), req.all_features, true, "hotpath")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.pbf, req.dataset, &ctx.paths, req.project_root)?;
    let risk = if req.alloc { oom::MemoryRisk::AllocTracking } else { oom::MemoryRisk::Normal };
    oom::check_memory(file_mb, &risk, req.no_mem_check)?;
    super::hotpath::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        &ctx.paths.scratch_dir,
        file_mb,
        req.runs,
        req.alloc,
        req.project_root,
    )
}

pub(crate) fn profile(
    req: &ProfileRequest,
    tool: Option<&str>,
) -> Result<(), DevError> {
    let tool_name = tool.unwrap_or("perf");
    preflight::run_preflight(&preflight::profile_checks(tool_name))?;
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "profile")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.pbf, req.dataset, &ctx.paths, req.project_root)?;
    oom::check_memory(file_mb, &oom::MemoryRisk::AllocTracking, req.no_mem_check)?;

    let data_dir = ctx.paths
        .datasets
        .get(req.dataset)
        .and_then(|ds| ds.data_dir.as_ref())
        .map(|d| ctx.paths.data_dir.join(d))
        .unwrap_or_else(|| ctx.paths.data_dir.clone());

    super::profile::run(
        &ctx.harness,
        &pbf_path,
        file_mb,
        &data_dir,
        &ctx.paths.scratch_dir,
        tool_name,
        req.features,
        req.build_root.unwrap_or(req.project_root),
    )
}
