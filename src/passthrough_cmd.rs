//! Executable passthrough and timing statistics.
use crate::{
    build, config,
    context::{bootstrap, bootstrap_config},
    error::DevError,
    lockfile, output,
    project::{self, Project},
};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

pub(crate) struct RunOptions {
    pub(crate) time: bool,
    pub(crate) json: bool,
    pub(crate) runs: NonZeroUsize,
    pub(crate) no_build: bool,
}

pub(crate) struct RunStats {
    pub(crate) min_ms: u64,
    pub(crate) median_ms: u64,
    pub(crate) p95_ms: u64,
}

pub(crate) fn duration_ms(duration: Duration) -> u64 {
    let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    if ms == 0 && !duration.is_zero() {
        1
    } else {
        ms
    }
}

pub(crate) fn summarize_runs(samples_ms: &[u64]) -> Result<RunStats, DevError> {
    if samples_ms.is_empty() {
        return Err(DevError::Usage("run requires at least one sample".into()));
    }

    let mut sorted = samples_ms.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let min_ms = sorted[0];
    let median_ms = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        let a = sorted[(n / 2) - 1];
        let b = sorted[n / 2];
        a.saturating_add(b) / 2
    };
    let p95_rank = (95 * n).div_ceil(100);
    let p95_index = p95_rank.saturating_sub(1);
    let p95_ms = sorted[p95_index];

    Ok(RunStats {
        min_ms,
        median_ms,
        p95_ms,
    })
}

#[allow(clippy::print_stdout)] // Data output; preserve its bytes and stdout stream.
pub(crate) fn print_run_timing(
    opts: &RunOptions,
    build_ms: u64,
    run_ms: u64,
    samples_ms: &[u64],
) -> Result<(), DevError> {
    let runs = opts.runs.get();
    let elapsed_ms = build_ms.saturating_add(run_ms);
    let stats = summarize_runs(samples_ms)?;

    if opts.json {
        if runs == 1 {
            println!(
                "{}",
                serde_json::json!({
                    "build_ms": build_ms,
                    "run_ms": run_ms,
                    "elapsed_ms": elapsed_ms,
                })
            );
        } else {
            println!(
                "{}",
                serde_json::json!({
                    "build_ms": build_ms,
                    "run_ms": run_ms,
                    "elapsed_ms": elapsed_ms,
                    "runs": runs,
                    "min_ms": stats.min_ms,
                    "median_ms": stats.median_ms,
                    "p95_ms": stats.p95_ms,
                    "run_samples_ms": samples_ms,
                })
            );
        }
        return Ok(());
    }

    if opts.time {
        if runs == 1 {
            println!("elapsed_ms={elapsed_ms} build_ms={build_ms} run_ms={run_ms}");
        } else {
            println!(
                "elapsed_ms={elapsed_ms} build_ms={build_ms} run_ms={run_ms} runs={} min_ms={} median_ms={} p95_ms={}",
                runs, stats.min_ms, stats.median_ms, stats.p95_ms,
            );
        }
        return Ok(());
    }

    if runs > 1 {
        output::run_msg(&format!(
            "runs={} min={}ms median={}ms p95={}ms total={}ms",
            runs, stats.min_ms, stats.median_ms, stats.p95_ms, run_ms,
        ));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_run(
    dev_config: &config::DevConfig,
    project: &Project,
    project_root: &project::ProjectRoot,
    build_root: &project::BuildRoot,
    features: &[String],
    args: &[String],
    opts: &RunOptions,
    lock: Option<&lockfile::LockGuard>,
) -> Result<(), DevError> {
    let runs = opts.runs.get();

    let package = project.cli_package();
    let feature_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let build_config = if feature_refs.is_empty() {
        build::BuildConfig::release(package)
    } else {
        build::BuildConfig::release_with_features(package, &feature_refs)
    };
    let build_start = Instant::now();
    let binary = if opts.no_build {
        build::resolve_existing_binary(&build_config, build_root.as_path())?
    } else {
        build::cargo_build(&build_config, build_root.as_path())?.path
    };
    let build_ms = if opts.no_build {
        0
    } else {
        duration_ms(build_start.elapsed())
    };

    // Ensure scratch dir exists - binary commands often write output there.
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    std::fs::create_dir_all(&paths.scratch_dir)?;

    let mut run_total = Duration::ZERO;
    let mut samples_ms = Vec::with_capacity(runs);

    for idx in 0..runs {
        if runs > 1 {
            output::run_msg(&format!("run {}/{}", idx + 1, runs));
        }
        let binary_str = binary.display().to_string();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        output::run_msg(&format!("{binary_str} {}", args.join(" ")));
        let out = output::run_passthrough_timed(&binary_str, &arg_refs, lock)?;
        if let Some(code) = out.failed_code(crate::output::child::Exits::SUCCESS) {
            return Err(DevError::ExitCode(code));
        }
        let run_elapsed = out.elapsed;
        run_total += run_elapsed;
        samples_ms.push(duration_ms(run_elapsed));
    }

    print_run_timing(opts, build_ms, duration_ms(run_total), &samples_ms)?;

    Ok(())
}
