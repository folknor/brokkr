//! Verify harness: cross-validate pbfhogg output against reference tools.
//!
//! Provides [`VerifyHarness`] - a shared context for verify subcommands that
//! handles locking, building the CLI binary, and common operations like
//! running pbfhogg/external tools, diffing PBFs, and checking sort order.

use std::fs;
use std::path::{Path, PathBuf};

use crate::build;
use crate::error::DevError;
pub(crate) use crate::measure::ExternalTool;
use crate::measure::Invocation;
use crate::output;
use crate::output::CapturedOutput;
pub(crate) use crate::pbfhogg::binary::Writes;
use crate::pbfhogg::binary::PbfhoggBinary;

pub(crate) const SORT_TYPE_THEN_ID: &str = "Sort.Type_then_ID";

/// Stable labels shared by individual verify dispatch and `verify all`.
pub(super) mod labels {
    pub const SORT: &str = "sort";
    pub const CAT: &str = "cat";
    pub const EXTRACT: &str = "extract";
    pub const MULTI_EXTRACT: &str = "multi-extract";
    pub const TAGS_FILTER: &str = "tags-filter";
    pub const GETID_REMOVEID: &str = "getid-removeid";
    pub const ADD_LOCATIONS_TO_WAYS: &str = "add-locations-to-ways";
    pub const CHECK_REFS: &str = "check-refs";
    pub const APPLY_CHANGES: &str = "apply-changes";
    pub const DIFF_OSC: &str = "diff --format osc";
    pub const RENUMBER: &str = "renumber";
    pub const DIFF: &str = "diff";
    pub const ALL: &str = "all";
}

// ---------------------------------------------------------------------------
// VerifyHarness
// ---------------------------------------------------------------------------

/// Shared context for verify subcommands.
///
/// Holds the exclusive lock (preventing concurrent bench/verify runs), a
/// shutdown guard, the path to the freshly-built CLI binary, and the output
/// directory under `target/verify/`.
pub struct VerifyHarness {
    /// Makes `brokkr kill` cooperative for the whole verify: without it the
    /// stop's `SIGTERM` took the default action, killing brokkr mid-check and
    /// orphaning the pbfhogg/osmium child it was waiting on. With it the
    /// captured-child supervisor sees the request, ends the child and returns
    /// `Interrupted`. Declared first, so it is dropped before the lock.
    _signals: crate::shutdown::SigtermGuard,
    /// RAII lock - released on drop.
    _lock: crate::lockfile::LockGuard,
    /// The built `pbfhogg` release binary. Verify builds once, here, so the
    /// handle stays valid for the harness's whole life.
    handle: PbfhoggBinary,
    /// Root output directory: `{target_dir}/verify`.
    pub output_dir: PathBuf,
    /// Project root (used as cwd for subprocess invocations).
    pub project_root: PathBuf,
}

impl VerifyHarness {
    /// Build the CLI binary and prepare the verify output directory.
    ///
    /// Holds the invocation's lock (re-entered, [`crate::context::reenter_command_lock`])
    /// so that no other dev/bench/verify process runs concurrently.
    pub fn new(
        project_root: &Path,
        target_dir: &Path,
        build_root: &Path,
        features: &[String],
    ) -> Result<Self, DevError> {
        let lock = crate::context::reenter_command_lock()?;
        let signals = crate::shutdown::SigtermGuard::install();
        let effective = build_root;
        let build_config = if features.is_empty() {
            build::BuildConfig::release(crate::project::Project::Pbfhogg.cli_package())
        } else {
            build::BuildConfig::release_with_owned_features(
                crate::project::Project::Pbfhogg.cli_package(),
                features,
            )
        };
        // Verify records no results rows, so the build's identity goes unused.
        let (handle, _) = PbfhoggBinary::build(&build_config, effective, project_root)?;
        let output_dir = target_dir.join(crate::target_namespace::VERIFY);

        Ok(Self {
            _signals: signals,
            _lock: lock,
            handle,
            output_dir,
            project_root: project_root.to_path_buf(),
        })
    }

    // -- Subprocess runners ------------------------------------------------

    /// Run the pbfhogg CLI with the given arguments. `writes` states whether
    /// the argv writes an output under the verify directory, which pbfhogg
    /// must then be allowed to replace.
    ///
    /// Does **not** check the exit status - the caller decides whether
    /// non-zero is an error (some commands exit non-zero normally).
    pub(crate) fn run_pbfhogg(
        &self,
        args: &[&str],
        writes: Writes,
    ) -> Result<CapturedOutput, DevError> {
        self.handle
            .invocation_strs(args, writes)?
            .capture_in(&self.project_root)
    }

    /// Run a reference tool with the given arguments.
    ///
    /// Does **not** check the exit status.
    pub(crate) fn run_tool(
        &self,
        tool: ExternalTool,
        args: &[&str],
    ) -> Result<CapturedOutput, DevError> {
        Invocation::tool(tool, args.iter().map(|&a| a.to_owned()).collect())
            .capture_in(&self.project_root)
    }

    // -- Common verify operations ------------------------------------------

    /// Print extended inspect output for a PBF, prefixed with `label`.
    ///
    /// Runs `pbfhogg inspect --extended <pbf>`. On failure, prints the
    /// error but does **not** propagate it (informational only).
    pub fn print_inspect(&self, label: &str, pbf: &Path) -> Result<(), DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["inspect", "--extended", &pbf_str], Writes::Nothing)?;

        if captured.success() {
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
    /// Returns [`Verdict::Pass`] only when `pbfhogg diff` exits 0 with empty
    /// output, [`Verdict::Fail`] when it reports differences (exit 1, or any
    /// diff output), and an error when the diff itself did not complete - a
    /// signal kill or any other exit code. The exit status is load-bearing:
    /// a crashed diff prints nothing on stdout, which read as "identical"
    /// back when only stdout was consulted.
    pub fn diff_pbfs(&self, a: &Path, b: &Path) -> Result<Verdict, DevError> {
        let a_str = a.display().to_string();
        let b_str = b.display().to_string();
        let captured = self.run_pbfhogg(
            &["diff", "--suppress-common", &a_str, &b_str],
            Writes::Nothing,
        )?;
        let stdout = String::from_utf8_lossy(&captured.stdout);
        let verdict =
            diff_verdict(captured.code(), &stdout).map_err(|why| DevError::Subprocess {
                program: format!("pbfhogg diff ({why})"),
                ending: captured
                    .ending
                    .process_ending()
                    .unwrap_or(crate::error::ProcessEnding::Code(-1)),
                stderr: String::from_utf8_lossy(&captured.stderr).into_owned(),
            })?;
        if verdict == Verdict::Fail {
            for line in stdout.lines() {
                output::verify_msg(line);
            }
            if stdout.trim().is_empty() {
                // Exit 1 with no listing: the stderr summary is the only
                // evidence of what differs.
                let stderr = String::from_utf8_lossy(&captured.stderr);
                for line in stderr.lines() {
                    output::verify_msg(&format!("  {line}"));
                }
            }
        }
        Ok(verdict)
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
    /// returns the verdict. A failed `inspect` is an error, not a FAIL: its
    /// empty stdout would otherwise read as "NOT sorted".
    pub fn check_sorted(&self, label: &str, pbf: &Path) -> Result<Verdict, DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["inspect", "--extended", &pbf_str], Writes::Nothing)?;
        self.check_exit(&captured, "pbfhogg inspect --extended")?;

        let stdout = String::from_utf8_lossy(&captured.stdout);
        let has_flag = stdout.contains(SORT_TYPE_THEN_ID);

        match parse_ordered(&stdout) {
            Some(true) => {
                output::verify_msg(&format!("  {label}: ordered (element order verified) PASS"));
                Ok(Verdict::Pass)
            }
            Some(false) => {
                let hint = if has_flag {
                    format!(" - header declares {SORT_TYPE_THEN_ID} but element order disagrees")
                } else {
                    String::new()
                };
                output::verify_msg(&format!(
                    "  {label}: NOT ordered (inspect Ordered: no){hint} FAIL"
                ));
                Ok(Verdict::Fail)
            }
            None => {
                // No `Ordered:` line (older pbfhogg without --extended support):
                // fall back to the header feature so the check still runs.
                if has_flag {
                    output::verify_msg(&format!(
                        "  {label}: sorted ({SORT_TYPE_THEN_ID} header; order not computed) PASS"
                    ));
                } else {
                    output::verify_msg(&format!("  {label}: NOT sorted FAIL"));
                }
                Ok(Verdict::from_pass(has_flag))
            }
        }
    }

    /// Write `input` without its blob-level indexdata to `output`, through
    /// `pbfhogg degrade --strip-indexdata` (element content and order kept),
    /// so a check can drive the unindexed code path of a command whose
    /// indexed run it already verifies.
    pub fn strip_indexdata(&self, input: &Path, output: &Path) -> Result<(), DevError> {
        let input_str = input.display().to_string();
        let output_str = output.display().to_string();
        let captured = self.run_pbfhogg(
            &["degrade", "--strip-indexdata", &input_str, "-o", &output_str],
            Writes::OwnedOutput,
        )?;
        self.check_exit(&captured, "pbfhogg degrade --strip-indexdata")
    }

    /// Compare the sort flag between a pbfhogg-produced PBF and a reference PBF.
    ///
    /// Returns [`Verdict::Fail`] only if the reference has Sort.Type_then_ID
    /// but the pbfhogg output does not. All other combinations pass.
    pub fn compare_sort_feature(
        &self,
        pbfhogg_pbf: &Path,
        other_pbf: &Path,
    ) -> Result<Verdict, DevError> {
        let ours = self.has_sort_flag(pbfhogg_pbf)?;
        let theirs = self.has_sort_flag(other_pbf)?;

        output::verify_msg(&format!(
            "  pbfhogg sorted={ours}, reference sorted={theirs}"
        ));

        // Fail only if reference is sorted but we are not.
        if theirs && !ours {
            output::verify_msg("  FAIL: reference is sorted but pbfhogg output is not");
            Ok(Verdict::Fail)
        } else {
            Ok(Verdict::Pass)
        }
    }

    // -- Directory helpers -------------------------------------------------

    /// Return an EMPTY subdirectory `output_dir/<name>` for one check's outputs.
    ///
    /// Anything a previous run left there is removed first. Checks treat the
    /// existence of an output file as proof that the tool producing it ran
    /// (`verify_merge`'s diff OSC, the optional osmosis/osmconvert outputs,
    /// multi-extract's strip files), so a directory that outlived its run
    /// would let a stale file stand in for one this run never wrote.
    ///
    /// `name` must be a single plain path component - it is joined under
    /// `output_dir` and then removed recursively, so `..`, separators or an
    /// empty name are refused rather than trusted.
    pub fn subdir(&self, name: &str) -> Result<PathBuf, DevError> {
        fresh_subdir(&self.output_dir, name)
    }

    // -- Exit-status helper ------------------------------------------------

    /// Assert that a captured subprocess exited successfully.
    ///
    /// Returns `DevError::Subprocess` with the program name and stderr if the
    /// exit code was non-zero, or naming the signal if one killed it - the
    /// shared [`CapturedOutput::check_success`], which knows the signal the
    /// variant cannot carry.
    pub fn check_exit(&self, captured: &CapturedOutput, program: &str) -> Result<(), DevError> {
        captured.check_success(program)
    }

    // -- Internal helpers --------------------------------------------------

    /// Run `inspect` and return whether stdout contains [`SORT_TYPE_THEN_ID`].
    /// A failed `inspect` is an error - its empty stdout is not "unsorted".
    fn has_sort_flag(&self, pbf: &Path) -> Result<bool, DevError> {
        let pbf_str = pbf.display().to_string();
        let captured = self.run_pbfhogg(&["inspect", &pbf_str], Writes::Nothing)?;
        self.check_exit(&captured, "pbfhogg inspect")?;
        let stdout = String::from_utf8_lossy(&captured.stdout);
        Ok(stdout.contains(SORT_TYPE_THEN_ID))
    }
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// The outcome of one content comparison inside a verify check (a diff, a
/// sort-order probe, a count comparison).
///
/// `#[must_use]` is the enforcement: a comparison helper returning a bare
/// `bool` was discarded with `...?;` at every call site, so a check printed
/// FAIL and then returned `Ok`. A discarded `Verdict` is a compile warning
/// (an error under `-Dwarnings`); the only way to consume one is to fold it
/// into the check's [`Findings`], which [`run_check`] turns into the result.
#[must_use = "a discarded verdict is a silent PASS - record it in the check's Findings"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
}

impl Verdict {
    /// `Pass` when `ok`, else `Fail`.
    pub fn from_pass(ok: bool) -> Self {
        if ok { Self::Pass } else { Self::Fail }
    }

    pub fn is_pass(self) -> bool {
        self == Self::Pass
    }
}

/// The failed comparisons of one verify check, folded by [`run_check`].
///
/// A check returns `Ok(findings)` once every tool ran; each comparison it
/// makes is [`record`](Self::record)ed here. Any recorded failure turns the
/// check into a FAIL - with its buffered detail replayed - exactly as an
/// `Err` does, so a check cannot print FAIL and still report PASS.
#[must_use = "a check's findings must reach run_check, or its failures are dropped"]
#[derive(Debug, Default)]
pub struct Findings {
    failures: Vec<String>,
}

impl Findings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one comparison's verdict under `what` (the failure's name in
    /// the summary line).
    pub fn record(&mut self, what: &str, verdict: Verdict) {
        if verdict == Verdict::Fail {
            self.failures.push(what.to_owned());
        }
    }

    /// Record an unconditional failure (a comparison that could not be made
    /// counts as failed, never as passed).
    pub fn fail(&mut self, what: impl Into<String>) {
        self.failures.push(what.into());
    }

    /// True when no comparison has failed so far.
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// What a verify check closure may return to [`run_check`].
///
/// `Findings` is the form every pbfhogg check uses. `()` stays accepted for a
/// check whose every failure is already an `Err` - it has no verdicts to fold.
pub(crate) trait CheckOutcome {
    /// Fold into the check's pass/fail result.
    fn into_check_result(self, name: &str) -> Result<(), DevError>;
}

impl CheckOutcome for () {
    fn into_check_result(self, _name: &str) -> Result<(), DevError> {
        Ok(())
    }
}

impl CheckOutcome for Findings {
    fn into_check_result(self, name: &str) -> Result<(), DevError> {
        if self.failures.is_empty() {
            return Ok(());
        }
        Err(DevError::Verify(format!(
            "{name}: {} comparison(s) failed: {}",
            self.failures.len(),
            self.failures.join("; ")
        )))
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
/// printed. A check fails when it returns `Err` OR when its [`Findings`]
/// carry a failed comparison - both replay the buffered detail, so a FAIL
/// line a check printed is never swallowed by a PASS summary. On failure
/// this returns `DevError::ExitCode(1)` (the failure has already been
/// reported here, so `main` exits non-zero without re-printing).
///
/// A check cut short by a shutdown request (`brokkr kill`, Ctrl-C) is neither
/// a PASS nor a FAIL: it prints `INTERRUPTED` and returns
/// `DevError::Interrupted` unchanged, so `main` runs the scratch cleanup and
/// `verify all` stops instead of starting checks that would be interrupted
/// at once.
pub(crate) fn run_check<F, O>(name: &str, verbose: bool, f: F) -> Result<(), DevError>
where
    F: FnOnce() -> Result<O, DevError>,
    O: CheckOutcome,
{
    let start = std::time::Instant::now();
    if !verbose {
        crate::output::verify_buffer_begin();
    }
    let result = f().and_then(|outcome| outcome.into_check_result(name));
    let ms = start.elapsed().as_millis();
    match &result {
        Ok(()) => {
            if !verbose {
                crate::output::verify_buffer_discard();
            }
            crate::output::verify_summary(&format!("{name}: PASS ({ms}ms)"));
            Ok(())
        }
        Err(DevError::Interrupted) => {
            if !verbose {
                crate::output::verify_buffer_flush();
            }
            crate::output::verify_summary(&format!("{name}: INTERRUPTED ({ms}ms)"));
            Err(DevError::Interrupted)
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
    crate::tools::find_executable(name).is_some()
}

/// Classify a `pbfhogg diff --suppress-common` run from its exit code and
/// stdout. pbfhogg follows osmium's convention: 0 = identical, 1 =
/// differences found. Anything else - a signal (`None`) or another code -
/// means the diff did not complete, which is an error rather than a verdict
/// (`Err` carries the reason for the error message).
///
/// A Rust binary whose `main` returns an error also exits 1, so exit 1 alone
/// cannot tell "differences" from "diff refused its inputs". Both are a FAIL,
/// which is the safe side; [`VerifyHarness::diff_pbfs`] replays stderr when
/// the listing is empty so the reader can tell which it was.
fn diff_verdict(code: Option<i32>, stdout: &str) -> Result<Verdict, &'static str> {
    match code {
        // Exit 0 with a listing is contradictory; the listing wins, since a
        // false FAIL is investigated and a false PASS is not.
        Some(0) => Ok(Verdict::from_pass(stdout.trim().is_empty())),
        Some(1) => Ok(Verdict::Fail),
        Some(_) => Err("unexpected exit code"),
        None => Err("killed by signal"),
    }
}

/// The body of [`VerifyHarness::subdir`]: `root/<name>`, emptied of anything
/// a previous run left, then (re)created.
fn fresh_subdir(root: &Path, name: &str) -> Result<PathBuf, DevError> {
    if !is_plain_component(name) {
        return Err(DevError::Config(format!(
            "verify output subdir must be a single path component, got {name:?}"
        )));
    }
    let dir = root.join(name);
    match fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(DevError::Config(format!(
                "cannot clear stale verify outputs in {}: {e}",
                dir.display()
            )));
        }
    }
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// True for a single, ordinary path component: non-empty, not `.`/`..`, no
/// separator. Guards [`fresh_subdir`]'s recursive removal.
fn is_plain_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// Parse the `Ordered:` line emitted by `pbfhogg inspect --extended`.
///
/// Returns `Some(true)` for `Ordered:  yes`, `Some(false)` for `Ordered: no`,
/// and `None` when no such line is present (e.g. plain, non-extended output).
/// The per-kind `(monotonic: yes|no)` id-range lines are deliberately ignored
/// - the top-level `Ordered:` value already folds in monotonicity.
fn parse_ordered(inspect_text: &str) -> Option<bool> {
    super::inspect_report::InspectReport::parse(inspect_text).ordered
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{
        CheckOutcome, Findings, SORT_TYPE_THEN_ID, Verdict, diff_verdict, fresh_subdir,
        is_plain_component, parse_ordered,
    };

    #[test]
    fn fresh_subdir_drops_a_previous_runs_outputs() {
        // The bug: target/verify/<check>/ outlived its run, so a stale output
        // passed an `exists()` check this run never earned.
        let root = crate::test_scratch::scratch("pbfhogg-verify", "fresh-subdir");
        let first = fresh_subdir(&root, "merge").unwrap();
        std::fs::write(first.join("pbfhogg-vs-osmium.osc"), "stale").unwrap();
        std::fs::create_dir_all(first.join("nested")).unwrap();

        let second = fresh_subdir(&root, "merge").unwrap();
        assert_eq!(first, second);
        assert!(second.is_dir());
        assert_eq!(std::fs::read_dir(&second).unwrap().count(), 0);
    }

    #[test]
    fn fresh_subdir_leaves_siblings_alone() {
        let root = crate::test_scratch::scratch("pbfhogg-verify", "fresh-subdir-siblings");
        let sort = fresh_subdir(&root, "sort").unwrap();
        std::fs::write(sort.join("keep"), "x").unwrap();
        fresh_subdir(&root, "cat").unwrap();
        assert!(sort.join("keep").exists());
        assert!(fresh_subdir(&root, "..").is_err());
        assert!(root.is_dir());
    }

    #[test]
    fn diff_verdict_exit_zero_empty_is_pass() {
        assert_eq!(diff_verdict(Some(0), ""), Ok(Verdict::Pass));
        assert_eq!(diff_verdict(Some(0), "  \n"), Ok(Verdict::Pass));
    }

    #[test]
    fn diff_verdict_listing_is_fail_whatever_the_exit() {
        assert_eq!(diff_verdict(Some(0), "*n1\n"), Ok(Verdict::Fail));
        assert_eq!(diff_verdict(Some(1), "*n1\n"), Ok(Verdict::Fail));
        // Exit 1 with nothing on stdout still reports differences.
        assert_eq!(diff_verdict(Some(1), ""), Ok(Verdict::Fail));
    }

    #[test]
    fn diff_verdict_crash_is_an_error_not_identical() {
        // The bug: a crashed diff with empty stdout read as "identical".
        assert!(diff_verdict(None, "").is_err());
        assert!(diff_verdict(Some(2), "").is_err());
        assert!(diff_verdict(Some(101), "").is_err());
    }

    #[test]
    fn findings_fold_to_fail_on_any_recorded_failure() {
        let mut f = Findings::new();
        f.record("diff", Verdict::Pass);
        assert!(f.into_check_result("sort").is_ok());

        let mut f = Findings::new();
        f.record("diff", Verdict::Pass);
        f.record("sort order", Verdict::Fail);
        f.fail("complement");
        assert_eq!(
            f.failures,
            vec!["sort order".to_owned(), "complement".to_owned()]
        );
        let err = f.into_check_result("sort").unwrap_err().to_string();
        assert!(err.contains("sort order"), "got: {err}");
        assert!(err.contains("complement"), "got: {err}");
    }

    #[test]
    fn unit_outcome_folds_to_ok() {
        assert!(().into_check_result("x").is_ok());
    }

    #[test]
    fn run_check_passes_an_interrupt_through_rather_than_failing() {
        // The bug: an interrupted check folded into a FAIL like any error,
        // so `main` never ran the interrupt cleanup and `verify all` went on
        // to start the next check.
        let interrupted = super::run_check("x", true, || -> Result<(), crate::error::DevError> {
            Err(crate::error::DevError::Interrupted)
        });
        assert!(matches!(
            interrupted,
            Err(crate::error::DevError::Interrupted)
        ));
        let failed = super::run_check("x", true, || -> Result<(), crate::error::DevError> {
            Err(crate::error::DevError::Verify("boom".into()))
        });
        assert!(matches!(failed, Err(crate::error::DevError::ExitCode(1))));
    }

    #[test]
    fn plain_component_guard() {
        assert!(is_plain_component("merge"));
        assert!(is_plain_component("getid-removeid"));
        assert!(!is_plain_component(""));
        assert!(!is_plain_component("."));
        assert!(!is_plain_component(".."));
        assert!(!is_plain_component("a/b"));
        assert!(!is_plain_component("../x"));
    }

    #[test]
    fn parse_ordered_yes() {
        let text = format!("Features: {SORT_TYPE_THEN_ID}\nOrdered:  yes\nTimestamps: ..\n");
        assert_eq!(parse_ordered(&text), Some(true));
    }

    #[test]
    fn parse_ordered_no() {
        // The degraded-unsorted case: header still claims the feature, but the
        // computed order says no. check_sorted must trust this line.
        let text = format!("Features: {SORT_TYPE_THEN_ID}\nOrdered:  no\n");
        assert_eq!(parse_ordered(&text), Some(false));
    }

    #[test]
    fn parse_ordered_absent() {
        // Plain (non-extended) inspect has no Ordered: line.
        let text = format!("Features: {SORT_TYPE_THEN_ID}\nElements: 100 total\n");
        assert_eq!(parse_ordered(&text), None);
    }

    #[test]
    fn parse_ordered_ignores_monotonic_lines() {
        let text = "Ordered:  no\nID ranges:\n  Nodes:  1 .. 9   (monotonic: yes)\n";
        assert_eq!(parse_ordered(text), Some(false));
    }
}
