//! Verify harness: cross-validate pbfhogg output against reference tools.
//!
//! Provides [`VerifyHarness`] - a shared context for verify subcommands that
//! handles locking, building the CLI binary, and common operations like
//! running pbfhogg/external tools, diffing PBFs, and checking sort order.

use std::fs;
use std::path::{Path, PathBuf};

use crate::build;
use crate::error::DevError;
use crate::output;
use crate::output::CapturedOutput;

// ---------------------------------------------------------------------------
// VerifyHarness
// ---------------------------------------------------------------------------

/// Shared context for verify subcommands.
///
/// Holds the exclusive lock (preventing concurrent bench/verify runs),
/// the path to the freshly-built CLI binary, and the output directory
/// under `target/verify/`.
pub struct VerifyHarness {
    /// RAII lock - released on drop.
    _lock: crate::lockfile::LockGuard,
    /// Path to the built `pbfhogg` release binary.
    pub binary: PathBuf,
    /// Root output directory: `{target_dir}/verify`.
    pub output_dir: PathBuf,
    /// Project root (used as cwd for subprocess invocations).
    pub project_root: PathBuf,
}

impl VerifyHarness {
    /// Build the CLI binary and prepare the verify output directory.
    ///
    /// Acquires an exclusive lock via [`crate::lockfile::acquire`] so that
    /// no other dev/bench/verify process runs concurrently.
    pub fn new(
        project_root: &Path,
        target_dir: &Path,
        build_root: Option<&Path>,
        features: &[String],
    ) -> Result<Self, DevError> {
        let lock = crate::lockfile::acquire(&crate::lockfile::LockContext {
            project: "pbfhogg",
            command: "verify",
            project_root: &project_root.display().to_string(),
        })?;
        let effective = build_root.unwrap_or(project_root);
        let build_config = if features.is_empty() {
            build::BuildConfig::release(Some("pbfhogg-cli"))
        } else {
            build::BuildConfig::release_with_owned_features(Some("pbfhogg-cli"), features)
        };
        let binary = build::cargo_build(&build_config, effective)?;
        let output_dir = target_dir.join("verify");

        Ok(Self {
            _lock: lock,
            binary,
            output_dir,
            project_root: project_root.to_path_buf(),
        })
    }

    // -- Subprocess runners ------------------------------------------------

    /// Run the pbfhogg CLI with the given arguments.
    ///
    /// Does **not** check the exit status - the caller decides whether
    /// non-zero is an error (some commands exit non-zero normally).
    pub fn run_pbfhogg(&self, args: &[&str]) -> Result<CapturedOutput, DevError> {
        output::run_captured(&self.binary.display().to_string(), args, &self.project_root)
    }

    /// Run an external tool (e.g. `osmium`, `osmconvert`) with the given arguments.
    ///
    /// Does **not** check the exit status.
    pub fn run_tool(&self, program: &str, args: &[&str]) -> Result<CapturedOutput, DevError> {
        output::run_captured(program, args, &self.project_root)
    }

    // -- Common verify operations ------------------------------------------

    /// Print extended inspect output for a PBF, prefixed with `label`.
    ///
    /// Runs `pbfhogg inspect --extended <pbf>`. On failure, prints the
    /// error but does **not** propagate it (informational only).
    pub fn print_inspect(&self, label: &str, pbf: &Path) -> Result<(), DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["inspect", "--extended", &pbf_str])?;

        if captured.status.success() {
            let stdout = String::from_utf8_lossy(&captured.stdout);
            for line in stdout.lines() {
                output::verify_msg(&format!("  {label}: {line}"));
            }
        } else {
            let stderr = String::from_utf8_lossy(&captured.stderr);
            output::error(&format!("inspect failed for {label}: {stderr}"));
        }

        Ok(())
    }

    /// Diff two PBF files using `pbfhogg diff --suppress-common`.
    ///
    /// Returns `Ok(true)` if the files are identical (empty diff output),
    /// `Ok(false)` if differences were found (prints the diff), or an error
    /// only if the subprocess fails to spawn.
    pub fn diff_pbfs(&self, a: &Path, b: &Path) -> Result<bool, DevError> {
        let a_str = a.display().to_string();
        let b_str = b.display().to_string();
        let captured = self.run_pbfhogg(&["diff", "--suppress-common", &a_str, &b_str])?;

        let stdout = String::from_utf8_lossy(&captured.stdout);
        if stdout.trim().is_empty() {
            Ok(true)
        } else {
            for line in stdout.lines() {
                output::verify_msg(line);
            }
            Ok(false)
        }
    }

    /// Check whether a PBF's elements are actually in sorted order.
    ///
    /// Runs `pbfhogg inspect --extended <pbf>` and reads the computed
    /// `Ordered: yes|no` line (derived from ordering segments + ID
    /// monotonicity), NOT the header `Sort.Type_then_ID` optional feature.
    /// The header is only a claim - a blob-level stamp that can be blind to
    /// intra-blob disorder - so a degraded/unsorted file can carry the flag
    /// while its elements are out of order. We trust the computed order and
    /// report the header only for context. Prints a PASS/FAIL message and
    /// returns the result.
    pub fn check_sorted(&self, label: &str, pbf: &Path) -> Result<bool, DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["inspect", "--extended", &pbf_str])?;

        let stdout = String::from_utf8_lossy(&captured.stdout);
        let has_flag = stdout.contains("Sort.Type_then_ID");

        match parse_ordered(&stdout) {
            Some(true) => {
                output::verify_msg(&format!("  {label}: ordered (element order verified) PASS"));
                Ok(true)
            }
            Some(false) => {
                let hint = if has_flag {
                    " - header declares Sort.Type_then_ID but element order disagrees"
                } else {
                    ""
                };
                output::verify_msg(&format!(
                    "  {label}: NOT ordered (inspect Ordered: no){hint} FAIL"
                ));
                Ok(false)
            }
            None => {
                // No `Ordered:` line (older pbfhogg without --extended support):
                // fall back to the header feature so the check still runs.
                if has_flag {
                    output::verify_msg(&format!(
                        "  {label}: sorted (Sort.Type_then_ID header; order not computed) PASS"
                    ));
                } else {
                    output::verify_msg(&format!("  {label}: NOT sorted FAIL"));
                }
                Ok(has_flag)
            }
        }
    }

    /// Compare the sort flag between a pbfhogg-produced PBF and a reference PBF.
    ///
    /// Returns `false` (fail) only if the reference has Sort.Type_then_ID but
    /// the pbfhogg output does not. All other combinations pass.
    pub fn compare_sort_feature(
        &self,
        pbfhogg_pbf: &Path,
        other_pbf: &Path,
    ) -> Result<bool, DevError> {
        let ours = self.has_sort_flag(pbfhogg_pbf)?;
        let theirs = self.has_sort_flag(other_pbf)?;

        output::verify_msg(&format!(
            "  pbfhogg sorted={ours}, reference sorted={theirs}"
        ));

        // Fail only if reference is sorted but we are not.
        if theirs && !ours {
            output::verify_msg("  FAIL: reference is sorted but pbfhogg output is not");
            Ok(false)
        } else {
            Ok(true)
        }
    }

    // -- Directory helpers -------------------------------------------------

    /// Create (if needed) a subdirectory under `output_dir` and return its path.
    pub fn subdir(&self, name: &str) -> Result<PathBuf, DevError> {
        let dir = self.output_dir.join(name);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    // -- Exit-status helper ------------------------------------------------

    /// Assert that a captured subprocess exited successfully.
    ///
    /// Returns `DevError::Subprocess` with the program name and stderr if the
    /// exit code was non-zero (or the process was killed by a signal).
    pub fn check_exit(&self, captured: &CapturedOutput, program: &str) -> Result<(), DevError> {
        if captured.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&captured.stderr);
        Err(DevError::Subprocess {
            program: program.to_owned(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        })
    }

    // -- Internal helpers --------------------------------------------------

    /// Run `inspect` and return whether stdout contains "Sort.Type_then_ID".
    fn has_sort_flag(&self, pbf: &Path) -> Result<bool, DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["inspect", &pbf_str])?;
        let stdout = String::from_utf8_lossy(&captured.stdout);
        Ok(stdout.contains("Sort.Type_then_ID"))
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Run a single verify check with "quiet on pass, loud on fail" output.
///
/// Unless `verbose`, the check's `verify_msg` detail is captured and replayed
/// only if it fails; a passing check prints just a one-line `PASS` summary.
/// With `verbose`, detail streams live. Either way a one-line result is
/// printed. On failure this returns `DevError::ExitCode(1)` (the failure has
/// already been reported here, so `main` exits non-zero without re-printing).
pub(crate) fn run_check<F>(name: &str, verbose: bool, f: F) -> Result<(), DevError>
where
    F: FnOnce() -> Result<(), DevError>,
{
    let start = std::time::Instant::now();
    if !verbose {
        crate::output::verify_buffer_begin();
    }
    let result = f();
    let ms = start.elapsed().as_millis();
    match &result {
        Ok(()) => {
            if !verbose {
                crate::output::verify_buffer_discard();
            }
            crate::output::verify_summary(&format!("{name}: PASS ({ms}ms)"));
            Ok(())
        }
        Err(e) => {
            if !verbose {
                // Replay the captured detail so the failure is debuggable
                // without re-running the check.
                crate::output::verify_buffer_flush();
            }
            crate::output::verify_summary(&format!("{name}: FAIL ({ms}ms): {e}"));
            Err(DevError::ExitCode(1))
        }
    }
}

/// Check whether an executable exists on `PATH`.
pub fn which_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Parse the `Ordered:` line emitted by `pbfhogg inspect --extended`.
///
/// Returns `Some(true)` for `Ordered:  yes`, `Some(false)` for `Ordered: no`,
/// and `None` when no such line is present (e.g. plain, non-extended output).
/// The per-kind `(monotonic: yes|no)` id-range lines are deliberately ignored
/// - the top-level `Ordered:` value already folds in monotonicity.
fn parse_ordered(inspect_text: &str) -> Option<bool> {
    for line in inspect_text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("Ordered:") {
            return Some(rest.trim() == "yes");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_ordered;

    #[test]
    fn parse_ordered_yes() {
        let text = "Features: Sort.Type_then_ID\nOrdered:  yes\nTimestamps: ..\n";
        assert_eq!(parse_ordered(text), Some(true));
    }

    #[test]
    fn parse_ordered_no() {
        // The degraded-unsorted case: header still claims the feature, but the
        // computed order says no. check_sorted must trust this line.
        let text = "Features: Sort.Type_then_ID\nOrdered:  no\n";
        assert_eq!(parse_ordered(text), Some(false));
    }

    #[test]
    fn parse_ordered_absent() {
        // Plain (non-extended) inspect has no Ordered: line.
        let text = "Features: Sort.Type_then_ID\nElements: 100 total\n";
        assert_eq!(parse_ordered(text), None);
    }

    #[test]
    fn parse_ordered_ignores_monotonic_lines() {
        let text = "Ordered:  no\nID ranges:\n  Nodes:  1 .. 9   (monotonic: yes)\n";
        assert_eq!(parse_ordered(text), Some(false));
    }
}
