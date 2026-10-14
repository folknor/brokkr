//! `brokkr mogwai` - build a mogwai surface and measure one invocation.

use crate::context::MeasurementSession;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::MeasureRequest;
use crate::output;
use crate::project::{self, Project};

use super::targets;

/// Build and run one invocation. `target` names a harness; `None` is the CLI.
pub(crate) fn run(
    req: &MeasureRequest,
    target: Option<&str>,
    args: &[String],
) -> Result<(), DevError> {
    project::require(&req.project, &Project::Mogwai, "mogwai")?;

    let cfg = targets::config(req.project_config)?;

    // Bare-is-an-index, but only when there is genuinely nothing to run. An
    // argv with no target is the CLI surface, which is the common case and
    // must not be mistaken for "show me the list".
    if target.is_none() && args.is_empty() {
        // Ungated, like the body below: `run_measured` sets quiet unless
        // `--verbose`, and the quiet-gated `bench_msg` dropped the header
        // while the raw `print!` body still printed.
        output::run_msg("mogwai surfaces:");
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
    let mode_features: Vec<String> = req
        .build_features()
        .into_iter()
        .map(str::to_owned)
        .collect();

    let resolved = targets::resolve(cfg, target, &mode_features)?;
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();

    if req.dry_run {
        // Not quiet-gated: a dry run's output is its whole point.
        output::dry_run_msg(&format!("{} -> {}", resolved.name, args.join(" ")));
        return Ok(());
    }

    let ctx = MeasurementSession::with_build_config_for_request(req, &resolved.build)?;

    let binary_str = ctx.binary()?.display().to_string();

    // Rows file under the target name (or the bin name for a CLI invocation)
    // and carry the exact invocation. Comparison uses its parsed arm_key,
    // which excludes commit provenance, verbosity, and iteration count while
    // retaining measurement-affecting options. `--grep` and `--grep-v` still
    // query the full invocation, including arms defined by an absent flag.
    let config = BenchConfig {
        command: resolved.name.clone(),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        external_build: None,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &argv)),
        metadata: vec![],
    };

    crate::measure_dispatch::dispatch_profiled(
        req,
        "mogwai",
        &resolved.name,
        || {
            // The profiling runner writes and reads the hotpath report as
            // well as timing the child.
            ctx.harness.run_hotpath(&config, ctx.binary()?, |i, fifo| {
                ctx.harness.capture_hotpath(
                    ctx.binary()?,
                    &argv,
                    req.project_root.as_path(),
                    &[],
                    &[],
                    fifo,
                    i,
                    None,
                )
            })
        },
        || {
            // External wall-clock, with the stderr counters that ride along
            // for free. Markers keep the excluded setup visible as its own
            // phase instead of deleting it from the record.
            output::bench_msg(&format!("mogwai {}: {} run(s)", resolved.name, config.runs));
            ctx.harness
                .run_external(&config, ctx.binary()?, &argv, req.project_root.as_path())
        },
    )?;

    Ok(())
}
