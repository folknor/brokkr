//! Sampling profiler support for elivagar.
//!
//! Replaces `run-perf.sh` and `run-samply.sh`. Builds with `--profile profiling`
//! (release + debug symbols), then runs under `perf record` or `samply record`.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::git;
use crate::output;


// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run elivagar under a sampling profiler.
///
/// `tool` must be `"perf"` or `"samply"`.
pub fn run(
    pbf_path: &Path,
    data_dir: &Path,
    scratch_dir: &Path,
    tool: &str,
    no_ocean: bool,
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

    // Check perf_event_paranoid and tool availability before acquiring lock.
    crate::preflight::run_preflight(&crate::preflight::profile_checks(tool))?;

    // Acquire exclusive lock to prevent conflicts with concurrent benchmarks.
    let _lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
        project: "elivagar",
        command: "profile",
        project_root: project_root.to_str().unwrap_or("unknown"),
    })?;

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    // Build with profiling profile.
    output::build_msg("building with profiling profile (release + debug symbols)");

    let config = build::BuildConfig {
        package: None,
        bin: None,
        example: None,
        features: Vec::new(),
        default_features: true,
        profile: "profiling",
    };
    let binary = build::cargo_build(&config, project_root)?;
    let binary_str = binary.display().to_string();

    std::fs::create_dir_all(scratch_dir)?;
    let output_pmtiles = scratch_dir.join("profile-output.pmtiles");
    let output_pmtiles_str = output_pmtiles.display().to_string();

    let tmp_dir = data_dir.join("tilegen_tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let tmp_dir_str = tmp_dir.display().to_string();

    // Build elivagar argument list.
    let mut elivagar_args: Vec<String> = vec![
        pbf_str.into(),
        output_pmtiles_str,
        "--tmp-dir".into(),
        tmp_dir_str,
    ];

    // Ocean flags.
    super::push_ocean_args(&mut elivagar_args, data_dir, no_ocean);

    // Collect git info for naming.
    let git_info = git::collect(project_root)?;
    let hostname = crate::config::hostname()?;

    match tool {
        "perf" => crate::profiler::run_perf(
            &binary_str,
            &elivagar_args,
            &hostname,
            &git_info.commit,
            data_dir,
            project_root,
        ),
        "samply" => crate::profiler::run_samply(
            &binary_str,
            &elivagar_args,
            &hostname,
            &git_info.commit,
            data_dir,
            project_root,
        ),
        _ => unreachable!(),
    }?;

    // Clean up output PMTiles.
    std::fs::remove_file(output_pmtiles).ok();

    Ok(())
}
