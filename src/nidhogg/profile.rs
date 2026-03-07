//! Sampling profiler support for nidhogg.
//!
//! Builds with `--profile profiling` (release + debug symbols), then runs
//! under `perf record` or `samply record`. Results (elapsed time) are stored
//! in `.brokkr/results.db`.

use std::path::Path;

use crate::build;
use crate::db::KvPair;
use crate::error::DevError;
use crate::harness::{BenchConfig, BenchHarness, BenchResult};
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run nidhogg under a sampling profiler.
///
/// `tool` must be `"perf"` or `"samply"`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    harness: &BenchHarness,
    pbf_path: &Path,
    file_mb: f64,
    data_dir: &Path,
    scratch_dir: &Path,
    tool: &str,
    extra_features: &[String],
    project_root: &Path,
) -> Result<(), DevError> {
    match tool {
        "perf" | "samply" => {}
        other => {
            return Err(DevError::Config(format!(
                "unknown profiling tool '{other}' (expected: perf, samply)"
            )));
        }
    }

    let pbf_str = super::client::path_str(pbf_path)?;

    let basename = pbf_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();

    // Build with profiling profile.
    output::build_msg("building with profiling profile (release + debug symbols)");

    let config = build::BuildConfig {
        package: Some("nidhogg".into()),
        bin: None,
        example: None,
        features: extra_features.to_vec(),
        default_features: true,
        profile: "profiling",
    };
    let binary = build::cargo_build(&config, project_root)?;
    let binary_str = binary.display().to_string();

    std::fs::create_dir_all(scratch_dir)?;
    let output_dir = scratch_dir.join("profile-output");
    let output_str = output_dir.display().to_string();

    // Build nidhogg argument list: nidhogg ingest <pbf> <output_dir>
    let nidhogg_args: Vec<String> = vec![
        "ingest".into(),
        pbf_str.into(),
        output_str,
    ];

    // Collect git info for profiler output naming.
    let git_info = crate::git::collect(project_root)?;
    let hostname = crate::config::hostname()?;

    let (elapsed_ms, _raw_stderr) = match tool {
        "perf" => crate::profiler::run_perf(
            &binary_str,
            &nidhogg_args,
            &hostname,
            &git_info.commit,
            data_dir,
            project_root,
        ),
        "samply" => crate::profiler::run_samply(
            &binary_str,
            &nidhogg_args,
            &hostname,
            &git_info.commit,
            data_dir,
            project_root,
        ),
        _ => unreachable!(),
    }?;

    // Store result in DB.
    let bench_config = BenchConfig {
        command: "profile".into(),
        variant: Some(tool.into()),
        input_file: Some(basename),
        input_mb: Some(file_mb),
        cargo_features: None,
        cargo_profile: "profiling".into(),
        runs: 1,
        cli_args: None,
        metadata: vec![KvPair::text("meta.tool", tool)],
    };

    let result = BenchResult {
        elapsed_ms,
        kv: vec![],
        distribution: None,
        hotpath: None,
    };

    harness.record_result(&bench_config, &result)?;

    // Clean up output directory.
    std::fs::remove_dir_all(output_dir).ok();

    Ok(())
}
