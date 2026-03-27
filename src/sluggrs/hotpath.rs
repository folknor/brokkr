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
use crate::config;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{self, BenchConfig, BenchHarness};
use crate::output;

// ---------------------------------------------------------------------------
// Build helper
// ---------------------------------------------------------------------------

/// Build the `hotpath` example with the appropriate hotpath feature.
fn build_hotpath_example(
    project_root: &Path,
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

    let config = build::BuildConfig {
        package: None,
        bin: None,
        example: Some("hotpath".into()),
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
) -> Result<(), DevError> {
    let binary_str = binary
        .to_str()
        .ok_or_else(|| DevError::Config("binary path is not valid UTF-8".into()))?;

    std::fs::create_dir_all(scratch_dir)?;

    let label = harness::hotpath_feature(alloc);
    output::hotpath_msg(&format!("=== sluggrs {label} ==="));

    if alloc {
        output::hotpath_msg(
            "NOTE: alloc profiling -- wall-clock times are not meaningful",
        );
    }

    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("render{variant_suffix}");

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
        let (result, _stderr) =
            harness::run_hotpath_capture(binary_str, &[], scratch_dir, project_root, &[])?;
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd(
    dev_config: &config::DevConfig,
    project: crate::project::Project,
    project_root: &Path,
    build_root: Option<&Path>,
    runs: usize,
    alloc: bool,
    force: bool,
    features: &[String],
) -> Result<(), DevError> {
    let effective_root = build_root.unwrap_or(project_root);
    let pi = crate::context::bootstrap(build_root)?;
    let paths = crate::context::bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
        project: project.name(),
        command: "hotpath",
        project_root: &project_root.display().to_string(),
    })?;

    let binary = build_hotpath_example(effective_root, alloc, features)?;

    let db_root = build_root.map(|_| project_root);
    let harness = harness::BenchHarness::new_with_lock(
        lock, &paths, effective_root, db_root, project, force,
    )?;

    run(&harness, &binary, runs, alloc, effective_root, &paths.scratch_dir)
}
