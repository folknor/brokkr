//! Sampling profiler support for nidhogg.
//!
//! Builds with `--profile profiling` (release + debug symbols), then runs
//! under `perf record` or `samply record`.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::git;
use crate::output;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run nidhogg under a sampling profiler.
///
/// `tool` must be `"perf"` or `"samply"`.
pub fn run(
    pbf_path: &Path,
    data_dir: &str,
    scratch_dir: &Path,
    tool: &str,
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

    // Acquire exclusive lock to prevent conflicts with concurrent benchmarks.
    let _lock = crate::lockfile::acquire(scratch_dir)?;

    // Check perf_event_paranoid on Linux.
    check_perf_paranoid()?;

    // Check that the tool is installed.
    check_tool_installed(tool)?;

    let pbf_str = pbf_path
        .to_str()
        .ok_or_else(|| DevError::Config("PBF path is not valid UTF-8".into()))?;

    // Build with profiling profile.
    output::build_msg("building with profiling profile (release + debug symbols)");

    let config = build::BuildConfig {
        package: Some("nidhogg".into()),
        bin: None,
        example: None,
        features: Vec::new(),
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

    // Collect git info for naming.
    let git_info = git::collect(project_root)?;
    let hostname = crate::config::hostname()?;

    match tool {
        "perf" => run_perf(
            &binary_str,
            &nidhogg_args,
            &hostname,
            &git_info.commit,
            data_dir,
            project_root,
        ),
        "samply" => run_samply(
            &binary_str,
            &nidhogg_args,
            &hostname,
            &git_info.commit,
            data_dir,
            project_root,
        ),
        _ => unreachable!(),
    }?;

    // Clean up output directory.
    std::fs::remove_dir_all(output_dir).ok();

    Ok(())
}

// ---------------------------------------------------------------------------
// perf
// ---------------------------------------------------------------------------

fn run_perf(
    binary_str: &str,
    nidhogg_args: &[String],
    hostname: &str,
    commit: &str,
    data_dir: &str,
    project_root: &Path,
) -> Result<(), DevError> {
    let data_path = Path::new(data_dir);
    let perf_data = data_path.join(format!("perf-{hostname}-{commit}.data"));
    let perf_data_str = perf_data.display().to_string();

    output::run_msg(&format!("perf record -> {}", perf_data.display()));

    // Build the full perf command args.
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
    args.extend(nidhogg_args.iter().cloned());

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let captured = output::run_captured("perf", &args_refs, project_root)?;

    // Print stderr (perf progress output).
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    if !captured.status.success() {
        return Err(DevError::Subprocess {
            program: "perf".into(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    output::run_msg(&format!("profile saved to {perf_data_str}"));
    output::run_msg("view with:");
    output::run_msg(&format!("  perf report -i {perf_data_str}"));
    output::run_msg(&format!(
        "  perf report -i {perf_data_str} --no-children    (self time only)"
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// samply
// ---------------------------------------------------------------------------

fn run_samply(
    binary_str: &str,
    nidhogg_args: &[String],
    hostname: &str,
    commit: &str,
    data_dir: &str,
    project_root: &Path,
) -> Result<(), DevError> {
    let data_path = Path::new(data_dir);
    let profile_out = data_path.join(format!("samply-{hostname}-{commit}.json.gz"));
    let profile_out_str = profile_out.display().to_string();

    output::run_msg(&format!("samply record -> {}", profile_out.display()));

    // Build the full samply command args.
    let mut args: Vec<String> = vec![
        "record".into(),
        "--save-only".into(),
        "-o".into(),
        profile_out_str.clone(),
        binary_str.into(),
    ];
    args.extend(nidhogg_args.iter().cloned());

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let captured = output::run_captured("samply", &args_refs, project_root)?;

    // Print stderr (samply progress output).
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    if !captured.status.success() {
        return Err(DevError::Subprocess {
            program: "samply".into(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    output::run_msg(&format!("profile saved to {profile_out_str}"));
    output::run_msg(&format!("view with: samply load {profile_out_str}"));

    Ok(())
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// Check that perf_event_paranoid is <= 1 (required for perf and samply).
fn check_perf_paranoid() -> Result<(), DevError> {
    let path = Path::new("/proc/sys/kernel/perf_event_paranoid");
    if !path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(path).map_err(|e| {
        DevError::Config(format!(
            "cannot read /proc/sys/kernel/perf_event_paranoid: {e}"
        ))
    })?;

    let value: i32 = content.trim().parse().map_err(|_| {
        DevError::Config(format!(
            "invalid perf_event_paranoid value: {content}"
        ))
    })?;

    if value > 1 {
        return Err(DevError::Preflight(vec![format!(
            "/proc/sys/kernel/perf_event_paranoid is {value} (needs <= 1)\n\
             Run:  echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid\n\
             This is a runtime-only change that resets on reboot."
        )]));
    }

    Ok(())
}

/// Check that a tool is installed and on PATH.
fn check_tool_installed(tool: &str) -> Result<(), DevError> {
    let result = std::process::Command::new("which")
        .arg(tool)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => Ok(()),
        _ => {
            let install_hint = match tool {
                "perf" => "sudo apt install linux-tools-common linux-tools-$(uname -r)",
                "samply" => "cargo install samply",
                _ => "",
            };
            Err(DevError::Preflight(vec![format!(
                "'{tool}' not found in PATH. Install: {install_hint}"
            )]))
        }
    }
}
