//! Combined benchmark suite: runs all pbfhogg benchmarks plus external baselines.

use std::collections::HashMap;
use std::path::Path;

use crate::build;
use crate::clean::layout::PbfhoggLayout;
use crate::config::ResolvedPaths;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;
use crate::pbfhogg::{
    bench_allocator, bench_blob_filter, bench_commands, bench_extract, bench_merge,
    bench_planetiler, bench_read, bench_write,
};
use crate::project::BuildRoot;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the full benchmark suite: read, write, merge, commands, plus external baselines.
///
/// External baselines (osmpbf, osmium, planetiler) are best-effort -- errors
/// skip the baseline without failing the whole run.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    paths: &ResolvedPaths,
    build_root: &BuildRoot,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    dataset: &str,
) -> Result<(), DevError> {
    let project_root = build_root.as_path();
    // 1. command presets; extract runs later with the dataset's configured bbox.
    output::bench_msg("=== bench commands ===");
    let binary = build::cargo_build(
        &build::BuildConfig::release_with_owned_features(
            crate::project::Project::Pbfhogg.cli_package(),
            &paths.features,
        ),
        project_root,
    )?
    .path;
    // Resolve a configured OSC through the verified resolver. Ambiguous
    // entries and stale hashes fail the suite instead of looking like an
    // optional dataset field that was never configured.
    let default_osc = match paths.datasets.get(dataset) {
        Some(ds) if !ds.osc.is_empty() => Some(crate::resolve::resolve_single_osc(
            dataset,
            &crate::resolve::SnapshotRef::Base,
            None,
            paths,
            project_root,
        )?),
        _ => None,
    };
    bench_commands::run(
        harness,
        &binary,
        pbf_path,
        default_osc.as_ref().map(|(path, _)| path.as_path()),
        default_osc.as_ref().map(|(_, seq)| seq.as_str()),
        &paths.scratch_dir,
        runs,
        bench_commands::ALL_COMMANDS,
        project_root,
    )?;

    // 2. bench read -- all 5 modes
    output::bench_msg("=== bench read ===");
    bench_read::run(
        harness,
        &binary,
        pbf_path,
        file_mb,
        runs,
        bench_read::ALL_MODES,
        project_root,
    )?;

    // 3. bench write -- sync + pipelined x default compressions
    output::bench_msg("=== bench write ===");
    let write_compressions =
        super::parse_compressions(super::DEFAULT_BENCH_WRITE_COMPRESSIONS, false)?;
    bench_write::run(
        harness,
        &binary,
        pbf_path,
        file_mb,
        runs,
        &write_compressions,
        project_root,
    )?;

    run_dataset_dependent(
        harness,
        paths,
        dataset,
        &binary,
        pbf_path,
        file_mb,
        runs,
        project_root,
        default_osc
            .as_ref()
            .map(|(path, seq)| (path.as_path(), seq.as_str())),
    )?;

    // bench allocator -- default, jemalloc, mimalloc
    output::bench_msg("=== bench allocator ===");
    bench_allocator::run(harness, pbf_path, file_mb, runs, project_root)?;

    run_baselines(harness, paths, build_root, pbf_path, file_mb, runs)
}

/// Run benchmarks that depend on optional dataset config fields (osc, bbox, pbf_raw).
#[allow(clippy::too_many_arguments)]
fn run_dataset_dependent(
    harness: &BenchHarness,
    paths: &ResolvedPaths,
    dataset: &str,
    binary: &Path,
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
        bench_merge::run(
            harness,
            binary,
            pbf_path,
            osc_path,
            file_mb,
            runs,
            &merge_compressions,
            false,
            &paths.scratch_dir,
            project_root,
        )?;
    } else {
        output::bench_msg("=== bench merge === (skipped, no osc file)");
    }

    // bench extract -- if bbox is available
    let bbox = ds
        .and_then(|dataset| dataset.bbox.as_deref())
        .map(crate::resolve::Bbox::parse)
        .transpose()?;

    if let Some(bbox) = bbox.as_ref() {
        output::bench_msg("=== bench extract ===");
        bench_extract::run(
            harness,
            binary,
            pbf_path,
            file_mb,
            runs,
            bbox,
            project_root,
            &paths.scratch_dir,
        )?;
    } else {
        output::bench_msg("=== bench extract === (skipped, no bbox in dataset config)");
    }

    // bench blob-filter -- if raw PBF is available
    let raw_pbf_path = if ds.is_some_and(|dataset| dataset.pbf.contains_key("raw")) {
        Some(crate::resolve::resolve_pbf_path(
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
    let manifest = project_root.join("bench/osmpbf-baseline/Cargo.toml");
    if manifest.exists() {
        output::bench_msg("=== osmpbf baseline ===");
        skip_on_error(
            "osmpbf baseline",
            run_osmpbf_baseline(harness, &manifest, pbf_path, file_mb, runs, build_root),
        )?;
    } else {
        output::bench_msg("=== osmpbf baseline === (skipped, no bench/osmpbf-baseline project)");
    }

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
    manifest: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    build_root: &BuildRoot,
) -> Result<(), DevError> {
    let project_root = build_root.as_path();
    let manifest_str = manifest.display().to_string();

    // Build the baseline binary under the hold, with the worktree's isolated
    // target dir when this is a `--commit` run.
    let captured = build::run_cargo_for_root(
        &[
            "build",
            "--release",
            "--manifest-path",
            &manifest_str,
            "--message-format=json",
        ],
        build_root,
        &[],
        None,
        crate::shutdown::ChildIsolation::Inherit,
    )?;

    if !captured.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "osmpbf-baseline build failed: {stderr}"
        )));
    }

    let binary = build::find_executable(&captured.stdout, None)?;

    // Run the baseline binary: {binary} {pbf} {runs}
    let pbf_str = crate::utf8_path::path_str(pbf_path)?;
    let runs_str = runs.to_string();
    let binary_str = binary.display().to_string();

    let captured = output::run_captured(&binary_str, &[pbf_str, &runs_str], project_root)?;

    captured.check_success(&binary_str)?;

    // Parse stderr for --- delimited blocks.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let blocks = parse_stderr_blocks(&stderr);

    let basename = crate::utf8_path::file_name(pbf_path);

    // Build variant list from parsed blocks.
    let variant_data: Vec<(String, i64, Vec<KvPair>)> = blocks
        .iter()
        .filter_map(|block| {
            let mode = block.get("mode")?.clone();
            let elapsed_ms: i64 = block.get("elapsed_ms")?.parse().ok()?;
            let kv = build_kv_from_block(block);
            Some((format!("osmpbf/{mode}"), elapsed_ms, kv))
        })
        .collect();
    let variant_names: Vec<&str> = variant_data.iter().map(|(v, ..)| v.as_str()).collect();

    crate::harness::run_variants("baseline", &variant_names, |variant| {
        let (_, elapsed_ms, kv) = variant_data
            .iter()
            .find(|(v, ..)| v == variant)
            .expect("variant exists in variant_data");

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
    })
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

    let osmium_args: Vec<&str> = vec![
        "cat",
        pbf_str,
        "-o",
        "/dev/null",
        "-f",
        "opl",
        "--overwrite",
    ];

    let config = BenchConfig {
        command: "baseline-osmium-cat-opl".into(),
        input_file: Some(basename.clone()),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: None,
        runs,
        cli_args: Some(crate::harness::format_cli_args("osmium", &osmium_args)),
        metadata: vec![],
    };

    harness.run_external(&config, Path::new("osmium"), &osmium_args, project_root)?;

    // osmium add-locations-to-ways baseline
    std::fs::create_dir_all(scratch_dir)
        .map_err(|e| DevError::Config(format!("failed to create scratch dir: {e}")))?;

    let altw_output =
        PbfhoggLayout::fixed_scratch_file(scratch_dir, "bench-osmium-altw-output.osm.pbf");
    let altw_output_str = crate::utf8_path::path_str(&altw_output)?;

    let altw_args: Vec<&str> = vec![
        "add-locations-to-ways",
        pbf_str,
        "-o",
        altw_output_str,
        "--overwrite",
    ];

    let altw_config = BenchConfig {
        command: "baseline-osmium-add-locations-to-ways".into(),
        input_file: Some(basename.clone()),
        input_mb: Some(file_mb),
        cargo_features: None,
        external_build: None,
        runs,
        cli_args: Some(crate::harness::format_cli_args("osmium", &altw_args)),
        metadata: vec![],
    };

    harness.run_external(&altw_config, Path::new("osmium"), &altw_args, project_root)?;

    std::fs::remove_file(&altw_output).ok();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse `---` delimited blocks from stderr output.
///
/// The osmpbf baseline emits results as blocks separated by `---` lines, with
/// `key=value` pairs on each line. Planetiler has its own typed parser.
fn parse_stderr_blocks(stderr: &str) -> Vec<HashMap<String, String>> {
    let mut blocks = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;

    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            // Start a new block, pushing the previous one if it exists.
            if let Some(block) = current.take()
                && !block.is_empty()
            {
                blocks.push(block);
            }
            current = Some(HashMap::new());
            continue;
        }

        if let Some(ref mut block) = current
            && let Some((key, value)) = trimmed.split_once('=')
        {
            block.insert(key.to_owned(), value.to_owned());
        }
    }

    // Push the final block.
    if let Some(block) = current
        && !block.is_empty()
    {
        blocks.push(block);
    }

    blocks
}

/// Build a KvPair vec from a parsed block, including relevant fields.
fn build_kv_from_block(block: &HashMap<String, String>) -> Vec<KvPair> {
    let mut kv = Vec::new();

    for (key, value) in block {
        // Skip keys already used as primary fields.
        if key == "mode" || key == "elapsed_ms" {
            continue;
        }
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
