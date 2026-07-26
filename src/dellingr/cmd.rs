//! `brokkr dellingr` - build the Lua bench harness and run one workload.

use crate::build;
use crate::context::BenchContext;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::{MeasureMode, MeasureRequest};
use crate::output;
use crate::project::{self, Project};

use super::workload;

/// Scratch dir for the harness's hotpath report and marker FIFO.
///
/// Under `.brokkr/` rather than the configured `scratch_dir`, whose default
/// (`data/scratch`) would create a `data/` tree in a project that has no data.
/// Everything here is brokkr's own leavings, which is what `brokkr clean`
/// expects of a `.brokkr/<project>` dir.
const SCRATCH_REL: &str = ".brokkr/dellingr";

/// Run one registered workload under the requested measurement mode.
pub(crate) fn run(req: &MeasureRequest, lua: &str) -> Result<(), DevError> {
    project::require(req.project, Project::Dellingr, "dellingr")?;

    // Resolve before building: a typo'd `--lua` or a drifted workload should
    // cost nothing. The path deliberately comes from the current tree even
    // under `--commit` - see `workload::resolve`. Instrumented modes resolve
    // the workload's `hotpath_file` variant, not `file`.
    let resolved = workload::resolve(req.dev_config, req.project_root, lua, uses_hotpath(req))?;
    let example = workload::config(req.dev_config)?.example.clone();
    let lua_path = resolved.path_str()?.to_owned();

    if req.dry_run {
        let variant = if uses_hotpath(req) {
            " (hotpath variant)"
        } else {
            ""
        };
        output::bench_msg(&format!("[dry-run] workload {lua} -> {lua_path}{variant}"));
        output::bench_msg(&format!(
            "[dry-run] would build --example {example} ({})",
            feature_summary(req)
        ));
        return Ok(());
    }

    // `--bench` builds the harness bare: its walls are the ones worth
    // comparing, and hotpath instrumentation would tax every call it wraps.
    // The profiling modes add the standard feature for their instrument.
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

    let ctx = BenchContext::with_build_config(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        &build_config,
        "dellingr",
        req.force,
        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let scratch_dir = req.project_root.join(SCRATCH_REL);
    std::fs::create_dir_all(&scratch_dir)?;

    let binary_str = ctx.binary.display().to_string();
    let args = [lua_path.as_str()];

    // Filed under the workload name, not `dellingr`: one row series per
    // workload is what makes `--command`, `--compare` and the per-iteration
    // view answer the question anyone actually asks of this data. `input_file`
    // stays empty - the workload is the operation here, not a dataset, and
    // duplicating the name into that column would only widen the table.
    let config = BenchConfig {
        command: resolved.name.clone(),
        mode: None,
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: build::CargoProfile::Release,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &args)),
        brokkr_args: None,
        metadata: vec![],
    };

    if uses_hotpath(req) {
        let label = harness::hotpath_feature(req.is_alloc());
        output::hotpath_msg(&format!("=== dellingr {label}: {} ===", resolved.name));
        if req.is_alloc() {
            output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
        }

        ctx.harness.run_hotpath(&config, &ctx.binary, |_i| {
            let (result, _stderr, sidecar) = harness::run_hotpath_capture(
                &binary_str,
                &args,
                &scratch_dir,
                req.project_root,
                &[],
                &[],
                req.stop_marker,
                Some(ctx.harness.lock()),
            )?;
            Ok((result, sidecar))
        })?;
    } else {
        output::bench_msg(&format!(
            "dellingr workload {}: {} run(s)",
            resolved.name,
            req.runs()
        ));
        ctx.harness
            .run_external(&config, &ctx.binary, &args, req.project_root)?;
    }

    Ok(())
}

/// Whether this mode builds the harness with hotpath instrumentation.
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
    } else {
        "no hotpath features".to_owned()
    }
}
