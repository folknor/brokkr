//! Hotpath profiling for elivagar: function-level timing and allocation instrumentation.
//!
//! Replaces `run-hotpath.sh` and `run-hotpath-alloc.sh`. Runs the elivagar
//! binary (pre-built with the appropriate feature) with hotpath metrics
//! collection enabled and HOTPATH_METRICS_SERVER_OFF=true.

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness};
use crate::output;


// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run elivagar with hotpath instrumentation.
///
/// The `binary` must already be built with `--features hotpath` (or
/// `--features hotpath-alloc` when `alloc` is true). The caller is responsible
/// for building the binary with the correct features before calling this.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    data_dir: &Path,
    scratch_dir: &Path,
    file_mb: f64,
    runs: usize,
    alloc: bool,
    no_ocean: bool,
    force_sorted: bool,
    allow_unsafe_flat_index: bool,
    tile_format: Option<&str>,
    tile_compression: Option<&str>,
    compress_sort_chunks: bool,
    in_memory: bool,
    locations_on_ways: bool,
    project_root: &Path,
) -> Result<(), DevError> {
    let binary_str = binary
        .to_str()
        .ok_or_else(|| DevError::Config("binary path is not valid UTF-8".into()))?;

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    std::fs::create_dir_all(scratch_dir)?;

    let suffix = if alloc { "alloc-" } else { "" };
    let output_path = scratch_dir.join(format!("hotpath-{suffix}output.pmtiles"));
    let output_str = output_path.display().to_string();

    let tmp_dir = data_dir.join("tilegen_tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_dir_str = tmp_dir.display().to_string();

    // Build argument list: elivagar run <pbf> -o <output> [flags]
    let mut args: Vec<String> = vec![
        "run".into(),
        pbf_str.into(),
        "-o".into(),
        output_str,
        "--tmp-dir".into(),
        tmp_dir_str,
    ];

    // Ocean flags.
    super::push_ocean_args(&mut args, data_dir, no_ocean);
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

    let label = crate::harness::hotpath_feature(alloc);
    output::hotpath_msg(&format!("=== elivagar {label} ==="));

    if alloc {
        output::hotpath_msg(
            "NOTE: mimalloc is disabled for alloc profiling -- wall-clock times are not meaningful"
        );
    }

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let variant_suffix = crate::harness::hotpath_variant_suffix(alloc);
    let variant = format!("tilegen{variant_suffix}");

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(crate::harness::format_cli_args(&binary.display().to_string(), &args_refs)),
        metadata: {
            let mut m = vec![
                KvPair::text("meta.alloc", alloc.to_string()),
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

    harness.run_internal(&config, |_i| {
        harness::run_hotpath_capture(
            binary_str, &args_refs, scratch_dir, project_root,
            &[("ELIVAGAR_NODE_STATS", "1")],
        )
    })?;

    // Clean up output file.
    std::fs::remove_file(&output_path).ok();

    Ok(())
}
