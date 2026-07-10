use std::path::{Path, PathBuf};

use crate::cli::VerifyCommand;
use crate::config;
use crate::context::{BenchContext, HarnessContext, bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::measure::MeasureRequest;
use crate::output;
use crate::preflight;
use crate::project::{self, Project};
use crate::resolve::{
    self, resolve_bbox, resolve_default_osc_path, resolve_pbf_with_size,
};
use crate::tools;

/// Resolve a verify subcommand's input PBF: prefer `--input <PATH>`
/// (canonicalised, validated to exist), then a `--snapshot <key>` if given,
/// finally fall back to primary dataset/variant resolution. Used by every
/// verify variant that takes `VerifyPbfArgs` so handcrafted fixtures
/// (`--input`) or registered snapshots (`--snapshot`, e.g. an adversarial
/// encoding from `degrade`/`repack --as-snapshot`) can replace the primary
/// data. `--input` and `--snapshot` are mutually exclusive at the CLI.
fn resolve_verify_input(
    input: Option<&Path>,
    dataset: &str,
    variant: &str,
    snapshot: Option<&str>,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    if let Some(p) = input {
        // Canonicalise so downstream tools see a consistent absolute
        // path regardless of where brokkr was invoked from. `canonicalize`
        // also surfaces a "no such file" error here rather than letting
        // pbfhogg / osmium fail mid-pipeline.
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            project_root.join(p)
        };
        let canonical = abs.canonicalize().map_err(|e| {
            DevError::Config(format!(
                "--input {p}: {e}",
                p = p.display()
            ))
        })?;
        return Ok(canonical);
    }
    // `--snapshot base` / omitted → primary data (identical to the legacy
    // `resolve_pbf_path`); a named key reads the snapshot's pbf table.
    let snapshot_ref = resolve::SnapshotRef::from_opt(snapshot)?;
    resolve::resolve_snapshot_pbf_path(dataset, &snapshot_ref, variant, paths, project_root)
}

/// Resolve the `read` benchmark's input PBF and its size, honouring an
/// optional `--snapshot <key>`. `base`/omitted resolves the primary
/// `--variant` (identical to `resolve_pbf_with_size`); a named key reads the
/// snapshot's pbf table so pure decode throughput can be measured against a
/// re-encoded corpus (e.g. a `repack --as-snapshot` at a different blob cap).
fn resolve_read_pbf_with_size(
    req: &MeasureRequest,
    snapshot: Option<&str>,
    paths: &config::ResolvedPaths,
) -> Result<(PathBuf, f64), DevError> {
    let snapshot_ref = resolve::SnapshotRef::from_opt(snapshot)?;
    let pbf_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        &snapshot_ref,
        req.variant,
        paths,
        req.project_root,
    )?;
    let file_mb = resolve::file_size_mb(&pbf_path)?;
    Ok((pbf_path, file_mb))
}

/// Which OSC chain a verify subcommand resolved against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OscScope {
    /// The snapshot's own `osc` table (a point-in-time snapshot with changes).
    Snapshot,
    /// The dataset's primary/legacy chain (no `--snapshot`, or the named
    /// snapshot carries no OSC table - e.g. an encoding-only degrade/repack).
    Base,
}

/// Resolve the OSC chain for a verify subcommand that consumes changes
/// (`merge` / `derive-changes` / `diff`).
///
/// Prefers the snapshot's own OSC table when a named `--snapshot` carries one
/// (point-in-time snapshots), and falls back to the dataset's primary chain
/// otherwise. Encoding-only snapshots (degrade/repack) have no OSC table, so
/// they fall through to base - the logically-correct diff stream, since they
/// are same-sequence re-encodings of the base PBF. Returns the resolved path
/// and which chain it came from (for log narration).
fn resolve_verify_osc(
    dataset: &str,
    snapshot: Option<&str>,
    osc_seq: Option<&str>,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, OscScope), DevError> {
    let snapshot_ref = resolve::SnapshotRef::from_opt(snapshot)?;
    let use_snapshot = match &snapshot_ref {
        resolve::SnapshotRef::Base => false,
        resolve::SnapshotRef::Named(key) => paths
            .datasets
            .get(dataset)
            .and_then(|ds| ds.snapshot.get(key))
            .is_some_and(|snap| !snap.osc.is_empty()),
    };
    let (scope_ref, scope) = if use_snapshot {
        (snapshot_ref, OscScope::Snapshot)
    } else {
        (resolve::SnapshotRef::Base, OscScope::Base)
    };
    let (path, _seq) =
        resolve::resolve_single_osc(dataset, &scope_ref, osc_seq, paths, project_root)?;
    Ok((path, scope))
}

/// Narrate which OSC chain a verify subcommand resolved, but only when the
/// user actually passed `--snapshot` (the plain base case is unremarkable).
fn narrate_osc_scope(snapshot: Option<&str>, scope: OscScope) {
    let Some(key) = snapshot else {
        return;
    };
    match scope {
        OscScope::Snapshot => output::verify_msg(&format!("osc: snapshot-scoped ({key})")),
        OscScope::Base => {
            output::verify_msg(&format!("osc: base fallback ({key} has no osc table)"));
        }
    }
}

pub(crate) fn bench_read(
    req: &MeasureRequest,
    modes_str: &str,
    snapshot: Option<&str>,
) -> Result<(), DevError> {
    if req.dry_run {
        let paths = dry_run_paths(req)?;
        let _ = resolve_read_pbf_with_size(req, snapshot, &paths)?;
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
    let (pbf_path, file_mb) = resolve_read_pbf_with_size(req, snapshot, &ctx.paths)?;
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
    verbose: bool,
) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let harness =
        super::verify::VerifyHarness::new(project_root, &pi.target_dir, build_root, features)?;

    // `all` runs the whole suite and prints its own per-check results + banner,
    // so it manages the quiet/verbose behaviour itself rather than being
    // wrapped in a single `run_check`.
    if let VerifyCommand::All { pbf, osc_seq, bbox } = &verify {
        let pbf_path = resolve_verify_input(
            pbf.input.as_deref(),
            &pbf.dataset,
            &pbf.variant,
            pbf.snapshot.as_deref(),
            &paths,
            project_root,
        )?;
        let osc_resolved = resolve_verify_osc(
            &pbf.dataset,
            pbf.snapshot.as_deref(),
            osc_seq.as_deref(),
            &paths,
            project_root,
        )
        .ok();
        if let Some((_, scope)) = &osc_resolved {
            narrate_osc_scope(pbf.snapshot.as_deref(), *scope);
        }
        let osc_path = osc_resolved.map(|(p, _)| p);
        let bbox_str = resolve_bbox(bbox.as_deref(), &pbf.dataset, &paths).ok();
        return super::verify_all::run(
            &harness,
            &pbf_path,
            osc_path.as_deref(),
            bbox_str.as_deref(),
            &paths.data_dir,
            project_root,
            pbf.direct_io,
            &pbf.dataset,
            verbose,
        );
    }

    // Every individual check runs under the pass-quiet / fail-loud wrapper.
    let name = verify_name(&verify);
    super::verify::run_check(name, verbose, || match verify {
        VerifyCommand::Sort { pbf } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            super::verify_sort::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::Cat { pbf } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            super::verify_cat::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::Extract { pbf, bbox } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            let bbox = resolve_bbox(bbox.as_deref(), &pbf.dataset, &paths)?;
            super::verify_extract::run(&harness, &pbf_path, &bbox, pbf.direct_io)
        }
        VerifyCommand::MultiExtract {
            pbf,
            bbox,
            regions,
        } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            let bbox = resolve_bbox(bbox.as_deref(), &pbf.dataset, &paths)?;
            super::verify_multi_extract::run(&harness, &pbf_path, &bbox, regions, pbf.direct_io)
        }
        VerifyCommand::TagsFilter { pbf } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            super::verify_tags_filter::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::GetidRemoveid { pbf } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            super::verify_getid_removeid::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::AddLocationsToWays { pbf, mode } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            super::verify_add_locations::run(&harness, &pbf_path, mode, pbf.direct_io)
        }
        VerifyCommand::CheckRefs { pbf } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            super::verify_check_refs::run(&harness, &pbf_path, pbf.direct_io)
        }
        VerifyCommand::Merge { pbf, osc_seq } => {
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            let (osc_path, scope) = resolve_verify_osc(
                &pbf.dataset,
                pbf.snapshot.as_deref(),
                osc_seq.as_deref(),
                &paths,
                project_root,
            )?;
            narrate_osc_scope(pbf.snapshot.as_deref(), scope);
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
            let pbf_path = resolve_verify_input(
                pbf.input.as_deref(),
                &pbf.dataset,
                &pbf.variant,
                pbf.snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            let (osc_path, scope) = resolve_verify_osc(
                &pbf.dataset,
                pbf.snapshot.as_deref(),
                osc_seq.as_deref(),
                &paths,
                project_root,
            )?;
            narrate_osc_scope(pbf.snapshot.as_deref(), scope);
            super::verify_derive_changes::run(&harness, &pbf_path, &osc_path, pbf.direct_io)
        }
        VerifyCommand::Renumber {
            dataset,
            variant,
            input,
            snapshot,
            start_id,
            verbose,
        } => {
            let pbf_path = resolve_verify_input(
                input.as_deref(),
                &dataset,
                &variant,
                snapshot.as_deref(),
                &paths,
                project_root,
            )?;
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
            input,
            snapshot,
            osc_seq,
        } => {
            let pbf_path = resolve_verify_input(
                input.as_deref(),
                &dataset,
                &variant,
                snapshot.as_deref(),
                &paths,
                project_root,
            )?;
            let (osc_path, scope) = resolve_verify_osc(
                &dataset,
                snapshot.as_deref(),
                osc_seq.as_deref(),
                &paths,
                project_root,
            )?;
            narrate_osc_scope(snapshot.as_deref(), scope);
            super::verify_diff::run(&harness, &pbf_path, &osc_path)
        }
        // `all` is handled before this match (it manages its own output).
        VerifyCommand::All { .. } => unreachable!("verify all handled above"),
        // Elivagar and nidhogg variants are handled above in cmd_verify().
        VerifyCommand::ElivVerify { .. }
        | VerifyCommand::Batch { .. }
        | VerifyCommand::NidGeocode { .. }
        | VerifyCommand::Readonly { .. } => unreachable!(),
    })
}

/// The result-line name for a pbfhogg verify subcommand (matches the labels
/// used by `verify all`).
fn verify_name(verify: &VerifyCommand) -> &'static str {
    match verify {
        VerifyCommand::Sort { .. } => "sort",
        VerifyCommand::Cat { .. } => "cat",
        VerifyCommand::Extract { .. } => "extract",
        VerifyCommand::MultiExtract { .. } => "multi-extract",
        VerifyCommand::TagsFilter { .. } => "tags-filter",
        VerifyCommand::GetidRemoveid { .. } => "getid-removeid",
        VerifyCommand::AddLocationsToWays { .. } => "add-locations-to-ways",
        VerifyCommand::CheckRefs { .. } => "check-refs",
        VerifyCommand::Merge { .. } => "apply-changes",
        VerifyCommand::DeriveChanges { .. } => "diff --format osc",
        VerifyCommand::Renumber { .. } => "renumber",
        VerifyCommand::Diff { .. } => "diff",
        VerifyCommand::All { .. } => "all",
        VerifyCommand::ElivVerify { .. }
        | VerifyCommand::Batch { .. }
        | VerifyCommand::NidGeocode { .. }
        | VerifyCommand::Readonly { .. } => "verify",
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
