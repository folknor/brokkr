//! Benchmark: Tilemaker Shortbread tilegen for comparison.
//!
//! Replaces `bench-tilemaker.sh`. Auto-downloads/builds Tilemaker + shortbread
//! config via `tools::ensure_tilemaker`, fetches 4326 ocean shapefiles, symlinks
//! them into the shortbread config tree, then runs N times with external
//! wall-clock timing (best of N).

use std::path::Path;

use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness};
use crate::output;
use crate::tools;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    runs: usize,
    data_dir: &Path,
    scratch_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let tm = tools::ensure_tilemaker(data_dir)?;
    super::download_ocean::ensure_ocean_4326(data_dir)?;

    // Symlink ocean directories into shortbread-tilemaker/data/ so Tilemaker
    // can find them at the relative paths its config expects.
    let sb_data = data_dir.join("shortbread-tilemaker/data");
    std::fs::create_dir_all(&sb_data)?;

    let full_link = sb_data.join("water-polygons-split-4326");
    if !full_link.exists() {
        std::os::unix::fs::symlink(data_dir.join("water-polygons-split-4326"), &full_link)?;
    }

    let simplified_link = sb_data.join("simplified-water-polygons-split-4326");
    if !simplified_link.exists() {
        std::os::unix::fs::symlink(
            data_dir.join("simplified-water-polygons-split-4326"),
            &simplified_link,
        )?;
    }

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    std::fs::create_dir_all(scratch_dir)?;
    let output_path = scratch_dir.join("tilemaker-bench.pmtiles");
    let output_str = output_path.display().to_string();

    output::bench_msg(&format!(
        "Tilemaker Shortbread: {basename} ({file_mb:.0} MB), {runs} run(s)"
    ));

    let config_str = tm.config.display().to_string();
    let process_str = tm.process.display().to_string();

    let args_owned: Vec<String> = vec![
        "--input".into(),
        pbf_str.into(),
        "--output".into(),
        output_str,
        "--config".into(),
        config_str,
        "--process".into(),
        process_str,
        "--fast".into(),
    ];
    let args_refs: Vec<&str> = args_owned.iter().map(String::as_str).collect();

    let config = BenchConfig {
        command: "bench tilemaker".into(),
        variant: Some("shortbread".into()),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "cmake".into(),
        runs,
        cli_args: Some(harness::format_cli_args(
            &tm.tilemaker.display().to_string(),
            &args_refs,
        )),
        metadata: vec![],
    };

    harness.run_external(&config, &tm.tilemaker, &args_refs, project_root)?;

    // Clean up output file.
    std::fs::remove_file(&output_path).ok();

    Ok(())
}
