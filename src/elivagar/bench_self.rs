//! Benchmark: full elivagar pipeline (PBF -> PMTiles).
//!
//! Replaces `bench-self.sh`. Builds the release binary, runs N times (best of),
//! and parses self-reported kv metrics from stderr (total_ms, phase12_ms,
//! ocean_ms, phase3_ms, phase4_ms, features, tiles, output_bytes).

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness};
use crate::output;

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
    force_sorted: bool,
    compression_level: Option<u32>,
    allow_unsafe_flat_index: bool,
    tile_format: Option<&str>,
    tile_compression: Option<&str>,
    compress_sort_chunks: bool,
    in_memory: bool,
    locations_on_ways: bool,
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
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_dir_str = tmp_dir.display().to_string();

    // Build the command args: elivagar run <pbf> -o <output> [flags]
    let mut args: Vec<String> = vec![
        "run".into(),
        pbf_str.into(),
        "-o".into(),
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
    if force_sorted {
        args.push("--force-sorted".into());
    }
    if allow_unsafe_flat_index {
        args.push("--allow-unsafe-flat-index".into());
    }
    if let Some(fmt) = tile_format {
        args.push("--tile-format".into());
        args.push(fmt.into());
    }
    if let Some(comp) = tile_compression {
        args.push("--tile-compression".into());
        args.push(comp.into());
    }
    if compress_sort_chunks {
        args.push("--compress-sort-chunks".into());
    }
    if in_memory {
        args.push("--in-memory".into());
    }
    if locations_on_ways {
        args.push("--locations-on-ways".into());
    }

    // Add ocean shapefile paths if they exist.
    super::push_ocean_args(&mut args, data_dir, no_ocean);

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    output::bench_msg(&format!(
        "elivagar pipeline: {basename} ({file_mb:.0} MB), {runs} run(s)"
    ));

    let mut metadata = vec![KvPair::text("meta.ocean", (!no_ocean).to_string())];
    if let Some(v) = skip_to {
        metadata.push(KvPair::text("meta.skip_to", v));
    }
    if let Some(v) = compression_level {
        metadata.push(KvPair::int("meta.compression_level", v as i64));
    }
    metadata.push(KvPair::text("meta.force_sorted", force_sorted.to_string()));
    metadata.push(KvPair::text(
        "meta.allow_unsafe_flat_index",
        allow_unsafe_flat_index.to_string(),
    ));
    if let Some(v) = tile_format {
        metadata.push(KvPair::text("meta.tile_format", v));
    }
    if let Some(v) = tile_compression {
        metadata.push(KvPair::text("meta.tile_compression", v));
    }
    metadata.push(KvPair::text("meta.compress_sort_chunks", compress_sort_chunks.to_string()));
    metadata.push(KvPair::text("meta.in_memory", in_memory.to_string()));
    metadata.push(KvPair::text("meta.locations_on_ways", locations_on_ways.to_string()));

    let config = BenchConfig {
        command: "bench self".into(),
        variant: None,
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &arg_refs)),
        metadata,
    };

    // Use kv parsing: elivagar emits elapsed_ms, phase12_ms, ocean_ms,
    // phase3_ms, phase4_ms, features, tiles, output_bytes to stderr.
    harness.run_external_with_kv(&config, binary, &arg_refs, project_root)?;

    // Clean up output.
    std::fs::remove_file(&output_path).ok();

    Ok(())
}
