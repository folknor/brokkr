use std::path::{Path, PathBuf};

use crate::cli::{VerifyCommand, VerifyInputArgs};
use crate::config;
use crate::context::{MeasurementSession, bootstrap, bootstrap_config};
use crate::datasets::resolve::{self, resolve_bbox, resolve_pbf_with_size};
use crate::error::DevError;
use crate::measure::MeasureRequest;
use crate::output;
use crate::pbfhogg::bench_cells;
use crate::pbfhogg::binary::PbfhoggBinary;
use crate::preflight;
use crate::project::Project;
use crate::tools;

/// Resolve a verify subcommand's input PBF: prefer `--input <PATH>`
/// (canonicalised, validated to exist), then a `--snapshot <key>` if given,
/// finally fall back to primary dataset/variant resolution. Every pbfhogg
/// verify variant selects its PBF through `VerifyInputArgs`, so handcrafted
/// fixtures (`--input`) or registered snapshots (`--snapshot`, e.g. an
/// adversarial encoding from `degrade`/`repack --as-snapshot`) can replace
/// the primary data. `--input` and `--snapshot` are mutually exclusive at the
/// CLI.
fn resolve_verify_input(
    source: &VerifyInputArgs,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    if let Some(p) = source.input.as_deref() {
        return canonical_fixture(p, project_root, "--input");
    }
    // `--snapshot base` / omitted → primary data (identical to the legacy
    // `resolve_pbf_path`); a named key reads the snapshot's pbf table.
    let snapshot_ref = resolve::SnapshotRef::from_opt(source.snapshot.as_ref());
    resolve::resolve_snapshot_pbf_path(
        &source.dataset,
        &snapshot_ref,
        &source.variant,
        paths,
        project_root,
    )
    .map_err(Into::into)
}

/// A handcrafted fixture path (`--input`, `--osc`), relative to the project
/// root. Canonicalised so downstream tools see a consistent absolute path
/// regardless of where brokkr was invoked from; `canonicalize` also surfaces
/// a "no such file" error here rather than letting pbfhogg / osmium fail
/// mid-pipeline.
fn canonical_fixture(p: &Path, project_root: &Path, flag: &str) -> Result<PathBuf, DevError> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        project_root.join(p)
    };
    abs.canonicalize()
        .map_err(|e| DevError::Config(format!("{flag} {p}: {e}", p = p.display())))
}

/// Resolve the `read` benchmark's input PBF and its size, honouring an
/// optional `--snapshot <key>`. `base`/omitted resolves the primary
/// `--variant` (identical to `resolve_pbf_with_size`); a named key reads the
/// snapshot's pbf table so pure decode throughput can be measured against a
/// re-encoded corpus (e.g. a `repack --as-snapshot` at a different blob cap).
fn resolve_read_pbf_with_size(
    req: &MeasureRequest,
    snapshot: Option<&resolve::SnapshotRef>,
    paths: &config::ResolvedPaths,
) -> Result<(PathBuf, f64), DevError> {
    let snapshot_ref = resolve::SnapshotRef::from_opt(snapshot);
    let pbf_path = resolve::resolve_snapshot_pbf_path(
        req.dataset,
        &snapshot_ref,
        req.variant,
        paths,
        req.project_root.as_path(),
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
    snapshot: Option<&resolve::SnapshotRef>,
    osc_seq: Option<&str>,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, OscScope), DevError> {
    let snapshot_ref = resolve::SnapshotRef::from_opt(snapshot);
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

/// Whether `verify all` has an OSC to resolve at all: `--osc-seq` was given,
/// or the dataset (or the named snapshot) registers at least one OSC. `false`
/// is the only case in which the OSC-consuming checks may be skipped; when
/// this is `true`, a resolution failure is an error. An unknown dataset
/// counts as configured so resolution reports it rather than a skip hiding it.
fn osc_configured(
    dataset: &str,
    snapshot: Option<&resolve::SnapshotRef>,
    osc_seq: Option<&str>,
    paths: &config::ResolvedPaths,
) -> bool {
    if osc_seq.is_some() {
        return true;
    }
    let Some(ds) = paths.datasets.get(dataset) else {
        return true;
    };
    !ds.osc.is_empty()
        || snapshot
            .and_then(|key| ds.snapshot.get(key.as_str()))
            .is_some_and(|snap| !snap.osc.is_empty())
}

/// Whether `verify all` has a bbox to resolve: `--bbox`, or the dataset's
/// configured `bbox`. Same skip-versus-error split as [`osc_configured`].
fn bbox_configured(
    bbox: Option<&crate::datasets::Bbox>,
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> bool {
    bbox.is_some()
        || paths
            .datasets
            .get(dataset)
            .is_none_or(|ds| ds.bbox.is_some())
}

/// The base of `verify all`'s `--locations-on-ways` merge: the dataset's (or
/// the named snapshot's) [`KEEP_UNTAGGED_VARIANT`] PBF. Skipped only when it
/// cannot apply - a handcrafted `--input`, or no such variant registered; a
/// registered variant that fails to resolve fails the command, like the OSC.
///
/// [`KEEP_UNTAGGED_VARIANT`]: super::verify_all::KEEP_UNTAGGED_VARIANT
fn resolve_locations_base(
    input: Option<&Path>,
    dataset: &str,
    snapshot: Option<&resolve::SnapshotRef>,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<super::verify_all::LocationsBase, DevError> {
    use super::verify_all::{KEEP_UNTAGGED_VARIANT, LocationsBase};
    if input.is_some() {
        return Ok(LocationsBase::Skipped(
            "SKIPPED (--input given; verify merge --input --osc --locations-on-ways checks a fixture)",
        ));
    }
    let snapshot_ref = resolve::SnapshotRef::from_opt(snapshot);
    let registered = paths
        .datasets
        .get(dataset)
        .is_none_or(|ds| match &snapshot_ref {
            resolve::SnapshotRef::Base => ds.pbf.contains_key(KEEP_UNTAGGED_VARIANT),
            resolve::SnapshotRef::Named(key) => ds
                .snapshot
                .get(key)
                .is_none_or(|snap| snap.pbf.contains_key(KEEP_UNTAGGED_VARIANT)),
        });
    if !registered {
        return Ok(LocationsBase::Skipped(
            "SKIPPED (no altw-keep-untagged variant registered for the dataset)",
        ));
    }
    let path = resolve::resolve_snapshot_pbf_path(
        dataset,
        &snapshot_ref,
        KEEP_UNTAGGED_VARIANT,
        paths,
        project_root,
    )?;
    Ok(LocationsBase::Resolved(path))
}

/// Narrate which OSC chain a verify subcommand resolved, but only when the
/// user actually passed `--snapshot` (the plain base case is unremarkable).
fn narrate_osc_scope(snapshot: Option<&resolve::SnapshotRef>, scope: OscScope) {
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
    snapshot: Option<&resolve::SnapshotRef>,
) -> Result<(), DevError> {
    let cells = bench_cells::read_cells(&bench_cells::parse_read_modes(modes_str)?);
    run_bench_cells(req, "mode", &cells, &[], |paths| {
        let (pbf, mb) = resolve_read_pbf_with_size(req, snapshot, paths)?;
        Ok((pbf, mb, None))
    })
}

pub(crate) fn bench_write(
    req: &MeasureRequest,
    compressions: &super::CompressionList,
) -> Result<(), DevError> {
    let compressions = super::compression_args(compressions.as_slice(), true);
    let cells = bench_cells::write_cells(&compressions);
    run_bench_cells(req, "variant", &cells, &[], |paths| {
        let (pbf, mb) =
            resolve_pbf_with_size(req.dataset, req.variant, paths, req.project_root.as_path())?;
        Ok((pbf, mb, None))
    })
}

/// With `uring` every merge cell, buffered ones included, runs on the one
/// build with `linux-io-uring`, so the I/O mode is the only thing a pair of
/// cells varies.
pub(crate) fn bench_merge(
    req: &MeasureRequest,
    osc_seq: Option<&str>,
    uring: bool,
    compressions: &super::CompressionList,
) -> Result<(), DevError> {
    if uring {
        preflight::run_preflight(&preflight::uring_checks())?;
    }
    let compressions = super::compression_args(compressions.as_slice(), false);
    let cells = bench_cells::merge_cells(&compressions, uring);
    let features: &[&str] = if uring { &["linux-io-uring"] } else { &[] };
    run_bench_cells(req, "variant", &cells, features, |paths| {
        let root = req.project_root.as_path();
        let (pbf, mb) = resolve_pbf_with_size(req.dataset, req.variant, paths, root)?;
        let osc = resolve_bench_osc(req.dataset, osc_seq, paths, root)?;
        Ok((pbf, mb, Some(osc)))
    })
}

/// The input a bench batch measures: its PBF, that PBF's size in MB, and the
/// OSC a merge batch applies.
type BatchInput = (PathBuf, f64, Option<PathBuf>);

/// Resolve, build and measure one `read`, `write` or `merge` batch. Dry run
/// resolves the same input and prints every cell's argv. A real run builds
/// once with the request's features (the profiled mode's instrumentation
/// included) plus `extra_features`, resolves the input after the session is
/// admitted, and measures each cell through the shared executor.
fn run_bench_cells(
    req: &MeasureRequest,
    label: &str,
    cells: &[bench_cells::BenchCell],
    extra_features: &[&str],
    resolve: impl Fn(&config::ResolvedPaths) -> Result<BatchInput, DevError>,
) -> Result<(), DevError> {
    if req.dry_run {
        let paths = dry_run_paths(req)?;
        let (pbf, _mb, osc) = resolve(&paths)?;
        let inputs = bench_cells::BatchInputs::new(&pbf, osc.as_deref(), &paths.scratch_dir);
        for cell in cells {
            output::dry_run_msg(&format!(
                "{} {}: {}",
                cell.command(),
                cell.name(),
                cell.argv(&inputs)?.args().join(" ")
            ));
        }
        output::dry_run_msg("ok");
        return Ok(());
    }

    let mut features = req.build_features();
    features.extend_from_slice(extra_features);
    let ctx = MeasurementSession::build_for_request(
        req,
        Project::Pbfhogg.cli_package(),
        &features,
        true,
    )?;
    let (pbf, file_mb, osc) = resolve(&ctx.paths)?;
    let inputs = bench_cells::BatchInputs::new(&pbf, osc.as_deref(), &ctx.paths.scratch_dir);
    let binary = PbfhoggBinary::from_session(&ctx, req.project_root.as_path())?;
    bench_cells::run_batch(
        &bench_cells::Batch {
            mode: &req.mode,
            harness: &ctx.harness,
            handle: &binary,
            label,
            file_mb,
            runs: req.runs(),
            cwd: req.project_root.as_path(),
        },
        cells,
        &inputs,
    )
}

pub(crate) fn bench_all(req: &MeasureRequest) -> Result<(), DevError> {
    if req.dry_run {
        let paths = dry_run_paths(req)?;
        let _ =
            resolve_pbf_with_size(req.dataset, req.variant, &paths, req.project_root.as_path())?;
        output::dry_run_msg("ok (bench all)");
        return Ok(());
    }

    let ctx = MeasurementSession::new_for_request(req)?;
    let (pbf_path, file_mb) = resolve_pbf_with_size(
        req.dataset,
        req.variant,
        &ctx.paths,
        req.project_root.as_path(),
    )?;
    super::bench_all::run(
        &req.mode,
        &ctx.harness,
        &ctx.paths,
        req.build_root,
        &pbf_path,
        file_mb,
        req.runs(),
        req.dataset,
        req.variant,
    )
}

fn resolve_bench_osc(
    dataset: &str,
    osc_seq: Option<&str>,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let (path, _seq) = resolve::resolve_single_osc(
        dataset,
        &resolve::SnapshotRef::Base,
        osc_seq,
        paths,
        project_root,
    )?;
    Ok(path)
}

/// Resolve host paths without building the binary, for dry-run validation.
fn dry_run_paths(req: &MeasureRequest) -> Result<config::ResolvedPaths, DevError> {
    let pi = bootstrap(req.build_root)?;
    bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn verify(
    dev_config: &config::DevConfig,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
    verify: VerifyCommand,
    features: &[String],
    verbose: bool,
) -> Result<(), DevError> {
    // Refused before the harness builds pbfhogg, not after.
    if matches!(
        verify,
        VerifyCommand::ElivVerify { .. }
            | VerifyCommand::Batch { .. }
            | VerifyCommand::NidGeocode { .. }
            | VerifyCommand::Readonly { .. }
    ) {
        return Err(not_a_pbfhogg_verify());
    }
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let (project_root, build_root) = (project_root.as_path(), build_root.as_path());

    let harness =
        super::verify::VerifyHarness::new(project_root, &pi.target_dir, build_root, features)?;

    // Every exit of the run below, an error or an interruption included,
    // reaches the reference cache's retention while `harness` and its lock
    // guard are still alive; a retention failure is a warning, and the run's
    // own result is what returns.
    let result = (|| -> Result<(), DevError> {
        // `all` runs the whole suite and prints its own per-check results + banner,
        // so it manages the quiet/verbose behaviour itself rather than being
        // wrapped in a single `run_check`.
        if let VerifyCommand::All { pbf, osc_seq, bbox } = &verify {
            let direct_io = pbf.direct_io;
            let pbf = &pbf.source;
            let pbf_path = resolve_verify_input(pbf, &paths, project_root)?;
            // An input that is simply not configured skips the checks needing it;
            // an input that IS configured but fails to resolve - hash mismatch,
            // missing file, ambiguous OSC, malformed bbox - fails the command.
            // Resolving with `.ok()` used to fold both into "SKIPPED", so a
            // corrupted OSC turned five checks into skips and the suite exited 0.
            let osc_path = if osc_configured(
                &pbf.dataset,
                pbf.snapshot.as_ref(),
                osc_seq.as_deref(),
                &paths,
            ) {
                let (path, scope) = resolve_verify_osc(
                    &pbf.dataset,
                    pbf.snapshot.as_ref(),
                    osc_seq.as_deref(),
                    &paths,
                    project_root,
                )?;
                narrate_osc_scope(pbf.snapshot.as_ref(), scope);
                Some(path)
            } else {
                None
            };
            let resolved_bbox = if bbox_configured(bbox.as_ref(), &pbf.dataset, &paths) {
                Some(resolve_bbox(bbox.as_ref(), &pbf.dataset, &paths)?)
            } else {
                None
            };
            let locations_base = resolve_locations_base(
                pbf.input.as_deref(),
                &pbf.dataset,
                pbf.snapshot.as_ref(),
                &paths,
                project_root,
            )?;
            return super::verify_all::run(
                &harness,
                &pbf_path,
                osc_path.as_deref(),
                resolved_bbox.as_ref(),
                &locations_base,
                &paths.data_dir,
                direct_io,
                &pbf.dataset,
                verbose,
            );
        }

        // Enriched (injected-prepass) output is osmium-incompatible by design:
        // BlobHeader field-5 headers run ~1-8 KB and libosmium 2.23 rejects any
        // BlobHeader over 127 bytes (their issue 405). There is no reference tool
        // to cross-validate against, so we refuse the flag up-front rather than let
        // the check report a false failure. Enriched-output correctness is covered
        // inside pbfhogg's own suite (oracle roundtrip + backend parity).
        if let VerifyCommand::AddLocationsToWays {
            inject_prepass: true,
            ..
        } = &verify
        {
            output::verify_msg(
                "add-locations-to-ways --inject-prepass emits osmium-incompatible wire \
             extensions (BlobHeader field 5 > 127 bytes; libosmium 2.23 issue 405); \
             enriched output cannot be osmium-verified.",
            );
            output::verify_msg(
                "flag-off `brokkr verify add-locations-to-ways` covers the element \
             semantics; enriched correctness is covered by pbfhogg's own oracle \
             roundtrip + backend-parity suite.",
            );
            return Err(DevError::Config(
                "verify add-locations-to-ways --inject-prepass: enriched output cannot be \
             osmium-verified"
                    .into(),
            ));
        }

        // Every individual check runs under the pass-quiet / fail-loud wrapper.
        let name = verify_name(&verify);
        super::verify::run_check(name, verbose, || match verify {
            VerifyCommand::Sort { pbf } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                super::verify_sort::run(&harness, &pbf_path, pbf.direct_io)
            }
            VerifyCommand::Cat { pbf } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                super::verify_cat::run(&harness, &pbf_path, pbf.direct_io)
            }
            VerifyCommand::Extract { pbf, bbox } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                let bbox = resolve_bbox(bbox.as_ref(), &pbf.source.dataset, &paths)?;
                super::verify_extract::run(&harness, &pbf_path, &bbox, pbf.direct_io)
            }
            VerifyCommand::MultiExtract { pbf, bbox, regions } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                let bbox = resolve_bbox(bbox.as_ref(), &pbf.source.dataset, &paths)?;
                super::verify_multi_extract::run(&harness, &pbf_path, &bbox, regions, pbf.direct_io)
            }
            VerifyCommand::TagsFilter { pbf } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                super::verify_tags_filter::run(&harness, &pbf_path, pbf.direct_io)
            }
            VerifyCommand::GetidRemoveid { pbf } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                super::verify_getid_removeid::run(&harness, &pbf_path, pbf.direct_io)
            }
            VerifyCommand::AddLocationsToWays {
                pbf,
                mode,
                inject_prepass: _,
            } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                super::verify_add_locations::run(&harness, &pbf_path, mode, pbf.direct_io)
            }
            VerifyCommand::CheckRefs { pbf } => {
                let pbf_path = resolve_verify_input(&pbf, &paths, project_root)?;
                super::verify_check_refs::run(&harness, &pbf_path)
            }
            VerifyCommand::Merge {
                pbf,
                osc_seq,
                osc,
                locations_on_ways,
            } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                let osc_path = if let Some(p) = osc.as_deref() {
                    canonical_fixture(p, project_root, "--osc")?
                } else {
                    let (path, scope) = resolve_verify_osc(
                        &pbf.source.dataset,
                        pbf.source.snapshot.as_ref(),
                        osc_seq.as_deref(),
                        &paths,
                        project_root,
                    )?;
                    narrate_osc_scope(pbf.source.snapshot.as_ref(), scope);
                    path
                };
                // osmosis and osmconvert write no inline locations, so an -l run
                // has nothing to ask them.
                let osmosis = if locations_on_ways {
                    None
                } else {
                    match tools::ensure_osmosis(&paths.data_dir) {
                        Ok(tools) => Some(tools),
                        Err(DevError::Interrupted) => return Err(DevError::Interrupted),
                        Err(e) => {
                            output::verify_msg(&format!("osmosis not available (non-fatal): {e}"));
                            None
                        }
                    }
                };
                super::verify_merge::run(
                    &harness,
                    &pbf_path,
                    &osc_path,
                    osmosis.as_ref(),
                    pbf.direct_io,
                    locations_on_ways,
                )
            }
            VerifyCommand::DeriveChanges { pbf, osc_seq } => {
                let pbf_path = resolve_verify_input(&pbf.source, &paths, project_root)?;
                let (osc_path, scope) = resolve_verify_osc(
                    &pbf.source.dataset,
                    pbf.source.snapshot.as_ref(),
                    osc_seq.as_deref(),
                    &paths,
                    project_root,
                )?;
                narrate_osc_scope(pbf.source.snapshot.as_ref(), scope);
                super::verify_derive_changes::run(&harness, &pbf_path, &osc_path, pbf.direct_io)
            }
            VerifyCommand::Renumber {
                pbf,
                start_id,
                verbose,
            } => {
                let pbf_path = resolve_verify_input(&pbf, &paths, project_root)?;
                super::verify_renumber::run(
                    &harness,
                    &pbf_path,
                    &pbf.dataset,
                    start_id.as_deref(),
                    verbose,
                )
            }
            VerifyCommand::Diff { pbf, osc_seq } => {
                let pbf_path = resolve_verify_input(&pbf, &paths, project_root)?;
                let (osc_path, scope) = resolve_verify_osc(
                    &pbf.dataset,
                    pbf.snapshot.as_ref(),
                    osc_seq.as_deref(),
                    &paths,
                    project_root,
                )?;
                narrate_osc_scope(pbf.snapshot.as_ref(), scope);
                super::verify_diff::run(&harness, &pbf_path, &osc_path)
            }
            // Both returned before this match: `all` (it manages its own output)
            // and the other projects' variants (refused at the top). Errors, not
            // `unreachable!()`: those guards are ordering this match cannot see,
            // and a reordering should fail the command, not panic the process.
            VerifyCommand::All { .. } => Err(DevError::Internal(
                "verify all reached the single-check path".into(),
            )),
            VerifyCommand::ElivVerify { .. }
            | VerifyCommand::Batch { .. }
            | VerifyCommand::NidGeocode { .. }
            | VerifyCommand::Readonly { .. } => Err(not_a_pbfhogg_verify()),
        })
    })();
    harness.retain_references(dev_config.verify_reference_budget(&paths.hostname));
    result
}

/// The refusal for an elivagar/nidhogg `verify` subcommand reaching the
/// pbfhogg verifier. `cmd_verify` routes those by project first; this is the
/// answer if one ever arrives here anyway.
fn not_a_pbfhogg_verify() -> DevError {
    DevError::Internal("a non-pbfhogg verify subcommand reached the pbfhogg verifier".into())
}

/// The result-line name for a pbfhogg verify subcommand (matches the labels
/// used by `verify all`).
fn verify_name(verify: &VerifyCommand) -> &'static str {
    match verify {
        VerifyCommand::Sort { .. } => super::verify::labels::SORT,
        VerifyCommand::Cat { .. } => super::verify::labels::CAT,
        VerifyCommand::Extract { .. } => super::verify::labels::EXTRACT,
        VerifyCommand::MultiExtract { .. } => super::verify::labels::MULTI_EXTRACT,
        VerifyCommand::TagsFilter { .. } => super::verify::labels::TAGS_FILTER,
        VerifyCommand::GetidRemoveid { .. } => super::verify::labels::GETID_REMOVEID,
        VerifyCommand::AddLocationsToWays { .. } => super::verify::labels::ADD_LOCATIONS_TO_WAYS,
        VerifyCommand::CheckRefs { .. } => super::verify::labels::CHECK_REFS,
        VerifyCommand::Merge { .. } => super::verify::labels::APPLY_CHANGES,
        VerifyCommand::DeriveChanges { .. } => super::verify::labels::DIFF_OSC,
        VerifyCommand::Renumber { .. } => super::verify::labels::RENUMBER,
        VerifyCommand::Diff { .. } => super::verify::labels::DIFF,
        VerifyCommand::All { .. } => super::verify::labels::ALL,
        VerifyCommand::ElivVerify { .. }
        | VerifyCommand::Batch { .. }
        | VerifyCommand::NidGeocode { .. }
        | VerifyCommand::Readonly { .. } => "verify",
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn download(
    dev_config: &config::DevConfig,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
    region: &str,
    osc_seq: Option<u64>,
    as_snapshot: Option<&str>,
    refresh: bool,
    force: bool,
) -> Result<(), DevError> {
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let (project_root, build_root) = (project_root.as_path(), build_root.as_path());

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
        build_root,
    )
}
