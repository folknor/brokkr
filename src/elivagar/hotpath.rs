//! Hotpath profiling for elivagar: function-level timing and allocation instrumentation.
//!
//! Replaces `run-hotpath.sh` and `run-hotpath-alloc.sh`. Runs the elivagar
//! binary (pre-built with the appropriate feature) with hotpath metrics
//! collection enabled and HOTPATH_METRICS_SERVER_OFF=true.

use std::path::Path;
use std::time::Duration;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;

use super::bench_self::detect_ocean;

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
    let tmp_dir_str = tmp_dir.display().to_string();

    // Build argument list.
    let mut args: Vec<String> = vec![
        pbf_str.into(),
        output_str,
        "--tmp-dir".into(),
        tmp_dir_str,
    ];

    // Ocean flags.
    let (ocean, simplified) = detect_ocean(data_dir);
    if let Some(ref shp) = ocean {
        args.push("--ocean".into());
        args.push(shp.display().to_string());
    }
    if let Some(ref shp) = simplified {
        args.push("--ocean-simplified".into());
        args.push(shp.display().to_string());
    }

    let label = if alloc { "hotpath-alloc" } else { "hotpath" };
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

    let feature = if alloc { "hotpath-alloc" } else { "hotpath" };
    let variant_suffix = if alloc { "/alloc" } else { "" };
    let variant = format!("tilegen{variant_suffix}");

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: Some(feature.into()),
        cargo_profile: "release".into(),
        runs,
    };

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    harness.run_internal(&config, |_i| {
        let json_file = scratch_dir.join("hotpath-report.json");
        let json_file_str = json_file.display().to_string();

        let captured = output::run_captured_with_env(
            binary_str,
            &args_refs,
            project_root,
            &[
                ("HOTPATH_METRICS_SERVER_OFF", "true"),
                ("HOTPATH_OUTPUT_FORMAT", "json"),
                ("HOTPATH_OUTPUT_PATH", &json_file_str),
            ],
        )?;

        let ms = elapsed_to_ms(&captured.elapsed);

        // Read and parse the JSON hotpath report.
        let extra = std::fs::read_to_string(&json_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        std::fs::remove_file(&json_file).ok();

        Ok(BenchResult {
            elapsed_ms: ms,
            extra,
        })
    })?;

    // Clean up output file.
    std::fs::remove_file(&output_path).ok();

    Ok(())
}

fn elapsed_to_ms(duration: &Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
