//! `brokkr dellingr` - build the Lua bench harness and run one workload.

use crate::build;
use crate::context::MeasurementSession;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::MeasureRequest;
use crate::output;
use crate::project::{self, Project};

use super::workload;

/// Run one registered workload under the requested measurement mode.
pub(crate) fn run(req: &MeasureRequest, lua: &str) -> Result<(), DevError> {
    project::require(&req.project, &Project::Dellingr, "dellingr")?;

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
    // The registered relative path, not the absolute one: the harness runs
    // from the project root (both paths below pass it as the working
    // directory, under `--commit` too, where only the binary comes from the
    // worktree), and uses this string as the chunk name and `target=` value.
    let lua_path = resolved.arg()?.to_owned();

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
        profile: crate::build::RustProfile::Release,
    };

    let ctx = MeasurementSession::with_build_config_for_request(req, &build_config)?;

    let binary_str = ctx.binary()?.display().to_string();
    let args = [lua_path.as_str()];
    let config = bench_config(&resolved, &binary_str, &args, req.runs())?;

    crate::measure_dispatch::dispatch_profiled(
        req,
        "dellingr",
        &resolved.name,
        || {
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
            })
        },
        || {
            output::bench_msg(&format!(
                "dellingr workload {}: {} run(s)",
                resolved.name,
                req.runs()
            ));
            let invocation = crate::measure::Invocation::new(
                ctx.binary()?,
                args.iter().map(|&a| a.to_owned()).collect(),
            );
            ctx.harness
                .run_external(&config, &invocation, req.project_root.as_path())
        },
    )?;

    Ok(())
}

/// The row configuration for one workload run.
///
/// Filed under the workload name, not `dellingr`: one row series per workload
/// is what makes `--command`, `--compare` and the per-iteration view answer the
/// question anyone actually asks of this data. `input_file` is the file that
/// ran and its verified digest: the bench and hotpath variants are different
/// files, and a re-registered workload is a new generation, so neither pairs
/// with rows of the other in a comparison.
fn bench_config(
    resolved: &workload::Resolved,
    binary: &str,
    args: &[&str],
    runs: usize,
) -> Result<BenchConfig, DevError> {
    Ok(BenchConfig {
        command: resolved.name.clone(),
        input_file: Some(resolved.input_file()?),
        input_mb: None,
        cargo_features: None,
        external_build: None,
        runs,
        cli_args: Some(harness::format_cli_args(binary, args)),
        metadata: vec![],
    })
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn resolved(file: &str, xxh128: &str) -> workload::Resolved {
        workload::Resolved {
            name: "method".into(),
            registered: file.into(),
            xxh128: xxh128.into(),
        }
    }

    #[test]
    fn the_row_names_the_registered_file_and_its_generation() {
        let bench = resolved("bench/method.lua", "51b15378d6e75699f24b1fd2d025b30a");
        let args = [bench.arg().unwrap()];
        let config = bench_config(&bench, "/t/release/examples/hotpath", &args, 3).unwrap();
        assert_eq!(config.command, "method");
        assert_eq!(
            config.input_file.as_deref(),
            Some("bench/method.lua@51b15378d6e75699f24b1fd2d025b30a")
        );
        // The harness is handed the registered path, never an absolute one.
        assert!(config.cli_args.unwrap().ends_with(" bench/method.lua"));

        // The hotpath variant and a re-registered generation are each a
        // different input, so none of the three rows pair with another.
        let hotpath = resolved("examples/calls/method.lua", "6372dbfbd4aea331f0642f73b0662aa5");
        let regenerated = resolved("bench/method.lua", "51b15378000000000000000000000000");
        let inputs: Vec<String> = [&bench, &hotpath, &regenerated]
            .iter()
            .map(|r| bench_config(r, "h", &[], 1).unwrap().input_file.unwrap())
            .collect();
        assert_ne!(inputs[0], inputs[1]);
        assert_ne!(inputs[0], inputs[2]);
    }
}
