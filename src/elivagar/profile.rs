//! Sampling profiler support for elivagar.
//!
//! Replaces `run-perf.sh` and `run-samply.sh`. Builds with `--profile profiling`
//! (release + debug symbols), then runs under `perf record` or `samply record`.
//! Results (elapsed time) are stored in `.brokkr/results.db`.

use std::path::Path;

use crate::build;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;


// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run elivagar under a sampling profiler.
///
/// `tool` must be `"perf"` or `"samply"`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    data_dir: &Path,
    scratch_dir: &Path,
    tool: &str,
    no_ocean: bool,
    force_sorted: bool,
    allow_unsafe_flat_index: bool,
    tile_format: Option<&str>,
    tile_compression: Option<&str>,
    compress_sort_chunks: bool,
    in_memory: bool,
    locations_on_ways: bool,
    extra_features: &[String],
    project_root: &Path,
) -> Result<(), DevError> {
    match tool {
        "perf" | "samply" => {}
        other => {
            return Err(DevError::Config(format!(
                "unknown profiling tool '{other}' (expected: perf, samply)"
            )));
        }
    }

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    // Build with profiling profile.
    output::build_msg("building with profiling profile (release + debug symbols)");

    let config = build::BuildConfig {
        package: None,
        bin: None,
        example: None,
        features: extra_features.to_vec(),
        default_features: true,
        profile: "profiling",
    };
    let binary = build::cargo_build(&config, project_root)?;
    let binary_str = binary.display().to_string();

    std::fs::create_dir_all(scratch_dir)?;
    let output_pmtiles = scratch_dir.join("profile-output.pmtiles");
    let output_pmtiles_str = output_pmtiles.display().to_string();

    let tmp_dir = data_dir.join("tilegen_tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_dir_str = tmp_dir.display().to_string();

    // Build elivagar argument list: elivagar run <pbf> -o <output> [flags]
    let mut elivagar_args: Vec<String> = vec![
        "run".into(),
        pbf_str.into(),
        "-o".into(),
        output_pmtiles_str,
        "--tmp-dir".into(),
        tmp_dir_str,
    ];

    // Ocean flags.
    super::push_ocean_args(&mut elivagar_args, data_dir, no_ocean);
    if force_sorted {
        elivagar_args.push("--force-sorted".into());
    }
    if allow_unsafe_flat_index {
        elivagar_args.push("--allow-unsafe-flat-index".into());
    }
    if let Some(fmt) = tile_format {
        elivagar_args.push("--tile-format".into());
        elivagar_args.push(fmt.into());
    }
    if let Some(comp) = tile_compression {
        elivagar_args.push("--tile-compression".into());
        elivagar_args.push(comp.into());
    }
    if compress_sort_chunks {
        elivagar_args.push("--compress-sort-chunks".into());
    }
    if in_memory {
        elivagar_args.push("--in-memory".into());
    }
    if locations_on_ways {
        elivagar_args.push("--locations-on-ways".into());
    }

    // Collect git info for profiler output naming.
    let git_info = crate::git::collect(project_root)?;
    let hostname = crate::config::hostname()?;

    let elapsed_ms = match tool {
        "perf" => crate::profiler::run_perf(
            &binary_str,
            &elivagar_args,
            &hostname,
            &git_info.commit,
            data_dir,
            project_root,
        ),
        "samply" => crate::profiler::run_samply(
            &binary_str,
            &elivagar_args,
            &hostname,
            &git_info.commit,
            data_dir,
            project_root,
        ),
        _ => unreachable!(),
    }?;

    // Store result in DB.
    let bench_config = BenchConfig {
        command: "profile".into(),
        variant: Some(tool.into()),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "profiling".into(),
        runs: 1,
        cli_args: None,
        metadata: {
            let mut m = vec![
                KvPair::text("meta.tool", tool),
                KvPair::text("meta.ocean", (!no_ocean).to_string()),
                KvPair::text("meta.force_sorted", force_sorted.to_string()),
                KvPair::text(
                    "meta.allow_unsafe_flat_index",
                    allow_unsafe_flat_index.to_string(),
                ),
            ];
            if let Some(v) = tile_format {
                m.push(KvPair::text("meta.tile_format", v));
            }
            if let Some(v) = tile_compression {
                m.push(KvPair::text("meta.tile_compression", v));
            }
            m.push(KvPair::text("meta.compress_sort_chunks", compress_sort_chunks.to_string()));
            m.push(KvPair::text("meta.in_memory", in_memory.to_string()));
            m.push(KvPair::text("meta.locations_on_ways", locations_on_ways.to_string()));
            m
        },
    };

    let result = BenchResult {
        elapsed_ms,
        kv: vec![],
        distribution: None,
        hotpath: None,
    };

    harness.record_result(&bench_config, &result)?;

    // Clean up output PMTiles.
    std::fs::remove_file(output_pmtiles).ok();

    Ok(())
}
