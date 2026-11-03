//! Combined benchmark suite: runs all pbfhogg benchmarks plus external baselines.

use std::path::Path;

use crate::build;
use crate::clean::layout::PbfhoggLayout;
use crate::config::ResolvedPaths;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::measure::MeasureMode;
use crate::measure::{ExternalTool, Invocation};
use crate::output;
use crate::pbfhogg::bench_cells::{self, Batch, BatchInputs};
use crate::pbfhogg::binary::PbfhoggBinary;
use crate::pbfhogg::context::Region;
use crate::pbfhogg::{bench_blob_filter, bench_commands, bench_extract, bench_planetiler};
use crate::project::BuildRoot;
use crate::tools;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the full benchmark suite: read, write, merge, commands, plus external baselines.
///
/// External baselines (osmpbf, osmium, planetiler) are best-effort -- errors
/// skip the baseline without failing the whole run.
#[allow(clippy::too_many_arguments)]
pub fn run(
    mode: &MeasureMode,
    harness: &BenchHarness,
    paths: &ResolvedPaths,
    build_root: &BuildRoot,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    dataset: &str,
    variant: &str,
) -> Result<(), DevError> {
    let project_root = build_root.as_path();
    // No build may start while a pbfhogg handle is alive, and the osmpbf
    // baseline builds, so the suite handle's scope closes first.
    {
        let (binary, identity) = PbfhoggBinary::build(
            &build::BuildConfig::release_with_owned_features(
                crate::project::Project::Pbfhogg.cli_package(),
                &paths.features,
            ),
            project_root,
            project_root,
        )?;
        harness.note_build(identity);
        run_suite_binary(
            mode,
            harness,
            paths,
            dataset,
            variant,
            &binary,
            pbf_path,
            file_mb,
            runs,
            project_root,
        )?;
    }

    run_baselines(harness, paths, build_root, pbf_path, file_mb, runs)
}

/// Every suite benchmark that runs the one suite build.
#[allow(clippy::too_many_arguments)]
fn run_suite_binary(
    mode: &MeasureMode,
    harness: &BenchHarness,
    paths: &ResolvedPaths,
    dataset: &str,
    variant: &str,
    binary: &PbfhoggBinary,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    project_root: &Path,
) -> Result<(), DevError> {
    // 1. command presets; extract runs later on the dataset's bbox and polygons.
    output::bench_msg("=== bench commands ===");
    // Resolve a configured OSC through the verified resolver. Ambiguous
    // entries and stale hashes fail the suite instead of looking like an
    // optional dataset field that was never configured.
    let default_osc = match paths.datasets.get(dataset) {
        Some(ds) if !ds.osc.is_empty() => Some(crate::datasets::resolve::resolve_single_osc(
            dataset,
            &crate::datasets::resolve::SnapshotRef::Base,
            None,
            paths,
            project_root,
        )?),
        _ => None,
    };
    bench_commands::run(
        harness,
        binary,
        pbf_path,
        default_osc.as_ref().map(|(path, _)| path.as_path()),
        default_osc.as_ref().map(|(_, seq)| seq.as_str()),
        &paths.scratch_dir,
        runs,
        bench_commands::ALL_COMMANDS,
        &crate::pbfhogg::context::BenchSelection::for_dataset(paths.datasets.get(dataset)),
        project_root,
        // The answers the primary entry the suite measures declares.
        paths
            .datasets
            .get(dataset)
            .and_then(|ds| ds.pbf.get(variant))
            .map(|entry| bench_commands::SuiteExpect {
                expect: &entry.expect,
                table: crate::pbfhogg::expect::entry_table(
                    dataset,
                    &crate::datasets::resolve::SnapshotRef::Base,
                    variant,
                ),
            })
            .as_ref(),
    )?;

    // The read, write and merge cells run under this suite's one handle,
    // through the same constructors and executor as `brokkr read`, `write`
    // and `merge`.
    let batch = |label| Batch {
        mode,
        harness,
        handle: binary,
        label,
        file_mb,
        runs,
        cwd: project_root,
    };

    // 2. bench read -- every read mode
    output::bench_msg("=== bench read ===");
    bench_cells::run_batch(
        &batch("mode"),
        &bench_cells::read_cells(bench_cells::ALL_READ_MODES),
        &BatchInputs::new(pbf_path, None, &paths.scratch_dir),
    )?;

    // 3. bench write -- sync + pipelined x default compressions
    output::bench_msg("=== bench write ===");
    let write_compressions =
        super::parse_compressions(super::DEFAULT_BENCH_WRITE_COMPRESSIONS, false)?;
    bench_cells::run_batch(
        &batch("variant"),
        &bench_cells::write_cells(&write_compressions),
        &BatchInputs::new(pbf_path, None, &paths.scratch_dir),
    )?;

    run_dataset_dependent(
        mode,
        harness,
        paths,
        dataset,
        binary,
        pbf_path,
        file_mb,
        runs,
        project_root,
        default_osc
            .as_ref()
            .map(|(path, seq)| (path.as_path(), seq.as_str())),
    )
}

/// Run benchmarks that depend on optional dataset config fields (osc, bbox,
/// polygon, pbf_raw).
#[allow(clippy::too_many_arguments)]
fn run_dataset_dependent(
    mode: &MeasureMode,
    harness: &BenchHarness,
    paths: &ResolvedPaths,
    dataset: &str,
    binary: &PbfhoggBinary,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    project_root: &Path,
    default_osc: Option<(&Path, &str)>,
) -> Result<(), DevError> {
    let ds = paths.datasets.get(dataset);

    // bench merge -- if osc is available
    let osc_path = default_osc.map(|(path, _)| path);

    if let Some(osc_path) = osc_path {
        output::bench_msg("=== bench merge ===");
        let merge_compressions =
            super::parse_compressions(super::DEFAULT_BENCH_MERGE_COMPRESSIONS, false)?;
        bench_cells::run_batch(
            &Batch {
                mode,
                harness,
                handle: binary,
                label: "variant",
                file_mb,
                runs,
                cwd: project_root,
            },
            &bench_cells::merge_cells(&merge_compressions, false),
            &BatchInputs::new(pbf_path, Some(osc_path), &paths.scratch_dir),
        )?;
    } else {
        output::bench_msg("=== bench merge === (skipped, no osc file)");
    }

    // bench extract -- on the dataset bbox, then on each dataset polygon
    // (ordered by name); a polygon that is configured but fails to resolve
    // fails the suite rather than skipping its cells.
    let mut regions: Vec<Region> = ds
        .and_then(|dataset| dataset.bbox.clone())
        .into_iter()
        .map(Region::Bbox)
        .collect();
    if ds.is_some() {
        regions.extend(
            crate::datasets::resolve::resolve_polygons(dataset, paths, project_root)?
                .into_iter()
                .map(Region::Polygon),
        );
    }

    if regions.is_empty() {
        output::bench_msg("=== bench extract === (skipped, no bbox or polygon in dataset config)");
    }
    for region in &regions {
        match region {
            Region::Bbox(_) => output::bench_msg("=== bench extract ==="),
            Region::Polygon(polygon) => {
                output::bench_msg(&format!("=== bench extract (polygon {}) ===", polygon.name));
            }
        }
        bench_extract::run(
            harness,
            binary,
            pbf_path,
            file_mb,
            runs,
            region,
            project_root,
            &paths.scratch_dir,
        )?;
    }

    // bench blob-filter -- if raw PBF is available
    let raw_pbf_path = if ds.is_some_and(|dataset| dataset.pbf.contains_key("raw")) {
        Some(crate::datasets::resolve::resolve_pbf_path(
            dataset,
            "raw",
            paths,
            project_root,
        )?)
    } else {
        None
    };

    if let Some(ref raw_path) = raw_pbf_path {
        output::bench_msg("=== bench blob-filter ===");
        bench_blob_filter::run(
            harness,
            binary,
            pbf_path,
            raw_path,
            file_mb,
            runs,
            project_root,
            &paths.scratch_dir,
        )?;
    } else {
        output::bench_msg(
            "=== bench blob-filter === (skipped, no raw pbf variant in dataset config)",
        );
    }

    Ok(())
}

/// Run external baselines: osmpbf, osmium, planetiler.
///
/// Errors from individual baselines are logged and skipped without failing the suite.
#[allow(clippy::too_many_arguments)]
fn run_baselines(
    harness: &BenchHarness,
    paths: &ResolvedPaths,
    build_root: &BuildRoot,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
) -> Result<(), DevError> {
    let project_root = build_root.as_path();
    // osmpbf baseline -- build and run
    // Same shape as the suite's other optional sections: a header only when
    // the section runs, a one-line reason when it does not.
    if project_root.join(tools::OSMPBF_BASELINE_MANIFEST).exists() {
        output::bench_msg("=== osmpbf baseline ===");
        skip_on_error(
            "osmpbf baseline",
            run_osmpbf_baseline(harness, pbf_path, file_mb, runs, build_root),
        )?;
    } else {
        output::bench_msg("=== osmpbf baseline === (skipped, no bench/osmpbf-baseline project)");
    }

    // An installed osmium is not a build of this session: its rows record no
    // provenance rather than the osmpbf baseline's.
    harness.forget_build();

    // osmium -- if available
    if super::verify::which_exists("osmium") {
        output::bench_msg("=== osmium baseline ===");
        skip_on_error(
            "osmium baseline",
            run_osmium_baseline(
                harness,
                pbf_path,
                file_mb,
                runs,
                &paths.scratch_dir,
                project_root,
            ),
        )?;
    } else {
        output::bench_msg("=== osmium baseline === (skipped, osmium not found)");
    }

    // planetiler -- always attempted (the tool is fetched on demand), so the
    // header always prints; a failure to obtain or run it is a warning from
    // skip_on_error.
    output::bench_msg("=== planetiler baseline ===");
    skip_on_error(
        "planetiler baseline",
        bench_planetiler::run(
            harness,
            pbf_path,
            file_mb,
            runs,
            &paths.data_dir,
            project_root,
        ),
    )?;

    Ok(())
}

/// A baseline is a comparison point, not the suite's subject: by the time
/// `run_baselines` runs, the project's own benchmarks are recorded, and a broken
/// external tool must not discard the baselines after it. So a failure is a
/// warning that names the baseline and its cause (a quiet-gated bench line
/// hid both), not an abort. Only a cooperative shutdown still ends the suite.
fn skip_on_error(what: &str, result: Result<(), DevError>) -> Result<(), DevError> {
    match result {
        Err(DevError::Interrupted) => Err(DevError::Interrupted),
        Err(e) => {
            output::warn(&format!("{what} skipped: {e}"));
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// osmpbf baseline
// ---------------------------------------------------------------------------

fn run_osmpbf_baseline(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    build_root: &BuildRoot,
) -> Result<(), DevError> {
    let project_root = build_root.as_path();
    let (binary, identity) = tools::osmpbf_baseline(build_root)?;
    // The baseline is its own crate with its own manifest profile: its rows
    // carry its build, not the suite's.
    harness.note_build(identity);

    // Run the baseline binary: {binary} {pbf} {runs}
    let pbf_str = crate::utf8_path::path_str(pbf_path)?;
    let invocation = Invocation::resolved(&binary, vec![pbf_str.to_owned(), runs.to_string()]);
    let captured = invocation.capture_in(project_root)?;
    invocation.check(&captured)?;

    // Every block is parsed and validated before the first row is recorded.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let variant_data: Vec<(String, i64, Vec<KvPair>)> =
        super::baseline_blocks::parse("osmpbf baseline", &stderr)?
            .into_iter()
            .map(|block| {
                let kv = kv_from_fields(&block.fields);
                (format!("osmpbf/{}", block.mode), block.elapsed_ms, kv)
            })
            .collect();

    let basename = crate::utf8_path::file_name(pbf_path);

    crate::bench_variants::run_variants_of(
        "baseline",
        &variant_data,
        |(variant, ..)| variant.as_str(),
        |(variant, elapsed_ms, kv)| {
            let config = BenchConfig {
                // `baseline-<tool>/<op>` - each external baseline is its own
                // row, distinguished by command. No natural cli/brokkr args
                // discriminator since baselines run different tools entirely.
                command: format!("baseline-{variant}"),
                input_file: Some(basename.clone()),
                input_mb: Some(file_mb),
                cargo_features: None,
                external_build: None,
                runs: 1,
                cli_args: None,
                metadata: vec![],
                arm_qualifier: None,
            };

            harness
                .run_internal(&config, |_i| {
                    Ok(BenchResult {
                        elapsed_ms: *elapsed_ms,
                        elapsed_us: None,
                        kv: kv.clone(),
                        iterations: Vec::new(),
                        distribution: None,
                        hotpath: None,
                    })
                })
                .map(|_| ())
        },
    )
}

// ---------------------------------------------------------------------------
// osmium baseline
// ---------------------------------------------------------------------------

fn run_osmium_baseline(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    scratch_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let pbf_str = crate::utf8_path::path_str(pbf_path)?;

    let basename = crate::utf8_path::file_name(pbf_path);

    let cat = Invocation::tool(
        ExternalTool::Osmium,
        strings(&[
            "cat",
            pbf_str,
            "-o",
            "/dev/null",
            "-f",
            "opl",
            "--overwrite",
        ]),
    );

    let config = BenchConfig {
        command: "baseline-osmium-cat-opl".into(),
        input_file: Some(basename.clone()),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: None,
        runs,
        cli_args: None,
        metadata: vec![],
        arm_qualifier: None,
    };

    harness.run_external(&config, &cat, project_root)?;

    // osmium add-locations-to-ways baseline
    std::fs::create_dir_all(scratch_dir)
        .map_err(|e| DevError::Config(format!("failed to create scratch dir: {e}")))?;

    let altw_output =
        PbfhoggLayout::fixed_scratch_file(scratch_dir, "bench-osmium-altw-output.osm.pbf");
    let altw_output_str = crate::utf8_path::path_str(&altw_output)?;

    let altw = Invocation::tool(
        ExternalTool::Osmium,
        strings(&[
            "add-locations-to-ways",
            pbf_str,
            "-o",
            altw_output_str,
            "--overwrite",
        ]),
    );

    let altw_config = BenchConfig {
        command: "baseline-osmium-add-locations-to-ways".into(),
        input_file: Some(basename.clone()),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: None,
        runs,
        cli_args: None,
        metadata: vec![],
        arm_qualifier: None,
    };

    harness.run_external(&altw_config, &altw, project_root)?;

    crate::reservations::remove_file(&altw_output).ok();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|&a| a.to_owned()).collect()
}

/// The KV pairs of an osmpbf result block's fields (its mode and elapsed time
/// are the row's own columns, not fields).
fn kv_from_fields(fields: &[(String, String)]) -> Vec<KvPair> {
    let mut kv = Vec::new();

    for (key, value) in fields {
        // Try to parse as number, fall back to string.
        if let Ok(n) = value.parse::<i64>() {
            kv.push(KvPair::int(key, n));
        } else if let Ok(f) = value.parse::<f64>() {
            kv.push(KvPair::real(key, f));
        } else {
            kv.push(KvPair::text(key, value));
        }
    }

    kv
}

#[cfg(test)]
mod skip_on_error_tests {
    use super::skip_on_error;
    use crate::error::DevError;

    #[test]
    fn a_failed_baseline_is_skipped() {
        assert!(skip_on_error("x", Err(DevError::Refused("boom".into()))).is_ok());
    }

    #[test]
    fn a_shutdown_still_ends_the_suite() {
        assert!(matches!(
            skip_on_error("x", Err(DevError::Interrupted)),
            Err(DevError::Interrupted)
        ));
    }
}
