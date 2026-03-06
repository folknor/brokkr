use std::path::Path;

use crate::cli::VerifyCommand;
use crate::config;
use crate::context::{bootstrap, bootstrap_config, BenchContext, HarnessContext};
use crate::error::DevError;
use crate::oom;
use crate::output;
use crate::preflight;
use crate::project::{self, Project};
use crate::request::{BenchRequest, HotpathRequest, ProfileRequest};
use crate::resolve::{
    self, resolve_bbox, resolve_default_osc_path, resolve_pbf_path, resolve_pbf_with_size,
};
use crate::tools;

pub(crate) fn bench_commands(
    req: &BenchRequest,
    command: &str,
    osc_seq: Option<&str>,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench commands")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let commands = super::bench_commands::parse_command(command)?;
    let osc_path = match osc_seq {
        Some(seq) => resolve::resolve_osc_path(req.dataset, seq, &ctx.paths, req.project_root).ok(),
        None => resolve_default_osc_path(req.dataset, &ctx.paths, req.project_root).ok(),
    };
    super::bench_commands::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        osc_path.as_deref(),
        Some(&ctx.paths.scratch_dir),
        file_mb,
        req.runs,
        &commands,
        req.project_root,
    )
}

pub(crate) fn bench_extract(
    req: &BenchRequest,
    bbox: Option<&str>,
    strategies_str: &str,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench extract")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let bbox = resolve_bbox(bbox, req.dataset, &ctx.paths)?;
    let strategies = super::bench_extract::parse_strategies(strategies_str)?;
    super::bench_extract::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, req.runs, &bbox, &strategies, req.project_root)
}

pub(crate) fn bench_allocator(
    req: &BenchRequest,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench allocator")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let effective = req.build_root.unwrap_or(req.project_root);
    super::bench_allocator::run(&ctx.harness, &pbf_path, file_mb, req.runs, effective)
}

pub(crate) fn bench_blob_filter(
    req: &BenchRequest,
    raw_variant: &str,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench blob-filter")?;
    let (indexed_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let raw_path = resolve_pbf_path(req.dataset, raw_variant, &ctx.paths, req.project_root)?;
    super::bench_blob_filter::run(&ctx.harness, &ctx.binary, &indexed_path, &raw_path, file_mb, req.runs, req.project_root)
}

pub(crate) fn bench_planetiler(
    req: &BenchRequest,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "bench planetiler")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    super::bench_planetiler::run(&ctx.harness, &pbf_path, file_mb, req.runs, &ctx.paths.data_dir, req.project_root)
}

pub(crate) fn bench_read(
    req: &BenchRequest,
    modes_str: &str,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench read")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let modes = super::bench_read::parse_modes(modes_str)?;
    super::bench_read::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, req.runs, &modes, req.project_root)
}

pub(crate) fn bench_write(
    req: &BenchRequest,
    compression_str: &str,
) -> Result<(), DevError> {
    let feat_refs: Vec<&str> = req.features.iter().map(String::as_str).collect();
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("pbfhogg-cli"), &feat_refs, true, "bench write")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let compressions = super::parse_compressions(compression_str, true)?;
    super::bench_write::run(&ctx.harness, &ctx.binary, &pbf_path, file_mb, req.runs, &compressions, req.project_root)
}

pub(crate) fn bench_merge(
    req: &BenchRequest,
    osc_seq: Option<&str>,
    uring: bool,
    compression_str: &str,
) -> Result<(), DevError> {
    if uring {
        preflight::run_preflight(&preflight::uring_checks())?;
    }

    let mut all_features: Vec<&str> = req.features.iter().map(String::as_str).collect();
    if uring {
        all_features.push("linux-io-uring");
    }
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("pbfhogg-cli"), &all_features, true, "bench merge")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let osc_path = match osc_seq {
        Some(seq) => resolve::resolve_osc_path(req.dataset, seq, &ctx.paths, req.project_root)?,
        None => resolve_default_osc_path(req.dataset, &ctx.paths, req.project_root)?,
    };
    let compressions = super::parse_compressions(compression_str, false)?;
    super::bench_merge::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        &osc_path,
        file_mb,
        req.runs,
        &compressions,
        uring,
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
    super::bench_all::run(&ctx.harness, &ctx.paths, effective, &pbf_path, file_mb, req.runs, req.dataset)
}

pub(crate) fn verify(dev_config: &config::DevConfig, _project: Project, project_root: &Path, build_root: Option<&Path>, verify: VerifyCommand) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let harness = super::verify::VerifyHarness::new(project_root, &pi.target_dir, build_root)?;

    match verify {
        VerifyCommand::Sort { dataset, variant } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            super::verify_sort::run(&harness, &pbf_path)
        }
        VerifyCommand::Cat { dataset, variant } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            super::verify_cat::run(&harness, &pbf_path)
        }
        VerifyCommand::Extract { dataset, variant, bbox } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            let bbox = resolve_bbox(bbox.as_deref(), &dataset, &paths)?;
            super::verify_extract::run(&harness, &pbf_path, &bbox)
        }
        VerifyCommand::TagsFilter { dataset, variant } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            super::verify_tags_filter::run(&harness, &pbf_path)
        }
        VerifyCommand::GetidRemoveid { dataset, variant } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            super::verify_getid_removeid::run(&harness, &pbf_path)
        }
        VerifyCommand::AddLocationsToWays { dataset, variant } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            super::verify_add_locations::run(&harness, &pbf_path)
        }
        VerifyCommand::CheckRefs { dataset, variant } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            super::verify_check_refs::run(&harness, &pbf_path)
        }
        VerifyCommand::Merge { dataset, variant, osc_seq } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            let osc_path = match osc_seq.as_deref() {
                Some(seq) => resolve::resolve_osc_path(&dataset, seq, &paths, project_root)?,
                None => resolve_default_osc_path(&dataset, &paths, project_root)?,
            };
            let osmosis = match tools::ensure_osmosis(&paths.data_dir, project_root) {
                Ok(tools) => Some(tools),
                Err(e) => {
                    output::verify_msg(&format!("osmosis not available (non-fatal): {e}"));
                    None
                }
            };
            super::verify_merge::run(&harness, &pbf_path, &osc_path, osmosis.as_ref())
        }
        VerifyCommand::DeriveChanges { dataset, variant, osc_seq } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            let osc_path = match osc_seq.as_deref() {
                Some(seq) => resolve::resolve_osc_path(&dataset, seq, &paths, project_root)?,
                None => resolve_default_osc_path(&dataset, &paths, project_root)?,
            };
            super::verify_derive_changes::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::Diff { dataset, variant, osc_seq } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            let osc_path = match osc_seq.as_deref() {
                Some(seq) => resolve::resolve_osc_path(&dataset, seq, &paths, project_root)?,
                None => resolve_default_osc_path(&dataset, &paths, project_root)?,
            };
            super::verify_diff::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::All { dataset, variant, osc_seq, bbox } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            let osc_path = match osc_seq.as_deref() {
                Some(seq) => resolve::resolve_osc_path(&dataset, seq, &paths, project_root).ok(),
                None => resolve_default_osc_path(&dataset, &paths, project_root).ok(),
            };
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
        // Elivagar and nidhogg variants are handled above in cmd_verify().
        VerifyCommand::ElivVerify { .. }
        | VerifyCommand::Batch
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

pub(crate) fn hotpath(
    req: &HotpathRequest,
    osc_seq: Option<&str>,
) -> Result<(), DevError> {
    let ctx = BenchContext::new(req.dev_config, req.project, req.project_root, req.build_root, Some("pbfhogg-cli"), req.all_features, true, "hotpath")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let risk = if req.alloc { oom::MemoryRisk::AllocTracking } else { oom::MemoryRisk::Normal };
    oom::check_memory(file_mb, &risk, req.no_mem_check)?;
    let osc_path = match osc_seq {
        Some(seq) => resolve::resolve_osc_path(req.dataset, seq, &ctx.paths, req.project_root)?,
        None => resolve_default_osc_path(req.dataset, &ctx.paths, req.project_root)?,
    };

    super::hotpath::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        &osc_path,
        file_mb,
        req.runs,
        req.alloc,
        &ctx.paths.scratch_dir,
        req.project_root,
    )
}

pub(crate) fn profile(
    req: &ProfileRequest,
    osc_seq: Option<&str>,
) -> Result<(), DevError> {
    let ctx = HarnessContext::new(req.dev_config, req.project, req.project_root, req.build_root, "profile")?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    oom::check_memory(file_mb, &oom::MemoryRisk::AllocTracking, req.no_mem_check)?;
    let osc_path = match osc_seq {
        Some(seq) => resolve::resolve_osc_path(req.dataset, seq, &ctx.paths, req.project_root)?,
        None => resolve_default_osc_path(req.dataset, &ctx.paths, req.project_root)?,
    };

    super::profile::run(
        &ctx.harness,
        &pbf_path,
        &osc_path,
        req.dataset,
        file_mb,
        &ctx.paths.scratch_dir,
        req.features,
        req.build_root.unwrap_or(req.project_root),
    )
}
