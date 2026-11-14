use std::path::{Path, PathBuf};

use crate::config;
use crate::context::{MeasurementSession, bootstrap_config};
use crate::datasets::resolve::{self, resolve_pbf_with_size};
use crate::error::DevError;
use crate::measure::MeasureRequest;
use crate::output;
use crate::pbfhogg::bench_cells;
use crate::pbfhogg::binary::PbfhoggBinary;
use crate::preflight;
use crate::project::Project;

/// Resolve the `read` benchmark's input PBF and its size, honouring an
/// optional `--snapshot <key>`. `base`/omitted resolves the primary
/// `--variant` (identical to `resolve_pbf_with_size`); a named key reads the
/// snapshot's pbf table so pure decode throughput can be measured against a
/// re-encoded corpus, such as a repack at a different blob cap.
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
        output::dry_run_msg("ok (suite pbfhogg)");
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
    bootstrap_config(req.dev_config, req.project_root)
}
