use std::path::Path;

use crate::build;
use crate::config;
use crate::context::{BenchContext, HarnessContext, bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::oom;
use crate::output;
use crate::preflight;
use crate::project::{self, Project};
use crate::request::{BenchRequest, HotpathRequest};
use crate::resolve::{
    self, file_size_mb, resolve_bbox, resolve_nidhogg_data_dir, resolve_pbf_path,
    resolve_pbf_with_size,
};

fn resolve_port(dev_config: &config::DevConfig) -> u16 {
    // Check PORT env var first
    if let Ok(port_str) = std::env::var("PORT")
        && let Ok(port) = port_str.parse::<u16>()
    {
        return port;
    }
    // Try brokkr.toml host config
    if let Ok(hostname) = config::hostname()
        && let Some(host) = dev_config.hosts.get(&hostname)
        && let Some(port) = host.port
    {
        return port;
    }
    super::server::DEFAULT_PORT
}

fn build_config_with_features(package: Option<&str>, features: &[String]) -> build::BuildConfig {
    if features.is_empty() {
        build::BuildConfig::release(package)
    } else {
        build::BuildConfig::release_with_owned_features(package, features)
    }
}

pub(crate) fn serve(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    data_dir: Option<&str>,
    dataset: &str,
    tiles_variant: Option<&str>,
    features: &[String],
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "serve")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let data_dir_str = match data_dir {
        Some(d) => d.to_owned(),
        None => resolve_nidhogg_data_dir(dataset, &paths)?
            .display()
            .to_string(),
    };

    let tiles_path = match tiles_variant {
        Some(v) => Some(resolve::resolve_pmtiles_path(
            dataset,
            v,
            &paths,
            project_root,
        )?),
        None => None,
    };
    let tiles_str = tiles_path.as_ref().map(|p| p.display().to_string());

    let port = resolve_port(dev_config);
    let build_config = build_config_with_features(Some("nidhogg"), features);
    let binary = build::cargo_build(&build_config, project_root)?;
    super::server::serve(
        &binary,
        &data_dir_str,
        tiles_str.as_deref(),
        port,
        project_root,
    )
}

pub(crate) fn stop(project: Project, project_root: &Path) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "stop")?;
    super::server::stop(project_root)
}

pub(crate) fn status(
    dev_config: &config::DevConfig,
    project: Project,
    _project_root: &Path,
) -> Result<(), DevError> {
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
    variant: &str,
    dataset: &str,
    features: &[String],
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "ingest")?;
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let pbf_path = resolve_pbf_path(dataset, variant, &paths, project_root)?;

    let data_dir = resolve_nidhogg_data_dir(dataset, &paths)?;

    let build_config = build_config_with_features(Some("nidhogg"), features);
    let binary = build::cargo_build(&build_config, project_root)?;
    super::ingest::run(&binary, &pbf_path, &data_dir, project_root)
}

pub(crate) fn update(
    project: Project,
    project_root: &Path,
    args: &[String],
    features: &[String],
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "update")?;
    let mut config = build_config_with_features(Some("nidhogg"), features);
    config.bin = Some("nidhogg-update".into());
    let binary = build::cargo_build(&config, project_root)?;
    super::update::run(&binary, args, project_root)
}

pub(crate) fn query(
    dev_config: &config::DevConfig,
    project: Project,
    _project_root: &Path,
    json: Option<&str>,
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "query")?;
    let port = resolve_port(dev_config);
    super::query::run(port, json)
}

pub(crate) fn geocode(
    dev_config: &config::DevConfig,
    project: Project,
    _project_root: &Path,
    term: &str,
) -> Result<(), DevError> {
    project::require(project, Project::Nidhogg, "geocode")?;
    let port = resolve_port(dev_config);
    super::geocode::run(port, term)
}

pub(crate) fn bench_api(req: &BenchRequest, query: Option<&str>) -> Result<(), DevError> {
    let ctx = HarnessContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        "bench api",
        req.force,
    )?;
    let port = resolve_port(req.dev_config);

    // Resolve dataset PBF for metadata recording.
    let pbf_path = resolve_pbf_path(req.dataset, req.variant, &ctx.paths, req.project_root).ok();
    let input_file = pbf_path
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str());
    let input_mb = pbf_path.as_ref().map(|p| file_size_mb(p)).transpose()?;

    let bbox = resolve_bbox(None, req.dataset, &ctx.paths)?;
    super::bench_api::run(
        &ctx.harness,
        port,
        req.runs,
        query,
        input_file,
        input_mb,
        &bbox,
    )
}

pub(crate) fn bench_ingest(req: &BenchRequest) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
        &feat_refs,
        true,
        "bench ingest",
        req.force,
    )?;
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_ingest::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        file_mb,
        req.runs,
        &ctx.paths.scratch_dir,
        req.project_root,
    )
}

pub(crate) fn bench_tiles(
    req: &BenchRequest,
    tiles_variant: Option<&str>,
    uring: bool,
) -> Result<(), DevError> {
    if uring {
        preflight::run_preflight(&preflight::uring_checks())?;
    }
    let mut all_features: Vec<&str> = req.features.iter().map(String::as_str).collect();
    if uring {
        all_features.push("linux-io-uring");
    }
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
        &all_features,
        true,
        "bench tiles",
        req.force,
    )?;
    let data_dir = resolve_nidhogg_data_dir(req.dataset, &ctx.paths)?;
    let port = resolve_port(req.dev_config);

    let (tiles_path, tiles_mb) = match tiles_variant {
        Some(v) => {
            resolve::resolve_pmtiles_with_size(req.dataset, v, &ctx.paths, req.project_root)?
        }
        None => {
            resolve::resolve_default_pmtiles_with_size(req.dataset, &ctx.paths, req.project_root)?
        }
    };
    let tiles_hash = {
        let ds = ctx.paths.datasets.get(req.dataset);
        ds.and_then(|d| {
            if let Some(v) = tiles_variant {
                d.pmtiles.get(v)
            } else if d.pmtiles.len() == 1 {
                d.pmtiles.values().next()
            } else {
                None
            }
        })
        .and_then(|e| e.xxhash.clone())
    };

    let tiles_file = tiles_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let tiles_str = tiles_path.display().to_string();

    super::bench_tiles::run(
        &ctx.harness,
        &ctx.binary,
        &data_dir.display().to_string(),
        &tiles_str,
        port,
        tiles_file,
        tiles_hash.as_deref(),
        tiles_mb,
        req.runs,
        req.project_root,
    )
}

pub(crate) fn verify_batch(
    dev_config: &config::DevConfig,
    _project: Project,
    project_root: &Path,
    dataset: &str,
) -> Result<(), DevError> {
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let port = resolve_port(dev_config);
    let bbox = resolve_bbox(None, dataset, &paths)?;
    super::verify_batch::run(port, &bbox)
}

pub(crate) fn verify_geocode(
    dev_config: &config::DevConfig,
    _project: Project,
    _project_root: &Path,
    queries: &[String],
) -> Result<(), DevError> {
    let port = resolve_port(dev_config);
    let query_refs: Vec<&str> = if queries.is_empty() {
        super::client::GEOCODE_TEST_QUERIES.to_vec()
    } else {
        queries.iter().map(String::as_str).collect()
    };
    super::verify_geocode::run(port, &query_refs)
}

pub(crate) fn verify_readonly(
    dev_config: &config::DevConfig,
    _project: Project,
    project_root: &Path,
    dataset: &str,
    features: &[String],
) -> Result<(), DevError> {
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let port = resolve_port(dev_config);

    let data_dir_str = resolve_nidhogg_data_dir(dataset, &paths)?
        .display()
        .to_string();

    let build_config = build_config_with_features(Some("nidhogg"), features);
    let binary = build::cargo_build(&build_config, project_root)?;
    let bbox = resolve_bbox(None, dataset, &paths)?;
    super::verify_readonly::run(&binary, &data_dir_str, port, project_root, &bbox)
}

pub(crate) fn hotpath(req: &HotpathRequest) -> Result<(), DevError> {
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("nidhogg"),
        req.all_features,
        true,
        "hotpath",
        req.force,
    )?;
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let risk = if req.alloc {
        oom::MemoryRisk::AllocTracking
    } else {
        oom::MemoryRisk::Normal
    };
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
