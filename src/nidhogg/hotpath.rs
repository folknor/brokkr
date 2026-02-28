//! Hotpath profiling for nidhogg: function-level timing and allocation instrumentation.
//!
//! Runs the nidhogg binary (pre-built with the appropriate feature) with hotpath
//! metrics collection enabled.

use std::path::Path;

use crate::error::DevError;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run nidhogg with hotpath instrumentation.
///
/// The `binary` must already be built with `--features hotpath` (or
/// `--features hotpath-alloc` when `alloc` is true). The caller is responsible
/// for building the binary with the correct features before calling this.
pub fn run(
    binary: &Path,
    pbf_path: &Path,
    data_dir: &str,
    scratch_dir: &Path,
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

    let captured = output::run_captured_with_env(
        binary_str,
        &args,
        project_root,
        &[("HOTPATH_METRICS_SERVER_OFF", "true")],
    )?;

    // Print stdout (hotpath output).
    let stdout = String::from_utf8_lossy(&captured.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }

    // Print stderr.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    // Clean up output directory.
    let _ = std::fs::remove_dir_all(&output_dir);

    Ok(())
}
