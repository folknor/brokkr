//! `brokkr dellingr` - build the Lua bench harness and run one workload.

use crate::build;
use crate::clean::layout::ProjectState;
use crate::context::MeasurementSession;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::MeasureRequest;
use crate::output;
use crate::project::{self, Project};

use super::workload;

/// Run one registered workload under the requested measurement mode.
pub(crate) fn run(req: &MeasureRequest, lua: &str) -> Result<(), DevError> {
    project::require(req.project, Project::Dellingr, "dellingr")?;

    // Resolve before building: a typo'd `--lua` or a drifted workload should
    // cost nothing. The path deliberately comes from the current tree even
    // under `--commit` - see `workload::resolve`. Instrumented modes resolve
    // the workload's `hotpath_file` variant, not `file`.
    let resolved = workload::resolve(
        req.project_config,
        req.project_root.as_path(),
        lua,
        req.mode.profiled().is_some(),
    )?;
    let example = workload::config(req.project_config)?.example.clone();
    let lua_path = resolved.path_str()?.to_owned();

    if req.dry_run {
        let variant = if req.mode.profiled().is_some() {
            " (hotpath variant)"
        } else {
            ""
        };
        // `dry_run_msg`, not the quiet-gated `bench_msg`: `run_measured` sets
        // quiet unless `--verbose`, and a dry run's output is its whole point.
        output::dry_run_msg(&format!("workload {lua} -> {lua_path}{variant}"));
        output::dry_run_msg(&format!(
            "would build --example {example} ({})",
            feature_summary(req)
        ));
        return Ok(());
    }

    // `--bench` builds the harness bare: its walls are the ones worth
    // comparing, and hotpath instrumentation would tax every call it wraps.
    // The profiling modes add the standard feature for their instrument.
    let features: Vec<String> = req
        .build_features()
        .into_iter()
        .map(str::to_owned)
        .collect();

    let build_config = build::BuildConfig {
        package: None,
        bin: None,
        example: Some(example),
        features,
        default_features: true,
        all_features: false,
        profile: "release",
    };

    let ctx = MeasurementSession::with_build_config_for_request(req, &build_config)?;

    let state = ProjectState::new(req.project_root.as_path());
    let scratch_dir = state.dellingr().scratch().to_path_buf();
    std::fs::create_dir_all(&scratch_dir)?;

    let binary_str = ctx.binary()?.display().to_string();
    let args = [lua_path.as_str()];

    // Filed under the workload name, not `dellingr`: one row series per
    // workload is what makes `--command`, `--compare` and the per-iteration
    // view answer the question anyone actually asks of this data. `input_file`
    // stays empty - the workload is the operation here, not a dataset, and
    // duplicating the name into that column would only widen the table.
    let config = BenchConfig {
        command: resolved.name.clone(),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        external_build: None,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &args)),
        metadata: vec![],
    };

    if req.mode.profiled().is_some() {
        let label = req
            .mode
            .profiled()
            .map_or("", crate::measure::ProfiledMode::feature);
        output::detail(&format!("dellingr {label}: {}", resolved.name));
        if let Some(profiling) = req.mode.profiled() {
            profiling.print_note();
        }

        ctx.harness.run_hotpath(&config, ctx.binary()?, |i, fifo| {
            ctx.harness.capture_hotpath(
                ctx.binary()?,
                &args,
                req.project_root.as_path(),
                &[],
                &[],
                fifo,
                i,
                None,
            )
        })?;
    } else {
        output::bench_msg(&format!(
            "dellingr workload {}: {} run(s)",
            resolved.name,
            req.runs()
        ));
        ctx.harness
            .run_external(&config, ctx.binary()?, &args, req.project_root.as_path())?;
    }

    Ok(())
}

/// Human-readable feature summary for `--dry-run` output.
fn feature_summary(req: &MeasureRequest) -> String {
    let features = req.build_features();
    if features.is_empty() {
        "no cargo features".to_owned()
    } else {
        format!("--features {}", features.join(","))
    }
}
