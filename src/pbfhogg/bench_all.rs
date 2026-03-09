//! Combined benchmark suite: runs all pbfhogg benchmarks plus external baselines.

use std::collections::HashMap;
use std::path::Path;

use crate::build;
use crate::config::ResolvedPaths;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;
use crate::pbfhogg::{
    bench_allocator, bench_blob_filter, bench_commands, bench_extract, bench_merge,
    bench_planetiler, bench_read, bench_write,
};

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
    project_root: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    dataset: &str,
) -> Result<(), DevError> {
    // 1. bench commands -- all
    output::bench_msg("=== bench commands ===");
    let binary = build::cargo_build(&build::BuildConfig::release(Some("pbfhogg-cli")), project_root)?;
    let osc_path = crate::resolve::get_default_osc_entry(dataset, paths)
        .map(|entry| paths.data_dir.join(&entry.file))
        .filter(|p| p.exists());
    bench_commands::run(
        harness,
        &binary,
        pbf_path,
        osc_path.as_deref(),
        Some(&paths.scratch_dir),
        file_mb,
        runs,
        bench_commands::ALL_COMMANDS,
        project_root,
        None,
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
    let write_compressions = super::parse_compressions("none,zlib,zstd", true)?;
    bench_write::run(
        harness,
        &binary,
        pbf_path,
        file_mb,
        runs,
        &write_compressions,
        project_root,
    )?;

    run_dataset_dependent(harness, paths, dataset, &binary, pbf_path, file_mb, runs, project_root)?;

    // bench allocator -- default, jemalloc, mimalloc
    output::bench_msg("=== bench allocator ===");
    bench_allocator::run(harness, pbf_path, file_mb, runs, project_root)?;

    run_baselines(harness, paths, project_root, pbf_path, file_mb, runs)
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
) -> Result<(), DevError> {
    let ds = paths.datasets.get(dataset);

    // bench merge -- if osc is available
    let osc_path = crate::resolve::get_default_osc_entry(dataset, paths)
        .map(|entry| paths.data_dir.join(&entry.file))
        .filter(|p| p.exists());

    if let Some(ref osc_path) = osc_path {
        output::bench_msg("=== bench merge ===");
        let merge_compressions = super::parse_compressions("zlib,none", false)?;
        bench_merge::run(
            harness, binary, pbf_path, osc_path, file_mb, runs,
            &merge_compressions, false, &paths.scratch_dir, project_root,
        )?;
    } else {
        output::bench_msg("=== bench merge === (skipped, no osc file)");
    }

    // bench extract -- if bbox is available
    let bbox = ds.and_then(|d| d.bbox.as_ref());

    if let Some(bbox) = bbox {
        output::bench_msg("=== bench extract ===");
        bench_extract::run(
            harness, binary, pbf_path, file_mb, runs,
            bbox, bench_extract::ALL_STRATEGIES, project_root,
            &paths.scratch_dir,
        )?;
    } else {
        output::bench_msg("=== bench extract === (skipped, no bbox in dataset config)");
    }

    // bench blob-filter -- if raw PBF is available
    let raw_pbf_path = crate::resolve::get_pbf_entry(dataset, "raw", paths)
        .map(|entry| paths.data_dir.join(&entry.file))
        .filter(|p| p.exists());

    if let Some(ref raw_path) = raw_pbf_path {
        output::bench_msg("=== bench blob-filter ===");
        bench_blob_filter::run(
            harness, binary, pbf_path, raw_path, file_mb, runs, project_root,
            &paths.scratch_dir,
        )?;
    } else {
        output::bench_msg("=== bench blob-filter === (skipped, no raw pbf variant in dataset config)");
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
    project_root: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
) -> Result<(), DevError> {
    // osmpbf baseline -- build and run
    output::bench_msg("=== osmpbf baseline ===");
    let manifest = project_root.join("bench/osmpbf-baseline/Cargo.toml");
    if manifest.exists() {
        match run_osmpbf_baseline(harness, &manifest, pbf_path, file_mb, runs, project_root) {
            Ok(()) => {}
            Err(e) => output::bench_msg(&format!("osmpbf baseline skipped: {e}")),
        }
    }

    // osmium -- if available
    output::bench_msg("=== osmium baseline ===");
    if super::verify::which_exists("osmium") {
        run_osmium_baseline(harness, pbf_path, file_mb, runs, &paths.scratch_dir, project_root)?;
    }

    // planetiler -- if available
    output::bench_msg("=== planetiler baseline ===");
    match bench_planetiler::run(harness, pbf_path, file_mb, runs, &paths.data_dir, project_root)
    {
        Ok(()) => {}
        Err(e) => output::bench_msg(&format!("planetiler skipped: {e}")),
    }

    Ok(())
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
    project_root: &Path,
) -> Result<(), DevError> {
    let manifest_str = manifest.display().to_string();

    // Build the baseline binary.
    let captured = output::run_captured(
        "cargo",
        &[
            "build",
            "--release",
            "--manifest-path",
            &manifest_str,
            "--message-format=json",
        ],
        project_root,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!(
            "osmpbf-baseline build failed: {stderr}"
        )));
    }

    let binary = build::find_executable(&captured.stdout, None)?;

    // Run the baseline binary: {binary} {pbf} {runs}
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;
    let runs_str = runs.to_string();
    let binary_str = binary.display().to_string();

    let captured = output::run_captured(
        &binary_str,
        &[pbf_str, &runs_str],
        project_root,
    )?;

    captured.check_success(&binary_str)?;

    // Parse stderr for --- delimited blocks.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    let blocks = parse_stderr_blocks(&stderr);

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    for block in &blocks {
        let mode = match block.get("mode") {
            Some(m) => m.clone(),
            None => continue,
        };

        let elapsed_ms: i64 = match block.get("elapsed_ms").and_then(|v| v.parse().ok()) {
            Some(ms) => ms,
            None => continue,
        };

        let variant = format!("osmpbf/{mode}");
        let kv = build_kv_from_block(block);

        let config = BenchConfig {
            command: "bench baseline".into(),
            variant: Some(variant),
            input_file: Some(basename.clone()),
            input_mb: Some(file_mb),
            cargo_features: None,
            cargo_profile: "release".into(),
            runs: 1,
            cli_args: None,
            metadata: vec![],
        };

        harness.run_internal(&config, |_i| {
            Ok(BenchResult {
                elapsed_ms,
                kv: kv.clone(),
                distribution: None,
                hotpath: None,
            })
        })?;
    }

    Ok(())
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
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let osmium_args: Vec<&str> = vec!["cat", pbf_str, "-o", "/dev/null", "-f", "opl", "--overwrite"];

    let config = BenchConfig {
        command: "bench baseline".into(),
        variant: Some("osmium/cat-opl".into()),
        input_file: Some(basename.clone()),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(crate::harness::format_cli_args("osmium", &osmium_args)),
        metadata: vec![],
    };

    harness.run_external(
        &config,
        Path::new("osmium"),
        &osmium_args,
        project_root,
    )?;

    // osmium add-locations-to-ways baseline
    std::fs::create_dir_all(scratch_dir).map_err(|e| {
        DevError::Config(format!("failed to create scratch dir: {e}"))
    })?;

    let altw_output = scratch_dir.join("bench-osmium-altw-output.osm.pbf");
    let altw_output_str = altw_output
        .to_str()
        .ok_or_else(|| DevError::Config("scratch path is not valid UTF-8".into()))?;

    let altw_args: Vec<&str> = vec![
        "add-locations-to-ways", pbf_str, "-o", altw_output_str, "--overwrite",
    ];

    let altw_config = BenchConfig {
        command: "bench baseline".into(),
        variant: Some("osmium/add-locations-to-ways".into()),
        input_file: Some(basename.clone()),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(crate::harness::format_cli_args("osmium", &altw_args)),
        metadata: vec![],
    };

    harness.run_external(
        &altw_config,
        Path::new("osmium"),
        &altw_args,
        project_root,
    )?;

    std::fs::remove_file(&altw_output).ok();

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse `---` delimited blocks from stderr output.
///
/// Both the osmpbf-baseline and planetiler benchmark emit results as blocks
/// separated by `---` lines, with `key=value` pairs on each line within a block.
fn parse_stderr_blocks(stderr: &str) -> Vec<HashMap<String, String>> {
    let mut blocks = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;

    for line in stderr.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            // Start a new block, pushing the previous one if it exists.
            if let Some(block) = current.take()
                && !block.is_empty() {
                    blocks.push(block);
                }
            current = Some(HashMap::new());
            continue;
        }

        if let Some(ref mut block) = current
            && let Some((key, value)) = trimmed.split_once('=') {
                block.insert(key.to_owned(), value.to_owned());
            }
    }

    // Push the final block.
    if let Some(block) = current
        && !block.is_empty() {
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
