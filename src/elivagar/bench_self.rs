//! Benchmark: full elivagar pipeline (PBF -> PMTiles).
//!
//! Replaces `bench-self.sh`. Builds the release binary, runs N times (best of),
//! and parses self-reported kv metrics from stderr (total_ms, phase12_ms,
//! ocean_ms, phase3_ms, phase4_ms, features, tiles, output_bytes).

use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Ocean shapefile detection (shared with hotpath.rs and profile.rs)
// ---------------------------------------------------------------------------

/// Detect ocean shapefiles in the data directory.
///
/// Returns (full-resolution, simplified) paths if they exist.
pub fn detect_ocean(data_dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let full = data_dir
        .join("water-polygons-split-3857")
        .join("water_polygons.shp");
    let simplified = data_dir
        .join("simplified-water-polygons-split-3857")
        .join("simplified_water_polygons.shp");

    (
        full.exists().then_some(full),
        simplified.exists().then_some(simplified),
    )
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    data_dir: &Path,
    scratch_dir: &Path,
    project_root: &Path,
    skip_to: Option<&str>,
    no_ocean: bool,
    compression_level: Option<u32>,
) -> Result<(), DevError> {
    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    std::fs::create_dir_all(scratch_dir)?;

    let output_path = scratch_dir.join("bench-self-output.pmtiles");
    let output_str = output_path.display().to_string();

    let tmp_dir = data_dir.join("tilegen_tmp");
    let tmp_dir_str = tmp_dir.display().to_string();

    // Build the command args: elivagar <pbf> <output> [flags]
    let mut args: Vec<String> = vec![
        pbf_str.into(),
        output_str,
        "--tmp-dir".into(),
        tmp_dir_str,
    ];

    if let Some(phase) = skip_to {
        args.push("--skip-to".into());
        args.push(phase.into());
    }

    if let Some(level) = compression_level {
        args.push("--compression-level".into());
        args.push(level.to_string());
    }

    // Add ocean shapefile paths if they exist.
    if !no_ocean {
        let (ocean, simplified) = detect_ocean(data_dir);
        if let Some(ref shp) = ocean {
            args.push("--ocean".into());
            args.push(shp.display().to_string());
        }
        if let Some(ref shp) = simplified {
            args.push("--ocean-simplified".into());
            args.push(shp.display().to_string());
        }
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!(
        "elivagar pipeline: {basename} ({file_mb:.0} MB), {runs} run(s)"
    ));

    let config = BenchConfig {
        command: "bench self".into(),
        variant: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
    };

    // Use kv parsing: elivagar emits elapsed_ms, phase12_ms, ocean_ms,
    // phase3_ms, phase4_ms, features, tiles, output_bytes to stderr.
    harness.run_external_with_kv(&config, binary, &arg_refs, project_root)?;

    // Clean up output.
    let _ = std::fs::remove_file(&output_path);

    Ok(())
}
