//! Hotpath profiling for nidhogg: function-level timing and allocation instrumentation.
//!
//! Runs the nidhogg binary (pre-built with the appropriate feature) with hotpath
//! metrics collection enabled.

use std::path::Path;
use std::time::Duration;

use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run nidhogg with hotpath instrumentation.
///
/// The `binary` must already be built with `--features hotpath` (or
/// `--features hotpath-alloc` when `alloc` is true). The caller is responsible
/// for building the binary with the correct features before calling this.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    pbf_path: &Path,
    _data_dir: &str,
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
    let output_dir = scratch_dir.join(format!("hotpath-{suffix}output"));
    let output_str = output_dir.display().to_string();

    // Build argument list: nidhogg ingest <pbf> <output_dir>
    let args: Vec<&str> = vec!["ingest", pbf_str, &output_str];

    let label = if alloc { "hotpath-alloc" } else { "hotpath" };
    output::hotpath_msg(&format!("=== nidhogg {label} ==="));

    if alloc {
        output::hotpath_msg(
            "NOTE: alloc profiling -- wall-clock times are not meaningful"
        );
    }

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    let feature = if alloc { "hotpath-alloc" } else { "hotpath" };
    let variant_suffix = if alloc { "/alloc" } else { "" };
    let variant = format!("ingest{variant_suffix}");

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: Some(feature.into()),
        cargo_profile: "release".into(),
        runs,
    };

    harness.run_internal(&config, |_i| {
        let json_file = scratch_dir.join("hotpath-report.json");
        let json_file_str = json_file.display().to_string();

        let captured = output::run_captured_with_env(
            binary_str,
            &args,
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

    // Clean up output directory.
    std::fs::remove_dir_all(&output_dir).ok();

    Ok(())
}

fn elapsed_to_ms(duration: &Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
