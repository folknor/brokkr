//! `brokkr mogwai` - build a mogwai surface and measure one invocation.

use crate::context::BenchContext;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::output;
use crate::project::{self, Project};

use super::targets;

/// Scratch dir for the harness's hotpath report and marker FIFO.
///
/// Under `.brokkr/` rather than the configured `scratch_dir`, whose default
/// (`data/scratch`) would create a `data/` tree in a project that has none.
const SCRATCH_REL: &str = ".brokkr/mogwai";

/// Build and run one invocation. `target` names a harness; `None` is the CLI.
pub(crate) fn run(
    req: &MeasureRequest,
    target: Option<&str>,
    args: &[String],
) -> Result<(), DevError> {
    project::require(req.project, Project::Mogwai, "mogwai")?;

    let cfg = targets::config(req.dev_config)?;

    // Bare-is-an-index, but only when there is genuinely nothing to run. An
    // argv with no target is the CLI surface, which is the common case and
    // must not be mistaken for "show me the list".
    if target.is_none() && args.is_empty() {
        output::bench_msg("mogwai surfaces:");
        print!("{}", targets::format_index(cfg));
        return Ok(());
    }

    // An instrumented mode contributes its own feature on top of whatever the
    // target registered. `hotpath` is what compiles the annotations in at all,
    // and `hotpath-alloc` is additionally required for `--alloc` - alloc alone
    // tracks nothing. Union, not replacement: the registered list is what makes
    // the target buildable (an instrumented example commonly carries
    // `required-features`), so dropping it under `--bench` would fail the build
    // rather than produce a leaner one.
    let mode_features: Vec<String> = if uses_hotpath(req) {
        req.hotpath_features()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        req.features.to_vec()
    };

    let resolved = targets::resolve(cfg, target, &mode_features)?;
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    if req.dry_run {
        output::bench_msg(&format!(
            "[dry-run] {} -> {}",
            resolved.name,
            args.join(" ")
        ));
        return Ok(());
    }

    let ctx = BenchContext::with_build_config(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        &resolved.build,
        "mogwai",
        req.force,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let binary_str = ctx.binary.display().to_string();

    // Rows file under the target name (or the bin name for a CLI invocation)
    // and carry the invocation VERBATIM. Nothing is kept out of the pairing
    // key: `brokkr_args` is a `pair_key` component precisely so that two
    // different invocations do not average into one row, and an invocation
    // that is not comparable is visible in the row rather than prevented by a
    // config constraint. Selecting an arm - including the arm defined by an
    // absent flag - is what `--grep` and `--grep-v` are for.
    let config = BenchConfig {
        command: resolved.name.clone(),
        mode: None,
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &argv)),
        brokkr_args: None,
        metadata: vec![],
    };

    // The instrumented modes need their own runner, not just their own build.
    // `run_external` measures a wall and stores nothing else, so a `--hotpath`
    // run built with every right feature still records a row with no profile in
    // it - the hotpath report is written to a file the child is told about, and
    // something has to set that up and read it back.
    if uses_hotpath(req) {
        let scratch_dir = req.project_root.join(SCRATCH_REL);
        std::fs::create_dir_all(&scratch_dir)?;

        let label = harness::hotpath_feature(req.is_alloc());
        output::hotpath_msg(&format!("=== mogwai {label}: {} ===", resolved.name));
        if req.is_alloc() {
            output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
        }

        ctx.harness.run_hotpath(&config, &ctx.binary, |_i| {
            let (result, _stderr, sidecar) = harness::run_hotpath_capture(
                &binary_str,
                &argv,
                &scratch_dir,
                req.project_root,
                &[],
                &[],
                req.stop_marker,
                Some(ctx.harness.lock()),
            )?;
            Ok((result, sidecar))
        })?;
        return Ok(());
    }

    output::bench_msg(&format!(
        "mogwai {}: {} run(s)",
        resolved.name, config.runs
    ));

    // External wall-clock, with the stderr counters that ride along for free.
    // Phase decomposition is the sidecar's job, not a second definition of
    // what `elapsed` means: markers keep the excluded setup visible as its own
    // phase instead of deleting it from the record.
    ctx.harness
        .run_external(&config, &ctx.binary, &argv, req.project_root)?;

    Ok(())
}

/// Whether this mode builds and runs with hotpath instrumentation.
fn uses_hotpath(req: &MeasureRequest) -> bool {
    matches!(
        req.mode,
        MeasureMode::Hotpath { .. } | MeasureMode::Alloc { .. }
    )
}
