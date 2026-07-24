//! `brokkr pmtiles-corpus <sub>` exec runner - a thin passthrough over
//! `elivagar corpus <sub> <archive> [flags]`.
//!
//! brokkr resolves the archive (and, where the subcommand uses one, the corpus
//! directory) and passes every other flag through verbatim. It adds no baseline
//! registry, no default comparand, and no interpretation of the verdict: the
//! elivagar corpus machinery enforces its own guards (contract refusal,
//! dirty-build refusal, `--rotate` protection), so the wrapper is convenience,
//! never safety. Exit codes (0 pass / 1 mismatch / 2 refusal) and
//! stdout/stderr pass through unchanged - the 0/1/2 distinction is load-bearing
//! for callers.
//!
//! No clean-tree gate and no tilegen lock: a corpus check is read-only on the
//! archive and never touches tilegen scratch, and bless writes only into the
//! corpus dir (committed with the landing, so a dirty tree is normal there).

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::lockfile::LockGuard;
use crate::output;

/// Build the elivagar release binary and exec
/// `elivagar corpus <subcommand> <archive> [trailing...]`, streaming the report
/// live and propagating the exit code verbatim.
pub fn run(
    build_root: &Path,
    subcommand: &str,
    archive: &Path,
    trailing: &[String],
    lock: Option<&LockGuard>,
) -> Result<(), DevError> {
    let binary = build::cargo_build(&build::BuildConfig::release(None), build_root)?;
    let binary_str = binary.display().to_string();
    let archive_str = archive.display().to_string();

    let mut args: Vec<&str> = vec!["corpus", subcommand, &archive_str];
    for t in trailing {
        args.push(t);
    }

    output::run_msg(&format!("{binary_str} {}", args.join(" ")));
    let out = output::run_passthrough_timed(&binary_str, &args, lock)?;
    if out.code != 0 {
        // Propagate the verdict verbatim (0/1/2); the report went to stdout.
        return Err(DevError::ExitCode(out.code));
    }
    Ok(())
}
