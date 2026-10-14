//! Generic instrumented measurement.
use crate::{context, error::DevError, harness, measure, output};

pub(crate) fn cmd_hotpath_generic(req: &measure::MeasureRequest) -> Result<(), DevError> {
    let profiling = req
        .mode
        .profiled()
        .ok_or_else(|| DevError::Config("generic-hotpath requires a profiling mode".into()))?;
    let hotpath_features = req.build_features();
    let ctx = context::MeasurementSession::build_for_request(
        req,
        req.project.cli_package(),
        &hotpath_features,
        true,
    )?;

    let label = profiling.feature();
    output::detail(&format!("{} {label}", req.project));
    profiling.print_note();

    let binary_str = ctx.binary()?.display().to_string();

    let config = harness::BenchConfig {
        command: "default".into(),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        external_build: None,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &[])),
        metadata: vec![],
    };

    ctx.harness.run_hotpath(&config, ctx.binary()?, |i, fifo| {
        ctx.harness.capture_hotpath(
            ctx.binary()?,
            &[],
            req.project_root.as_path(),
            &[],
            &[],
            fifo,
            i,
            None,
        )
    })?;

    Ok(())
}
