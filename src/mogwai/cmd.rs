//! `brokkr mogwai` - build a mogwai surface and measure one invocation.

use crate::context::BenchContext;
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

    let resolved = targets::resolve(cfg, target, req.features)?;
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

    output::bench_msg(&format!(
        "mogwai {}: {} run(s)",
        resolved.name, config.runs
    ));

    // External wall-clock, with the stderr counters that ride along for free.
    // Phase decomposition is the sidecar's job, not a second definition of
    // what `elapsed` means: markers keep the excluded setup visible as its own
    // phase instead of deleting it from the record, which is what a
    // self-reported window did.
    ctx.harness
        .run_external(&config, &ctx.binary, &argv, req.project_root)?;

    Ok(())
}
