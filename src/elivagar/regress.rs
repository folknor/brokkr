//! Output-regression diff wrapper.
//!
//! Builds the elivagar release binary and runs
//! `elivagar regress <current> --against <blessed> [flags]`, streaming its
//! report live and propagating its exit code: elivagar `regress` uses exit 0
//! for "no accountable diff" and exit 1 for any set/feature/attr/structural
//! diff (or a tolerance budget overrun), and `brokkr regress` is a gate, so
//! that code must reach the caller unchanged. The current archive comes from
//! the durable output dir (by --commit/--file); the blessed archive is
//! resolved + xxhash-verified from brokkr.toml (or passed via --against).

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::output;

#[allow(clippy::too_many_arguments)]
pub fn run(
    current: &Path,
    blessed: &Path,
    project_root: &Path,
    tol: i32,
    max_moved: u64,
    max_examples: usize,
    svg_dump: Option<&Path>,
    json: bool,
) -> Result<(), DevError> {
    let binary = build::cargo_build(&build::BuildConfig::release(None), project_root)?;
    let binary_str = binary.display().to_string();

    let current_str = current.display().to_string();
    let blessed_str = blessed.display().to_string();
    let tol_str = tol.to_string();
    let max_moved_str = max_moved.to_string();
    let max_examples_str = max_examples.to_string();
    let svg_dump_str = svg_dump.map(|p| p.display().to_string());

    let mut args: Vec<&str> = vec![
        "regress",
        &current_str,
        "--against",
        &blessed_str,
        "--tol",
        &tol_str,
        "--max-moved",
        &max_moved_str,
        "--max-examples",
        &max_examples_str,
    ];
    if let Some(dir) = &svg_dump_str {
        args.push("--svg-dump");
        args.push(dir);
    }
    if json {
        args.push("--json");
    }

    output::run_msg(&format!("{binary_str} {}", args.join(" ")));

    let out = output::run_passthrough_timed(&binary_str, &args)?;
    if out.code != 0 {
        // Propagate the gate verdict verbatim (the report went to stdout).
        return Err(DevError::ExitCode(out.code));
    }
    Ok(())
}
