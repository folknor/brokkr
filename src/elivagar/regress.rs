//! Output-regression diff wrapper (tier-3 attribution).
//!
//! Builds the elivagar release binary and runs
//! `elivagar regress <current> --against <comparand> [flags]`, streaming its
//! report live and propagating its exit code: elivagar `regress` uses exit 0
//! for "no accountable diff" and exit 1 for any set/feature/attr/structural
//! diff (or a tolerance budget overrun), and `brokkr regress` is a passthrough,
//! so that code must reach the caller unchanged.
//!
//! There is deliberately **no comparability gate** and no baseline registry.
//! regress reads no provenance contract by design - it is the attribution
//! instrument, and its legitimate uses include cross-contract comparisons
//! (adjudicating artifact-active vs computed output, pricing an intended config
//! change). A brokkr-side hard refusal would block those and push people back
//! to the raw binary. Comparability is the caller's responsibility; the help
//! text points at `brokkr pmtiles-inspect` for reading the provenance blocks,
//! and warns that cross-variant comparisons report six-figure diffs on two
//! correct builds.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::output;

#[allow(clippy::too_many_arguments)]
pub fn run(
    current: &Path,
    comparand: &Path,
    build_root: &Path,
    tol: i32,
    max_moved: u64,
    max_examples: usize,
    overlay: Option<&Path>,
    overlay_max: Option<usize>,
    json: bool,
    lock: Option<&LockGuard>,
) -> Result<(), DevError> {
    let binary = build::cargo_build(&build::BuildConfig::release(None), build_root)?;
    let binary_str = binary.display().to_string();

    let current_str = current.display().to_string();
    let comparand_str = comparand.display().to_string();
    let tol_str = tol.to_string();
    let max_moved_str = max_moved.to_string();
    let max_examples_str = max_examples.to_string();
    let overlay_str = overlay.map(|p| p.display().to_string());
    let overlay_max_str = overlay_max.map(|n| n.to_string());

    let mut args: Vec<&str> = vec![
        "regress",
        &current_str,
        "--against",
        &comparand_str,
        "--tol",
        &tol_str,
        "--max-moved",
        &max_moved_str,
        "--max-examples",
        &max_examples_str,
    ];
    if let Some(dir) = &overlay_str {
        args.push("--overlay");
        args.push(dir);
    }
    if let Some(n) = &overlay_max_str {
        args.push("--overlay-max");
        args.push(n);
    }
    if json {
        args.push("--json");
    }

    output::run_msg(&format!("{binary_str} {}", args.join(" ")));

    let out = output::run_passthrough_timed(&binary_str, &args, lock)?;
    if out.code != 0 {
        // Propagate the verdict verbatim (the report went to stdout).
        return Err(DevError::ExitCode(out.code));
    }
    Ok(())
}
