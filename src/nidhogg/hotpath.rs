//! Hotpath profiling for nidhogg: function-level timing and allocation instrumentation.
//!
//! Runs the nidhogg binary (pre-built with the appropriate feature) with hotpath
//! metrics collection enabled.

use std::path::Path;

use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness};
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
    scratch_dir: &Path,
    file_mb: f64,
    runs: usize,
    alloc: bool,
    project_root: &Path,
) -> Result<(), DevError> {
    let binary_str = super::client::path_str(binary)?;
    let pbf_str = super::client::path_str(pbf_path)?;

    std::fs::create_dir_all(scratch_dir)?;

    let suffix = if alloc { "alloc-" } else { "" };
    let output_dir = scratch_dir.join(format!("hotpath-{suffix}output"));
    let output_str = output_dir.display().to_string();

    // Build argument list: nidhogg ingest <pbf> <output_dir>
    let args: Vec<&str> = vec!["ingest", pbf_str, &output_str];

    let label = crate::harness::hotpath_feature(alloc);
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

    let variant_suffix = crate::harness::hotpath_variant_suffix(alloc);
    let variant = format!("ingest{variant_suffix}");

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(crate::harness::format_cli_args(binary_str, &args)),
        metadata: vec![KvPair::text("meta.alloc", alloc.to_string())],
    };

    harness.run_internal(&config, |_i| {
        let (result, _stderr) = harness::run_hotpath_capture(binary_str, &args, scratch_dir, project_root, &[])?;
        Ok(result)
    })?;

    // Clean up output directory.
    std::fs::remove_dir_all(&output_dir).ok();

    Ok(())
}
