//! Hotpath profiling for sluggrs: function-level timing and allocation instrumentation.
//!
//! Builds the `hotpath` example binary with the appropriate feature and runs it
//! through the bench harness to capture per-function timing and allocation data.
//!
//! The sluggrs `hotpath` example exercises two rendering paths:
//! - **cache-miss**: first frame with cold glyph cache (outline extraction +
//!   band building + texture upload)
//! - **cache-hit**: subsequent frames reusing cached glyphs (vertex buffer reuse)

use std::path::Path;

use crate::build;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness};
use crate::measure::MeasureRequest;
use crate::output;

// ---------------------------------------------------------------------------
// Build helper
// ---------------------------------------------------------------------------

/// Build an example binary with the appropriate hotpath feature.
fn build_hotpath_example(
    project_root: &Path,
    target: &str,
    alloc: bool,
    extra_features: &[String],
) -> Result<std::path::PathBuf, DevError> {
    let hotpath_feature = if alloc { "hotpath-alloc" } else { "hotpath" };
    let mut features = vec![hotpath_feature.to_owned()];
    for f in extra_features {
        if !features.iter().any(|existing| existing == f) {
            features.push(f.clone());
        }
    }

    // The default "hotpath" target maps to the "hotpath" example directly;
    // other targets map to "{target}_bench" examples (e.g. "email" → "email_bench").
    let example_name = if target == "hotpath" {
        target.to_owned()
    } else {
        format!("{target}_bench")
    };

    let config = build::BuildConfig {
        package: None,
        bin: None,
        example: Some(example_name),
        features,
        default_features: true,
        profile: "release",
    };
    build::cargo_build(&config, project_root)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run sluggrs hotpath profiling.
///
/// Builds the `examples/hotpath.rs` binary with the `hotpath` (or
/// `hotpath-alloc`) feature and runs it through the bench harness. The example
/// binary is expected to exercise the rendering pipeline and emit hotpath
/// metrics via the standard env-var mechanism.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    binary: &Path,
    runs: usize,
    alloc: bool,
    project_root: &Path,
    scratch_dir: &Path,
    variant_base: &str,
) -> Result<(), DevError> {
    let binary_str = binary
        .to_str()
        .ok_or_else(|| DevError::Config("binary path is not valid UTF-8".into()))?;

    std::fs::create_dir_all(scratch_dir)?;

    let label = harness::hotpath_feature(alloc);
    output::hotpath_msg(&format!("=== sluggrs {label} ==="));

    if alloc {
        output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
    }

    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("{variant_base}{variant_suffix}");

    let config = BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: "release".into(),
        runs,
        cli_args: Some(harness::format_cli_args(binary_str, &[])),
        metadata: vec![KvPair::text("meta.alloc", alloc.to_string())],
    };

    harness.run_internal(&config, |_i| {
        let (result, _stderr, _sidecar) =
            harness::run_hotpath_capture(binary_str, &[], scratch_dir, project_root, &[], &[], None)?;
        Ok(result)
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Command entry point (called from cmd dispatch)
// ---------------------------------------------------------------------------

/// Top-level hotpath command for sluggrs.
///
/// Handles lock acquisition, building the example binary with the correct
/// features, and delegating to [`run`].
pub(crate) fn cmd(req: &MeasureRequest, target: &str) -> Result<(), DevError> {
    let effective_root = req.effective_build_root();
    let pi = crate::context::bootstrap(req.build_root)?;
    let paths = crate::context::bootstrap_config(req.dev_config, req.project_root, &pi.target_dir)?;

    let lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
        project: req.project.name(),
        command: "hotpath",
        project_root: &req.project_root.display().to_string(),
    })?;

    let binary = build_hotpath_example(effective_root, target, req.is_alloc(), req.features)?;

    let db_root = req.build_root.map(|_| req.project_root);
    let harness = harness::BenchHarness::new_with_lock(
        lock,
        &paths,
        effective_root,
        db_root,
        req.project,
        req.force,
        req.stop_marker.map(str::to_owned),
    )?;

    // The target name is the variant label (and also the cargo example name,
    // with "_bench" appended). The default "hotpath" example maps to "render"
    // for backwards compatibility.
    let variant_base = if target == "hotpath" {
        "render".to_owned()
    } else {
        target.to_owned()
    };

    run(
        &harness,
        &binary,
        req.runs(),
        req.is_alloc(),
        effective_root,
        &paths.scratch_dir,
        &variant_base,
    )
}
