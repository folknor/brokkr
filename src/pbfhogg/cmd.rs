use std::path::Path;

use crate::cli::VerifyCommand;
use crate::config;
use crate::context::{bootstrap, bootstrap_config, BenchContext, HarnessContext};
use crate::error::DevError;
use crate::oom;
use crate::output;
use crate::preflight;
use crate::project::{self, Project};
use crate::resolve::{
    resolve_bbox, resolve_osc_path, resolve_pbf_path, resolve_pbf_with_size, resolve_raw_pbf_path,
};
use crate::tools;

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench_commands(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    command: &str,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench commands")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let commands = super::bench_commands::parse_command(command)?;
    let osc_path = resolve_osc_path(None, dataset, &ctx.paths, project_root).ok();
    super::bench_commands::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        osc_path.as_deref(),
        Some(&ctx.paths.scratch_dir),
        file_mb,
        runs,
        &commands,
        project_root,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench_extract(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    bbox: Option<&str>,
    strategies_str: &str,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench extract")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let bbox = resolve_bbox(bbox, dataset, &ctx.paths)?;
    let strategies = super::bench_extract::parse_strategies(strategies_str)?;
    super::bench_extract::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &bbox, &strategies, project_root)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench_allocator(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    _features: &[String],
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench allocator")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let effective = build_root.unwrap_or(project_root);
    super::bench_allocator::run(&ctx.harness, &pbf_path, file_mb, runs, effective)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench_blob_filter(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf_indexed: Option<&str>,
    pbf_raw: Option<&str>,
    runs: usize,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench blob-filter")?;
    let (indexed_path, file_mb) = resolve_pbf_with_size(pbf_indexed, dataset, &ctx.paths, project_root)?;
    let raw_path = resolve_raw_pbf_path(pbf_raw, dataset, &ctx.paths)?;
    super::bench_blob_filter::run(&ctx.harness, &ctx.binary, &indexed_path, &raw_path, file_mb, runs, project_root)
}

pub(crate) fn bench_planetiler(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench planetiler")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    super::bench_planetiler::run(&ctx.harness, &pbf_path, file_mb, runs, &ctx.paths.data_dir, project_root)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench_read(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    modes_str: &str,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench read")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let modes = super::bench_read::parse_modes(modes_str)?;
    super::bench_read::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &modes, project_root)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench_write(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    compression_str: &str,
    features: &[String],
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench write")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let compressions = super::parse_compressions(compression_str, true)?;
    super::bench_write::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, runs, &compressions, project_root)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench_merge(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    runs: usize,
    uring: bool,
    compression_str: &str,
    features: &[String],
) -> Result<(), DevError> {
    if uring {
        preflight::run_preflight(&preflight::uring_checks())?;
    }

    let mut all_features: Vec<&str> = features.iter().map(String::as_str).collect();
    if uring {
        all_features.push("linux-io-uring");
    }
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), &all_features, true, "bench merge")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let osc_path = resolve_osc_path(osc, dataset, &ctx.paths, project_root)?;
    let compressions = super::parse_compressions(compression_str, false)?;
    super::bench_merge::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        &osc_path,
        file_mb,
        runs,
        &compressions,
        uring,
        &ctx.paths.scratch_dir,
        project_root,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn bench_all(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    runs: usize,
    _features: &[String],
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "bench all")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let effective = build_root.unwrap_or(project_root);
    super::bench_all::run(&ctx.harness, &ctx.paths, effective, &pbf_path, file_mb, runs, dataset)
}

pub(crate) fn verify(dev_config: &config::DevConfig, _project: Project, project_root: &Path, build_root: Option<&Path>, verify: VerifyCommand) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let harness = super::verify::VerifyHarness::new(project_root, &pi.target_dir, build_root)?;

    match verify {
        VerifyCommand::Sort { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            super::verify_sort::run(&harness, &pbf_path)
        }
        VerifyCommand::Cat { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            super::verify_cat::run(&harness, &pbf_path)
        }
        VerifyCommand::Extract { dataset, pbf, bbox } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let bbox = resolve_bbox(bbox.as_deref(), &dataset, &paths)?;
            super::verify_extract::run(&harness, &pbf_path, &bbox)
        }
        VerifyCommand::TagsFilter { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            super::verify_tags_filter::run(&harness, &pbf_path)
        }
        VerifyCommand::GetidRemoveid { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            super::verify_getid_removeid::run(&harness, &pbf_path)
        }
        VerifyCommand::AddLocationsToWays { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            super::verify_add_locations::run(&harness, &pbf_path)
        }
        VerifyCommand::CheckRefs { dataset, pbf } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            super::verify_check_refs::run(&harness, &pbf_path)
        }
        VerifyCommand::Merge { dataset, pbf, osc } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc.as_deref(), &dataset, &paths, project_root)?;
            let osmosis = match tools::ensure_osmosis(&paths.data_dir, project_root) {
                Ok(tools) => Some(tools),
                Err(e) => {
                    output::verify_msg(&format!("osmosis not available (non-fatal): {e}"));
                    None
                }
            };
            super::verify_merge::run(&harness, &pbf_path, &osc_path, osmosis.as_ref())
        }
        VerifyCommand::DeriveChanges { dataset, pbf, osc } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc.as_deref(), &dataset, &paths, project_root)?;
            super::verify_derive_changes::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::Diff { dataset, pbf, osc } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc.as_deref(), &dataset, &paths, project_root)?;
            super::verify_diff::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::All { dataset, pbf, osc, bbox } => {
            let pbf_path = resolve_pbf_path(pbf.as_deref(), &dataset, &paths, project_root)?;
            let osc_path = resolve_osc_path(osc.as_deref(), &dataset, &paths, project_root).ok();
            let bbox_str = resolve_bbox(bbox.as_deref(), &dataset, &paths).ok();
            super::verify_all::run(
                &harness,
                &pbf_path,
                osc_path.as_deref(),
                bbox_str.as_deref(),
                &paths.data_dir,
                project_root,
            )
        }
        // Nidhogg variants are handled above in cmd_verify().
        VerifyCommand::Batch
        | VerifyCommand::NidGeocode { .. }
        | VerifyCommand::Readonly { .. } => unreachable!(),
    }
}

pub(crate) fn download(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    region: &str,
    osc_url: Option<&str>,
) -> Result<(), DevError> {
    project::require(project, Project::Pbfhogg, "download")?;

    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    super::download::run(
        region,
        osc_url,
        &paths.data_dir,
        project_root,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn hotpath(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    runs: usize,
    all_features: &[&str],
    no_mem_check: bool,
    alloc: bool,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(dev_config, project, project_root, build_root, Some("pbfhogg-cli"), all_features, true, "hotpath")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    let risk = if alloc { oom::MemoryRisk::AllocTracking } else { oom::MemoryRisk::Normal };
    oom::check_memory(file_mb, &risk, no_mem_check)?;
    let osc_path = resolve_osc_path(osc, dataset, &ctx.paths, project_root)?;

    // Try to get raw PBF path (optional).
    let pbf_raw_path = ctx.paths
        .datasets
        .get(dataset)
        .and_then(|ds| ds.pbf_raw.as_ref())
        .map(|raw_file| ctx.paths.data_dir.join(raw_file))
        .filter(|p| p.exists());

    super::hotpath::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        pbf_raw_path.as_deref(),
        &osc_path,
        file_mb,
        runs,
        alloc,
        &ctx.paths.scratch_dir,
        project_root,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn profile(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    dataset: &str,
    pbf: Option<&str>,
    osc: Option<&str>,
    features: &[String],
    no_mem_check: bool,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(dev_config, project, project_root, build_root, "profile")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(pbf, dataset, &ctx.paths, project_root)?;
    oom::check_memory(file_mb, &oom::MemoryRisk::AllocTracking, no_mem_check)?;
    let osc_path = resolve_osc_path(osc, dataset, &ctx.paths, project_root)?;

    // Try to get raw PBF path (optional).
    let pbf_raw_path = ctx.paths
        .datasets
        .get(dataset)
        .and_then(|ds| ds.pbf_raw.as_ref())
        .map(|raw_file| ctx.paths.data_dir.join(raw_file))
        .filter(|p| p.exists());

    super::profile::run(
        &ctx.harness,
        &pbf_path,
        pbf_raw_path.as_deref(),
        &osc_path,
        dataset,
        file_mb,
        &ctx.paths.scratch_dir,
        features,
        build_root.unwrap_or(project_root),
    )
}
