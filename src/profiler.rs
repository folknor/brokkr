//! Shared sampling profiler support (perf, samply).
//!
//! Contains the tool-specific `run_perf` and `run_samply` functions used by
//! both `elivagar/profile.rs` and `nidhogg/profile.rs`.

use std::path::Path;

use crate::error::DevError;
use crate::output;

/// Run the application under `perf record` and save the profile data.
pub fn run_perf(
    binary_str: &str,
    app_args: &[String],
    hostname: &str,
    commit: &str,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let perf_data = data_dir.join(format!("perf-{hostname}-{commit}.data"));
    let perf_data_str = perf_data.display().to_string();

    output::run_msg(&format!("perf record -> {}", perf_data.display()));

    let mut args: Vec<String> = vec![
        "record".into(),
        "-g".into(),
        "--call-graph".into(),
        "dwarf,16384".into(),
        "-F".into(),
        "997".into(),
        "-o".into(),
        perf_data_str.clone(),
        binary_str.into(),
    ];
    args.extend(app_args.iter().cloned());

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let captured = output::run_captured("perf", &args_refs, project_root)?;

    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    captured.check_success("perf")?;

    output::run_msg(&format!("profile saved to {perf_data_str}"));
    output::run_msg("view with:");
    output::run_msg(&format!("  perf report -i {perf_data_str}"));
    output::run_msg(&format!(
        "  perf report -i {perf_data_str} --no-children    (self time only)"
    ));

    Ok(())
}

/// Run the application under `samply record` and save the profile data.
pub fn run_samply(
    binary_str: &str,
    app_args: &[String],
    hostname: &str,
    commit: &str,
    data_dir: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let profile_out = data_dir.join(format!("samply-{hostname}-{commit}.json.gz"));
    let profile_out_str = profile_out.display().to_string();

    output::run_msg(&format!("samply record -> {}", profile_out.display()));

    let mut args: Vec<String> = vec![
        "record".into(),
        "--save-only".into(),
        "-o".into(),
        profile_out_str.clone(),
        binary_str.into(),
    ];
    args.extend(app_args.iter().cloned());

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let captured = output::run_captured("samply", &args_refs, project_root)?;

    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    captured.check_success("samply")?;

    output::run_msg(&format!("profile saved to {profile_out_str}"));
    output::run_msg(&format!("view with: samply load {profile_out_str}"));

    Ok(())
}
