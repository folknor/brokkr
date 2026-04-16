use std::path::Path;

use crate::cli::VerifyCommand;
use crate::config;
use crate::context::{BenchContext, HarnessContext, bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::measure::MeasureRequest;
use crate::output;
use crate::preflight;
use crate::project::{self, Project};
use crate::resolve::{
    self, resolve_bbox, resolve_default_osc_path, resolve_pbf_path, resolve_pbf_with_size,
};
use crate::tools;

pub(crate) fn bench_read(req: &MeasureRequest, modes_str: &str) -> Result<(), DevError> {
    if req.dry_run {
        let paths = dry_run_paths(req)?;
        let _ = resolve_pbf_with_size(req.dataset, req.variant, &paths, req.project_root)?;
        let _ = super::bench_read::parse_modes(modes_str)?;
        output::run_msg("[dry-run] ok (bench read)");
        return Ok(());
    }

    let feat_refs = req.feat_refs();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &feat_refs,
        true,
        "bench read",
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let modes = super::bench_read::parse_modes(modes_str)?;
    super::bench_read::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        file_mb,
        req.runs(),
        &modes,
        req.project_root,
    )
}

pub(crate) fn bench_write(req: &MeasureRequest, compression_str: &str) -> Result<(), DevError> {
    if req.dry_run {
        let paths = dry_run_paths(req)?;
        let _ = resolve_pbf_with_size(req.dataset, req.variant, &paths, req.project_root)?;
        let _ = super::parse_compressions(compression_str, true)?;
        output::run_msg("[dry-run] ok (bench write)");
        return Ok(());
    }

    let feat_refs = req.feat_refs();
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &feat_refs,
        true,
        "bench write",
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
    let compressions = super::parse_compressions(compression_str, true)?;
    super::bench_write::run(
        &ctx.harness,
        &ctx.binary,
        &pbf_path,
        file_mb,
        req.runs(),
        &compressions,
        req.project_root,
    )
}

pub(crate) fn bench_merge(
    req: &MeasureRequest,
    osc_seq: Option<&str>,
    uring: bool,
    compression_str: &str,
) -> Result<(), DevError> {
    if req.dry_run {
        if uring {
            preflight::run_preflight(&preflight::uring_checks())?;
        }
        let paths = dry_run_paths(req)?;
        let (pbf_path, _file_mb) =
            resolve_pbf_with_size(req.dataset, req.variant, &paths, req.project_root)?;
        let osc_path = match osc_seq {
            Some(seq) => resolve::resolve_osc_path(req.dataset, seq, &paths, req.project_root)?,
            None => resolve_default_osc_path(req.dataset, &paths, req.project_root)?,
        };
        let _ = super::parse_compressions(compression_str, false)?;
        output::run_msg(&format!("[dry-run] pbf: {}", pbf_path.display()));
        output::run_msg(&format!("[dry-run] osc: {}", osc_path.display()));
        output::run_msg("[dry-run] ok (bench merge)");
        return Ok(());
    }

    if uring {
        preflight::run_preflight(&preflight::uring_checks())?;
    }

    let mut all_features = req.feat_refs();
    if uring {
        all_features.push("linux-io-uring");
    }
    let ctx = BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        Some("pbfhogg-cli"),
        &all_features,
        true,
        "bench merge",
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);
    let (pbf_path, file_mb) =
        resolve_pbf_with_size(req.dataset, req.variant, &ctx.paths, req.project_root)?;
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
        req.runs(),
        &compressions,
        uring,
        &ctx.paths.scratch_dir,
        req.project_root,
    )
}

pub(crate) fn bench_all(req: &MeasureRequest) -> Result<(), DevError> {
    if req.dry_run {
        let paths = dry_run_paths(req)?;
        let _ = resolve_pbf_with_size(req.dataset, req.variant, &paths, req.project_root)?;
        output::run_msg("[dry-run] ok (bench all)");
        return Ok(());
    }

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
    let effective = req.build_root.unwrap_or(req.project_root);
    super::bench_all::run(
        &ctx.harness,
        &ctx.paths,
        effective,
        &pbf_path,
        file_mb,
        req.runs(),
        req.dataset,
    )
}

/// Resolve host paths without building the binary, for dry-run validation.
fn dry_run_paths(req: &MeasureRequest) -> Result<config::ResolvedPaths, DevError> {
    let pi = bootstrap(req.build_root)?;
    bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn verify(
    dev_config: &config::DevConfig,
    _project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    verify: VerifyCommand,
    features: &[String],
) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let harness =
        super::verify::VerifyHarness::new(project_root, &pi.target_dir, build_root, features)?;

    match verify {
        VerifyCommand::Sort { pbf } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            super::verify_sort::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::Cat { pbf } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            super::verify_cat::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::Extract { pbf, bbox } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            let bbox = resolve_bbox(bbox.as_deref(), &pbf.dataset, &paths)?;
            super::verify_extract::run(&harness, &pbf_path, &bbox, pbf.direct_io)
        }
        VerifyCommand::MultiExtract {
            pbf,
            bbox,
            regions,
        } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            let bbox = resolve_bbox(bbox.as_deref(), &pbf.dataset, &paths)?;
            super::verify_multi_extract::run(&harness, &pbf_path, &bbox, regions, pbf.direct_io)
        }
        VerifyCommand::TagsFilter { pbf } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            super::verify_tags_filter::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::GetidRemoveid { pbf } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            super::verify_getid_removeid::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::AddLocationsToWays { pbf, mode } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            super::verify_add_locations::run(&harness, &pbf_path, mode, pbf.direct_io)
        }
        VerifyCommand::CheckRefs { pbf } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            super::verify_check_refs::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::Merge { pbf, osc_seq } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            let osc_path = match osc_seq.as_deref() {
                Some(seq) => resolve::resolve_osc_path(&pbf.dataset, seq, &paths, project_root)?,
                None => resolve_default_osc_path(&pbf.dataset, &paths, project_root)?,
            };
            let osmosis = match tools::ensure_osmosis(&paths.data_dir, project_root) {
                Ok(tools) => Some(tools),
                Err(e) => {
                    output::verify_msg(&format!("osmosis not available (non-fatal): {e}"));
                    None
                }
            };
            super::verify_merge::run(
                &harness,
                &pbf_path,
                &osc_path,
                osmosis.as_ref(),
                pbf.direct_io,
            )
        }
        VerifyCommand::DeriveChanges { pbf, osc_seq } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            let osc_path = match osc_seq.as_deref() {
                Some(seq) => resolve::resolve_osc_path(&pbf.dataset, seq, &paths, project_root)?,
                None => resolve_default_osc_path(&pbf.dataset, &paths, project_root)?,
            };
            super::verify_derive_changes::run(&harness, &pbf_path, &osc_path, pbf.direct_io)
        }
        VerifyCommand::Renumber {
            dataset,
            variant,
            start_id,
            verbose,
        } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            super::verify_renumber::run(
                &harness,
                &pbf_path,
                &dataset,
                start_id.as_deref(),
                verbose,
            )
        }
        VerifyCommand::Diff {
            dataset,
            variant,
            osc_seq,
        } => {
            let pbf_path = resolve_pbf_path(&dataset, &variant, &paths, project_root)?;
            let osc_path = match osc_seq.as_deref() {
                Some(seq) => resolve::resolve_osc_path(&dataset, seq, &paths, project_root)?,
                None => resolve_default_osc_path(&dataset, &paths, project_root)?,
            };
            super::verify_diff::run(&harness, &pbf_path, &osc_path)
        }
        VerifyCommand::All {
            pbf,
            osc_seq,
            bbox,
        } => {
            let pbf_path = resolve_pbf_path(&pbf.dataset, &pbf.variant, &paths, project_root)?;
            let osc_path = match osc_seq.as_deref() {
                Some(seq) => {
                    resolve::resolve_osc_path(&pbf.dataset, seq, &paths, project_root).ok()
                }
                None => resolve_default_osc_path(&pbf.dataset, &paths, project_root).ok(),
            };
            let bbox_str = resolve_bbox(bbox.as_deref(), &pbf.dataset, &paths).ok();
            super::verify_all::run(
                &harness,
                &pbf_path,
                osc_path.as_deref(),
                bbox_str.as_deref(),
                &paths.data_dir,
                project_root,
                pbf.direct_io,
                &pbf.dataset,
            )
        }
        // Elivagar and nidhogg variants are handled above in cmd_verify().
        VerifyCommand::ElivVerify { .. }
        | VerifyCommand::Batch { .. }
        | VerifyCommand::NidGeocode { .. }
        | VerifyCommand::Readonly { .. } => unreachable!(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn download(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    region: &str,
    osc_seq: Option<u64>,
    as_snapshot: Option<&str>,
    refresh: bool,
    force: bool,
) -> Result<(), DevError> {
    project::require(project, Project::Pbfhogg, "download")?;

    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    super::download::run(
        region,
        osc_seq,
        as_snapshot,
        refresh,
        force,
        &paths.datasets,
        &paths.hostname,
        &paths.data_dir,
        project_root,
    )
}
