//! `brokkr dellingr` - build the Lua bench harness and run the selected
//! workloads.

use crate::build;
use crate::config::project_config::dellingr::DellingrConfig;
use crate::context::MeasurementSession;
use crate::error::DevError;
use crate::harness::{self, BenchConfig};
use crate::measure::MeasureRequest;
use crate::output;
use crate::project::{self, Project};

use super::workload;

/// Which workloads a run covers.
pub(crate) enum Selection {
    /// `--lua`: the names given, in order.
    Named(Vec<String>),
    /// `--all`: every registered workload the mode can run.
    All,
}

/// Run the selected registered workloads under the requested measurement
/// mode, one row each, against a single build of the harness.
pub(crate) fn run(req: &MeasureRequest, selection: &Selection) -> Result<(), DevError> {
    project::require(&req.project, &Project::Dellingr, "dellingr")?;

    let instrumented = req.mode.profiled().is_some();
    let (names, skipped) = selected(
        workload::config(req.project_config)?,
        selection,
        instrumented,
    )?;
    if !skipped.is_empty() {
        output::warn(&format!(
            "--all skips workloads without a hotpath variant: {}",
            skipped.join(", ")
        ));
    }

    // Resolve every workload before building: a typo'd `--lua` or a drifted
    // workload should cost nothing, and should not surface only after the
    // workloads ahead of it have run. The path deliberately comes from the
    // current tree even under `--commit` - see `workload::resolve`.
    // Instrumented modes resolve the workload's `hotpath_file` variant, not
    // `file`.
    let resolved = names
        .into_iter()
        .map(|name| {
            workload::resolve(
                req.project_config,
                req.project_root.as_path(),
                name,
                instrumented,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let example = workload::config(req.project_config)?.example.clone();

    if req.dry_run {
        let variant = if req.mode.profiled().is_some() {
            " (hotpath variant)"
        } else {
            ""
        };
        // `dry_run_msg`, not the quiet-gated `bench_msg`: `run_measured` sets
        // quiet unless `--verbose`, and a dry run's output is its whole point.
        for workload in &resolved {
            output::dry_run_msg(&format!(
                "workload {} -> {}{variant}",
                workload.name,
                workload.arg()?
            ));
        }
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

    // One workload keeps its own error; several run as variants, so one
    // failing does not skip the rest and the end names every one that failed.
    match resolved.as_slice() {
        [only] => run_one(req, &ctx, only),
        _ => {
            let names: Vec<&str> = resolved.iter().map(|w| w.name.as_str()).collect();
            crate::bench_variants::run_variants("workload", &names, |name| {
                let workload = resolved
                    .iter()
                    .find(|w| w.name == name)
                    .ok_or_else(|| DevError::Internal(format!("unresolved workload {name}")))?;
                run_one(req, &ctx, workload)
            })
        }
    }
}

/// The workload names a selection runs, and the ones `--all` skipped.
///
/// `--lua` names run in the order given, a repeated name kept once: running it
/// twice would only store a second row of the same measurement. A named
/// workload without a hotpath variant still refuses an instrumented run (in
/// `workload::resolve`), since naming it asked for it. `--all` asked for
/// whatever the mode can run, so in an instrumented mode it skips those
/// workloads instead, and refuses only when that leaves nothing.
fn selected<'a>(
    cfg: &'a DellingrConfig,
    selection: &'a Selection,
    instrumented: bool,
) -> Result<(Vec<&'a str>, Vec<&'a str>), DevError> {
    match selection {
        Selection::Named(lua) => {
            let mut names: Vec<&str> = Vec::new();
            for name in lua {
                if !names.contains(&name.as_str()) {
                    names.push(name);
                }
            }
            Ok((names, Vec::new()))
        }
        Selection::All => {
            let mut names: Vec<&str> = Vec::new();
            let mut skipped: Vec<&str> = Vec::new();
            for (name, entry) in &cfg.workloads {
                if instrumented && entry.hotpath_file.is_none() {
                    skipped.push(name);
                } else {
                    names.push(name);
                }
            }
            if names.is_empty() {
                let why = if cfg.workloads.is_empty() {
                    "no workloads are registered in [dellingr.workloads.*]"
                } else {
                    "no registered workload has a hotpath variant \
                     (hotpath_file / hotpath_xxh128)"
                };
                return Err(DevError::Config(format!("--all has nothing to run: {why}")));
            }
            Ok((names, skipped))
        }
    }
}

/// Run one resolved workload against the session's harness build.
fn run_one(
    req: &MeasureRequest,
    ctx: &MeasurementSession,
    resolved: &workload::Resolved,
) -> Result<(), DevError> {
    // The registered relative path, not the absolute one: the harness runs
    // from the project root (both paths below pass it as the working
    // directory, under `--commit` too, where only the binary comes from the
    // worktree), and uses this string as the chunk name and `target=` value.
    let binary_str = ctx.binary()?.display().to_string();
    let args = [resolved.arg()?];
    let config = bench_config(resolved, &binary_str, &args, req.runs())?;

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

    /// A registry of `(name, has a hotpath variant)` workloads.
    fn registry(workloads: &[(&str, bool)]) -> DellingrConfig {
        let digest: crate::preflight::FileDigest =
            "51b15378d6e75699f24b1fd2d025b30a".parse().unwrap();
        DellingrConfig {
            example: "hotpath".into(),
            workloads: workloads
                .iter()
                .map(|&(name, hotpath)| {
                    let entry = crate::config::project_config::dellingr::DellingrWorkload {
                        file: format!("bench/{name}.lua").into(),
                        xxh128: digest,
                        hotpath_file: hotpath.then(|| format!("examples/{name}.lua").into()),
                        hotpath_xxh128: hotpath.then_some(digest),
                    };
                    (name.to_owned(), entry)
                })
                .collect(),
        }
    }

    #[test]
    fn a_named_selection_keeps_the_given_order_and_each_name_once() {
        let cfg = registry(&[]);
        let lua = Selection::Named(
            ["sort", "method", "sort", "calls", "method"]
                .iter()
                .map(|&s| s.to_owned())
                .collect(),
        );
        // Unknown names pass through: `workload::resolve` refuses them with
        // the registered list.
        let (names, skipped) = selected(&cfg, &lua, true).unwrap();
        assert_eq!(names, ["sort", "method", "calls"]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn all_runs_every_workload_and_skips_bench_only_ones_when_instrumented() {
        let cfg = registry(&[("sort", true), ("calls", false), ("method", true)]);
        let (names, skipped) = selected(&cfg, &Selection::All, false).unwrap();
        assert_eq!(names, ["calls", "method", "sort"]);
        assert!(skipped.is_empty());

        let (names, skipped) = selected(&cfg, &Selection::All, true).unwrap();
        assert_eq!(names, ["method", "sort"]);
        assert_eq!(skipped, ["calls"]);
    }

    #[test]
    fn all_refuses_when_nothing_is_left_to_run() {
        assert!(selected(&registry(&[]), &Selection::All, false).is_err());
        let bench_only = registry(&[("calls", false)]);
        assert!(selected(&bench_only, &Selection::All, false).is_ok());
        let err = selected(&bench_only, &Selection::All, true).unwrap_err();
        assert!(err.to_string().contains("hotpath variant"), "{err}");
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
        let hotpath = resolved(
            "examples/calls/method.lua",
            "6372dbfbd4aea331f0642f73b0662aa5",
        );
        let regenerated = resolved("bench/method.lua", "51b15378000000000000000000000000");
        let inputs: Vec<String> = [&bench, &hotpath, &regenerated]
            .iter()
            .map(|r| bench_config(r, "h", &[], 1).unwrap().input_file.unwrap())
            .collect();
        assert_ne!(inputs[0], inputs[1]);
        assert_ne!(inputs[0], inputs[2]);
    }
}
