//! Hotpath profiling for sluggrs: function-level timing and allocation instrumentation.
//!
//! Builds a cargo *example* target with the appropriate feature and runs it
//! through the bench harness to capture per-function timing and allocation data.
//!
//! The sluggrs `hotpath` example exercises two rendering paths:
//! - **cache-miss**: first frame with cold glyph cache (outline extraction +
//!   band building + texture upload)
//! - **cache-hit**: subsequent frames reusing cached glyphs (vertex buffer reuse)
//!
//! # Modes
//!
//! The command is named for its default, not its only mode. `--hotpath` /
//! `--alloc` build the example with the `hotpath` / `hotpath-alloc` feature and
//! capture a per-function report; `--bench` builds it **bare** and records the
//! wall clock. The bare walls are the ones worth comparing across commits -
//! instrumentation taxes every call it wraps, so an instrumented wall measures
//! the instrument as much as the renderer. A bare `brokkr hotpath` with no mode
//! flag stays `--hotpath 1`, which is what it has always meant.
//!
//! # `--commit`
//!
//! Unlike dellingr, which holds its Lua workload fixed while varying the VM,
//! everything sluggrs measures here *is* code: the example target and the
//! renderer it drives both come from the worktree. There is no asset to pin, so
//! `--commit` needs no split-tree rule - the whole subject is the old commit.

use std::path::Path;

use crate::build;
use crate::context::BenchContext;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::output;
use crate::project::{self, Project};

// ---------------------------------------------------------------------------
// Target naming
// ---------------------------------------------------------------------------

/// The cargo example target built for `--target NAME`.
///
/// The default `hotpath` target maps to the `hotpath` example directly; every
/// other target maps to a `{target}_bench` example (e.g. `email` →
/// `email_bench`).
fn example_name(target: &str) -> String {
    if target == "hotpath" {
        target.to_owned()
    } else {
        format!("{target}_bench")
    }
}

/// The `command` column rows are filed under.
///
/// The default `hotpath` target files under `render` for continuity with
/// historical rows, which predate `--target`.
fn command_label(target: &str) -> String {
    if target == "hotpath" {
        "render".to_owned()
    } else {
        target.to_owned()
    }
}

/// Whether this mode builds the example with hotpath instrumentation.
fn uses_hotpath(req: &MeasureRequest) -> bool {
    matches!(
        req.mode,
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
    )
}

/// Human-readable feature summary for `--dry-run` output.
fn feature_summary(req: &MeasureRequest) -> String {
    if uses_hotpath(req) {
        format!("--features {}", req.hotpath_features().join(","))
    } else if req.features.is_empty() {
        "no hotpath features".to_owned()
    } else {
        format!("--features {} (no hotpath features)", req.features.join(","))
    }
}

// ---------------------------------------------------------------------------
// Command entry point (called from cmd dispatch)
// ---------------------------------------------------------------------------

/// Top-level hotpath command for sluggrs.
///
/// Resolves the example target, builds it with the mode's features (in the
/// `--commit` worktree when there is one), and runs it through the bench
/// harness.
pub(crate) fn cmd(req: &MeasureRequest, target: &str) -> Result<(), DevError> {
    project::require(req.project, Project::Sluggrs, "hotpath")?;

    let example = example_name(target);
    let command_label = command_label(target);

    if req.dry_run {
        output::bench_msg(&format!(
            "[dry-run] target {target} -> --example {example}, filed as '{command_label}'"
        ));
        output::bench_msg(&format!("[dry-run] would build ({})", feature_summary(req)));
        return Ok(());
    }

    // `--bench` builds bare; the profiling modes prepend their instrument's
    // feature to the user/host features. See the module header.
    let features: Vec<String> = if uses_hotpath(req) {
        req.hotpath_features()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        req.features.to_vec()
    };

    let build_config = build::BuildConfig {
        package: None,
        bin: None,
        example: Some(example),
        features,
        default_features: true,
        profile: "release",
    };

    // `BenchContext` builds against `build_root` (the worktree under
    // `--commit`) while resolving data paths and the results DB against
    // `project_root`, and takes the global lock before the `cargo metadata`
    // call so a worktree's own toolchain pin is already disarmed.
    let ctx = BenchContext::with_build_config(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        &build_config,
        "hotpath",
        req.force,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let scratch_dir = ctx.paths.scratch_dir.clone();
    run(
        &ctx,
        req,
        req.runs(),
        req.effective_build_root(),
        &scratch_dir,
        &command_label,
    )
}

// ---------------------------------------------------------------------------
// Harness driver
// ---------------------------------------------------------------------------

/// Run the built example through the bench harness under `req`'s mode.
fn run(
    ctx: &BenchContext,
    req: &MeasureRequest,
    runs: usize,
    build_root: &Path,
    scratch_dir: &Path,
    command_label: &str,
) -> Result<(), DevError> {
    let binary_str = ctx
        .binary
        .to_str()
        .ok_or_else(|| DevError::Config("binary path is not valid UTF-8".into()))?
        .to_owned();

    std::fs::create_dir_all(scratch_dir)?;

    let config = BenchConfig {
        command: command_label.to_owned(),
        // Harness carries the measurement mode (bench/hotpath/alloc) and
        // the brokkr invocation - no need to set them here.
        mode: None,
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: build::CargoProfile::Release,
        runs,
        cli_args: Some(harness::format_cli_args(&binary_str, &[])),
        brokkr_args: None,
        metadata: vec![],
    };

    if uses_hotpath(req) {
        let label = harness::hotpath_feature(req.is_alloc());
        output::hotpath_msg(&format!("=== sluggrs {label}: {command_label} ==="));

        if req.is_alloc() {
            output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
        }

        ctx.harness.run_hotpath(&config, &ctx.binary, |_i| {
            let (result, _stderr, sidecar) = harness::run_hotpath_capture(
                &binary_str,
                &[],
                scratch_dir,
                build_root,
                &[],
                &[],
                req.stop_marker,
                Some(ctx.harness.lock()),
            )?;
            Ok((result, sidecar))
        })?;
    } else {
        output::bench_msg(&format!("sluggrs {command_label}: {runs} run(s)"));
        // The kv path, not the plain external one: sluggrs' examples
        // self-report `elapsed_ms=` on stderr covering only the measured
        // region. Brokkr's own wall would instead time the whole process,
        // which here is dominated by device init, shader compilation and font
        // loading - hundreds of milliseconds of setup around a single-digit
        // millisecond measurement. That wall is nearly constant, so it does
        // not just add noise, it hides the signal entirely.
        //
        // The trade is that `elapsed_ms=` becomes mandatory on stderr: a run
        // that omits it fails rather than being silently mistimed.
        ctx.harness
            .run_external_with_kv(&config, &ctx.binary, &[], build_root)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{command_label, example_name};

    #[test]
    fn default_target_keeps_its_historical_names() {
        // The example is `hotpath`, but rows have always been filed as
        // `render` - `brokkr results --command render` must keep working.
        assert_eq!(example_name("hotpath"), "hotpath");
        assert_eq!(command_label("hotpath"), "render");
    }

    #[test]
    fn other_targets_get_the_bench_suffix() {
        assert_eq!(example_name("email"), "email_bench");
        assert_eq!(command_label("email"), "email");
    }
}
