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
pub(crate) use crate::pbfhogg::argv::PbfArgv;
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

    /// Run the pbfhogg CLI with `argv`, whose outputs under the verify
    /// directory pbfhogg is allowed to replace.
    ///
    /// Does **not** check the exit status - the caller decides whether
    /// non-zero is an error (some commands exit non-zero normally).
    pub(crate) fn run_pbfhogg(&self, argv: PbfArgv) -> Result<CapturedOutput, DevError> {
        let subcommand = argv.args().first().cloned().unwrap_or_default();
        let _step = step(&format!("pbfhogg {subcommand}"));
        self.handle.invocation(argv)?.capture_in(&self.project_root)
    }

    /// Run the pbfhogg CLI with its stdout written to the file `stdout`
    /// rather than held in memory: for a step whose output scales with the
    /// data (a diff listing). The returned output's `stdout` is empty; stderr
    /// is captured bounded, and a truncated or uncloseable stderr is an error.
    ///
    /// Does **not** check the exit status.
    pub(crate) fn run_pbfhogg_to(
        &self,
        argv: PbfArgv,
        stdout: &Path,
    ) -> Result<CapturedOutput, DevError> {
        let subcommand = argv.args().first().cloned().unwrap_or_default();
        let label = format!("pbfhogg {subcommand}");
        let _step = step(&label);
        let run = self
            .handle
            .invocation(argv)?
            .run_to_file(&self.project_root, stdout)?;
        if let Some(failure) = run.capture_failure(&label) {
            return Err(failure);
        }
        Ok(run.captured)
    }

    /// Run a reference tool with the given arguments, its stdout written to
    /// the file `stdout` (every reference run does this, so none holds its
    /// stdout in memory and every one reaches the ending classification).
    ///
    /// A reference tool killed by `SIGKILL` - which brokkr never sends one;
    /// its own stop is a SIGTERM that comes back as `Interrupted` - is a
    /// [`DevError::VerifyEnvironment`]: the comparison could not be made, and
    /// that says nothing about pbfhogg. The kill is classified before any
    /// capture failure, so a tool that flooded stderr and was then killed is
    /// still reported as killed.
    ///
    /// Does **not** check the exit status otherwise.
    pub(crate) fn run_tool(
        &self,
        tool: ExternalTool,
        args: &[&str],
        stdout: &Path,
    ) -> Result<CapturedOutput, DevError> {
        let label = format!("{} {}", tool.name(), args.first().copied().unwrap_or(""));
        let invocation = Invocation::tool(tool, args.iter().map(|&a| a.to_owned()).collect());
        self.run_reference(&invocation, &label, stdout)
    }

    /// [`run_tool`](Self::run_tool) for a reference program that is not a
    /// `PATH` tool (osmosis, a resolved launcher with its own environment):
    /// the same file sink, death signal, orphan token and ending
    /// classification, under the step `label`.
    ///
    /// The death signal reaches the program brokkr spawned. A launcher that
    /// `exec`s its real program (osmosis's start script execs java) passes it
    /// on; helpers it forks first (its preamble's subshells) do not inherit
    /// it, and are left to the orphan reaper.
    pub(crate) fn run_reference(
        &self,
        invocation: &Invocation<'_>,
        label: &str,
        stdout: &Path,
    ) -> Result<CapturedOutput, DevError> {
        let _step = step(label);
        let run = invocation.run_to_file(&self.project_root, stdout)?;
        if let Some(why) = reference_kill(run.captured.signal(), label) {
            return Err(DevError::VerifyEnvironment(why));
        }
        if let Some(failure) = run.capture_failure(label) {
            return Err(failure);
        }
        Ok(run.captured)
    }

    // -- Common verify operations ------------------------------------------

    /// Print the extended inspect summary of a PBF, prefixed with `label`,
    /// and name the file holding the full report.
    ///
    /// Runs `pbfhogg inspect --json --extended <pbf>` with its report written
    /// to a file ([`super::inspect_report`]). A failed inspect or unreadable
    /// report is printed but **not** propagated (informational only); an
    /// interrupt still is. An output whose order a check judges is
    /// summarised by [`check_sorted`](Self::check_sorted) instead.
    pub fn print_inspect(&self, label: &str, pbf: &Path) -> Result<(), DevError> {
        match super::inspect_report::inspect(self, pbf, true) {
            Ok(summary) => print_summary(label, &summary, pbf),
            Err(DevError::Interrupted) => return Err(DevError::Interrupted),
            Err(e) => output::error(&format!("inspect failed for {label}: {e}")),
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
    ///
    /// The listing goes to the file `listing` (in the calling check's
    /// directory), never into memory: a real mismatch at planet scale lists
    /// as many elements as differ. A FAIL replays only a bounded head of it
    /// and names the file.
    pub fn diff_pbfs(&self, a: &Path, b: &Path, listing: &Path) -> Result<Verdict, DevError> {
        // A text diff with automatic jobs: it makes run scratch.
        let argv = PbfArgv::new("diff")
            .arg("--suppress-common")
            .input(a)?
            .input(b)?
            .scratch(&self.output_dir);
        let captured = self.run_pbfhogg_to(argv, listing)?;
        let listed = super::verify_stream::has_content(listing)?;
        let verdict =
            diff_verdict(captured.code(), listed).map_err(|why| DevError::Subprocess {
                program: format!("pbfhogg diff ({why})"),
                ending: captured
                    .ending
                    .process_ending()
                    .unwrap_or(crate::error::ProcessEnding::Code(-1)),
                stderr: String::from_utf8_lossy(&captured.stderr).into_owned(),
            })?;
        if verdict == Verdict::Fail {
            replay_listing(listing)?;
            if !listed {
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
    /// Runs `pbfhogg inspect --json --extended <pbf>` (report to a file,
    /// [`super::inspect_report`]), prints its summary as
    /// [`print_inspect`](Self::print_inspect) does, and reads the computed
    /// `data.objects_ordered` (ordering segments + ID monotonicity), NOT the
    /// header `Sort.Type_then_ID` feature. The header is only a claim - a
    /// blob-level stamp that can be blind to intra-blob disorder - so a
    /// degraded/unsorted file can carry the flag while its elements are out
    /// of order. We trust the computed order and report the header only for
    /// context. Prints a PASS/FAIL message and returns the verdict. A failed
    /// `inspect`, or a report without the computed order, is an error, not a
    /// FAIL.
    pub fn check_sorted(&self, label: &str, pbf: &Path) -> Result<Verdict, DevError> {
        Ok(self.check_sorted_counted(label, pbf)?.0)
    }

    /// [`check_sorted`](Self::check_sorted), also returning the element
    /// counts of the same extended report. Those are decoded counts, and a
    /// `Pass` also proves IDs strictly ascend within each kind, so each count
    /// is of distinct keys: what set arithmetic over an element diff needs.
    pub(super) fn check_sorted_counted(
        &self,
        label: &str,
        pbf: &Path,
    ) -> Result<(Verdict, super::inspect_report::InspectCounts), DevError> {
        let summary = super::inspect_report::inspect(self, pbf, true)?;
        let counts = summary.counts();
        let verdict = Self::sorted_verdict(label, &summary, pbf)?;
        Ok((verdict, counts))
    }

    fn sorted_verdict(
        label: &str,
        summary: &super::inspect_report::InspectSummary,
        pbf: &Path,
    ) -> Result<Verdict, DevError> {
        print_summary(label, summary, pbf);
        let ordered = summary.objects_ordered().ok_or_else(|| {
            DevError::Internal("an extended inspect summary without its order".into())
        })?;
        if ordered {
            output::verify_msg(&format!("  {label}: ordered (element order verified) PASS"));
            return Ok(Verdict::Pass);
        }
        let hint = if summary.declares_sorted() {
            format!(" - header declares {SORT_TYPE_THEN_ID} but element order disagrees")
        } else {
            String::new()
        };
        output::verify_msg(&format!(
            "  {label}: NOT ordered (inspect objects_ordered: false){hint} FAIL"
        ));
        Ok(Verdict::Fail)
    }

    /// Check that every way's inline coordinates in a LocationsOnWays PBF
    /// match the records of the nodes it references, through `pbfhogg check
    /// --locations`. Exit 0 is consistent, 1 is violations found (the report
    /// is printed), anything else - a signal, a usage error, an older pbfhogg
    /// without the flag - is an error, never a verdict.
    ///
    /// Returns the verdict and the check's report (its stdout), which on a
    /// FAIL is also written to the detail channel. A caller reporting an
    /// informational result reads the report itself: detail is replayed only
    /// when the check fails.
    pub fn check_locations(&self, label: &str, pbf: &Path) -> Result<(Verdict, String), DevError> {
        let captured = self.run_pbfhogg(PbfArgv::new("check").arg("--locations").input(pbf)?)?;
        let report = String::from_utf8_lossy(&captured.stdout).into_owned();
        match captured.code() {
            Some(0) => {
                output::verify_msg(&format!("  {label}: inline locations consistent PASS"));
                Ok((Verdict::Pass, report))
            }
            Some(1) => {
                for line in report.lines() {
                    output::verify_msg(&format!("  {label}: {line}"));
                }
                output::verify_msg(&format!("  {label}: inline locations STALE FAIL"));
                Ok((Verdict::Fail, report))
            }
            _ => {
                self.check_exit(&captured, "pbfhogg check --locations")?;
                Err(DevError::Internal(
                    "pbfhogg check --locations exited 0 after a non-zero status".into(),
                ))
            }
        }
    }

    /// Write `input` without its blob-level indexdata to `output`, through
    /// `pbfhogg degrade --strip-indexdata` (element content and order kept),
    /// so a check can drive the unindexed code path of a command whose
    /// indexed run it already verifies.
    pub fn strip_indexdata(&self, input: &Path, output: &Path) -> Result<(), DevError> {
        let captured = self.run_pbfhogg(
            PbfArgv::new("degrade")
                .arg("--strip-indexdata")
                .input(input)?
                .output(output)?,
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

    /// Whether `pbf`'s header declares [`SORT_TYPE_THEN_ID`], from a plain
    /// `inspect --json` (report to a file). A failed `inspect` is an error,
    /// not "unsorted".
    fn has_sort_flag(&self, pbf: &Path) -> Result<bool, DevError> {
        Ok(super::inspect_report::inspect(self, pbf, false)?.declares_sorted())
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
    /// What a PASS of this check establishes, when that is narrower than
    /// the check's name suggests; printed in its PASS line.
    claim: Option<&'static str>,
}

impl Findings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Findings whose PASS line states `claim`: what the check's recorded
    /// comparisons establish, so a PASS is not read as more (such as
    /// equivalence with a reference tool whose output it does not gate on).
    /// Set once here, never overwritten.
    pub fn claiming(claim: &'static str) -> Self {
        Self {
            failures: Vec::new(),
            claim: Some(claim),
        }
    }

    /// Record one comparison's verdict under `what` (the failure's name in
    /// the summary line).
    pub fn record(&mut self, what: &str, verdict: Verdict) {
        if verdict == Verdict::Fail {
            self.fail(what);
        }
    }

    /// Record an unconditional failure (a comparison that could not be made
    /// counts as failed, never as passed).
    ///
    /// Also written to the check's ledger ([`FailureLedger`]), so a failure
    /// stays on record when a later step's error unwinds the check past this
    /// value - an environment kill must not erase a mismatch already seen.
    pub fn fail(&mut self, what: impl Into<String>) {
        let what = what.into();
        FailureLedger::add(&what);
        self.failures.push(what);
    }
}

/// What a verify check closure may return to [`run_check`].
///
/// `Findings` is the form every pbfhogg check uses. `()` stays accepted for a
/// check whose every failure is already an `Err` - it has no verdicts to fold.
pub(crate) trait CheckOutcome {
    /// Fold into the check's pass/fail result; a pass carries the check's
    /// claim ([`Findings::claiming`]), if it states one.
    fn into_check_result(self, name: &str) -> Result<Option<&'static str>, DevError>;
}

impl CheckOutcome for () {
    fn into_check_result(self, _name: &str) -> Result<Option<&'static str>, DevError> {
        Ok(None)
    }
}

impl CheckOutcome for Findings {
    fn into_check_result(self, name: &str) -> Result<Option<&'static str>, DevError> {
        if self.failures.is_empty() {
            return Ok(self.claim);
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
///
/// A check that could not be completed for a reason outside pbfhogg (a
/// [`DevError::VerifyEnvironment`], with no comparison failure on record)
/// prints `ENVIRONMENT` and returns `ExitCode(ENVIRONMENT_EXIT_CODE)`.
pub(crate) fn run_check<F, O>(name: &str, verbose: bool, f: F) -> Result<(), DevError>
where
    F: FnOnce() -> Result<O, DevError>,
    O: CheckOutcome,
{
    match run_check_status(name, verbose, f)? {
        CheckStatus::Passed => Ok(()),
        CheckStatus::Failed => Err(DevError::ExitCode(1)),
        CheckStatus::Environment => Err(DevError::ExitCode(ENVIRONMENT_EXIT_CODE)),
    }
}

/// Exit code of a verify run whose only non-passing checks could not be
/// completed for environmental reasons: `EX_TEMPFAIL` from sysexits, "try
/// again" - distinct from 1, which always means pbfhogg failed a check.
pub(crate) const ENVIRONMENT_EXIT_CODE: i32 = 75;

/// How one check ended, for a caller that tallies (`verify all`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckStatus {
    Passed,
    Failed,
    /// Not completed for a reason outside the code under test, with no
    /// comparison failure recorded before it.
    Environment,
}

/// [`run_check`]'s body, reporting how the check ended rather than mapping
/// it onto an exit code. `Err` is only `Interrupted`.
pub(crate) fn run_check_status<F, O>(
    name: &str,
    verbose: bool,
    f: F,
) -> Result<CheckStatus, DevError>
where
    F: FnOnce() -> Result<O, DevError>,
    O: CheckOutcome,
{
    let start = std::time::Instant::now();
    StepTimes::reset();
    FailureLedger::reset();
    // The check as a holder activity, its tool runs as steps beneath it
    // (`step`): what `brokkr lock` shows from another shell. On a terminal
    // the buffered (non-verbose) run also keeps a live status line naming
    // the step in flight, so a long check is not silent until its summary.
    let _activity = crate::lockfile::activity::Activity::begin(format!("verify {name}"));
    if !verbose {
        crate::output::verify_buffer_begin();
        crate::output::enable_status_line();
        crate::output::status(&format!("{name}: running"));
    }
    let result = f().and_then(|outcome| outcome.into_check_result(name));
    if !verbose {
        crate::output::disable_status_line();
    }
    let ms = start.elapsed().as_millis();
    let steps = StepTimes::render();
    let ms = if steps.is_empty() {
        format!("{ms}ms")
    } else {
        format!("{ms}ms; {steps}")
    };
    let recorded = FailureLedger::take();
    // The ledger is authoritative: a failure recorded in findings the check
    // then dropped still fails it.
    let result = match result {
        Ok(_) if !recorded.is_empty() => Err(DevError::Verify(format!(
            "{name}: {} comparison(s) failed: {}",
            recorded.len(),
            recorded.join("; ")
        ))),
        other => other,
    };
    match &result {
        Ok(claim) => {
            if !verbose {
                crate::output::verify_buffer_discard();
            }
            crate::output::verify_summary(&pass_line(name, &ms, *claim));
            Ok(CheckStatus::Passed)
        }
        Err(DevError::Interrupted) => {
            if !verbose {
                crate::output::verify_buffer_flush();
            }
            crate::output::verify_summary(&format!("{name}: INTERRUPTED ({ms})"));
            Err(DevError::Interrupted)
        }
        Err(e @ DevError::VerifyEnvironment(_)) if recorded.is_empty() => {
            if !verbose {
                crate::output::verify_buffer_flush();
            }
            crate::output::verify_summary(&format!("{name}: ENVIRONMENT ({ms}): {e}"));
            Ok(CheckStatus::Environment)
        }
        Err(e) => {
            if !verbose {
                // Replay the captured detail so the failure is debuggable
                // without re-running the check.
                crate::output::verify_buffer_flush();
            }
            // A failure recorded before an error that unwound the check is
            // still the verdict; the error is named beside it.
            if matches!(e, DevError::VerifyEnvironment(_)) {
                crate::output::verify_summary(&format!(
                    "{name}: FAIL ({ms}): {} comparison(s) failed before the check \
                     stopped: {}; then {e}",
                    recorded.len(),
                    recorded.join("; ")
                ));
            } else {
                crate::output::verify_summary(&format!("{name}: FAIL ({ms}): {e}"));
            }
            Ok(CheckStatus::Failed)
        }
    }
}

/// A passing check's summary line, naming what it established when the check
/// states a claim.
fn pass_line(name: &str, ms: &str, claim: Option<&str>) -> String {
    match claim {
        Some(claim) => format!("{name}: PASS ({ms}): {claim}"),
        None => format!("{name}: PASS ({ms})"),
    }
}

/// The comparison failures recorded in the check in flight, written by
/// [`Findings::fail`] and read by [`run_check_status`]: the copy of a check's
/// findings that an early `?` cannot drop. Process-wide, because checks run
/// one at a time.
struct FailureLedger;

static FAILURE_LEDGER: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

impl FailureLedger {
    fn lock() -> std::sync::MutexGuard<'static, Vec<String>> {
        FAILURE_LEDGER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn add(what: &str) {
        Self::lock().push(what.to_owned());
    }

    fn reset() {
        Self::lock().clear();
    }

    fn take() -> Vec<String> {
        std::mem::take(&mut *Self::lock())
    }
}

/// The environment reason for a reference tool's ending, when it was killed
/// by `SIGKILL`. brokkr never sends a reference tool SIGKILL (its own stop
/// arrives as SIGTERM and is classified `Interrupted`), so the kill came from
/// outside: commonly the kernel OOM killer, though the signal alone does not
/// prove which. Any other signal stays a tool failure.
fn reference_kill(signal: Option<i32>, label: &str) -> Option<String> {
    (signal == Some(libc::SIGKILL)).then(|| {
        format!(
            "{label} was killed by SIGKILL, which brokkr did not send (commonly the kernel \
             OOM killer); the comparison could not be made and says nothing about pbfhogg"
        )
    })
}

/// Where [`VerifyHarness::diff_pbfs`] writes the listing for a comparison
/// involving `output` (a file in the calling check's directory): beside it,
/// as `<stem>.diff.txt`, so each compared output has its own listing.
pub(crate) fn listing_beside(output: &Path) -> PathBuf {
    output.with_extension("diff.txt")
}

/// Replay a bounded head of a diff listing into the check detail, naming the
/// file that holds all of it.
pub(super) fn replay_listing(listing: &Path) -> Result<(), DevError> {
    let (lines, more) = super::verify_stream::preview(
        listing,
        super::verify_stream::PREVIEW_LINES,
        super::verify_stream::PREVIEW_LINE_BYTES,
    )?;
    for line in &lines {
        output::verify_msg(line);
    }
    let shown = if more {
        "a bounded head"
    } else {
        "lines cut to a bound"
    };
    output::verify_msg(&format!(
        "  (listing above is {shown}; all of it is in {})",
        output::path(listing)
    ));
    Ok(())
}

/// One verify step - a tool run - as a holder activity under the check's, and
/// as the status line on a terminal. Closed when the guard drops, which also
/// adds its wall time to the check's [`StepTimes`].
pub(super) fn step(label: &str) -> Step {
    let activity = crate::lockfile::activity::Activity::begin(label);
    crate::output::status(label);
    Step {
        _activity: activity,
        label: label.to_owned(),
        started: std::time::Instant::now(),
    }
}

pub(super) struct Step {
    _activity: crate::lockfile::activity::Activity,
    label: String,
    started: std::time::Instant,
}

impl Drop for Step {
    fn drop(&mut self) {
        StepTimes::add(std::mem::take(&mut self.label), self.started.elapsed());
    }
}

/// Wall time per step label within the check in flight, read by
/// [`run_check`] into its summary line, so a `verify` run shows where each
/// check spends its time: the pbfhogg command under test, the reference tool,
/// and the comparisons (`pbfhogg diff`, `pbfhogg inspect`). Process-wide,
/// because checks run one at a time.
struct StepTimes;

static STEP_TIMES: std::sync::Mutex<Vec<(String, std::time::Duration, usize)>> =
    std::sync::Mutex::new(Vec::new());

impl StepTimes {
    fn lock() -> std::sync::MutexGuard<'static, Vec<(String, std::time::Duration, usize)>> {
        STEP_TIMES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn add(label: String, took: std::time::Duration) {
        let mut times = Self::lock();
        match times.iter_mut().find(|(l, _, _)| *l == label) {
            Some((_, total, runs)) => {
                *total += took;
                *runs += 1;
            }
            None => times.push((label, took, 1)),
        }
    }

    /// Forget every recorded step, at a check's start.
    fn reset() {
        Self::lock().clear();
    }

    /// The recorded steps, slowest first: `osmium sort 2m0s, pbfhogg diff
    /// 15.1s (2 runs)`. Empty when nothing ran.
    fn render() -> String {
        let mut times = std::mem::take(&mut *Self::lock());
        render_step_times(&mut times)
    }
}

fn render_step_times(times: &mut [(String, std::time::Duration, usize)]) -> String {
    times.sort_by_key(|(_, total, _)| std::cmp::Reverse(*total));
    times
        .iter()
        .map(|(label, total, runs)| {
            let runs = if *runs > 1 {
                format!(" ({runs} runs)")
            } else {
                String::new()
            };
            format!("{label} {}{runs}", output::format_duration(*total))
        })
        .collect::<Vec<_>>()
        .join(", ")
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
fn diff_verdict(code: Option<i32>, listed: bool) -> Result<Verdict, &'static str> {
    match code {
        // Exit 0 with a listing is contradictory; the listing wins, since a
        // false FAIL is investigated and a false PASS is not.
        Some(0) => Ok(Verdict::from_pass(!listed)),
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
    match crate::reservations::remove_dir_all(&dir) {
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

/// Print an inspect summary into the check detail, naming the report file
/// that holds all of it.
fn print_summary(label: &str, summary: &super::inspect_report::InspectSummary, pbf: &Path) {
    for line in summary.lines(label) {
        output::verify_msg(&format!("  {line}"));
    }
    output::verify_msg(&format!(
        "  {label}: full report in {}",
        output::path(&super::inspect_report::report_path(pbf, true))
    ));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::{
        CheckOutcome, Findings, Verdict, diff_verdict, fresh_subdir, is_plain_component,
        render_step_times,
    };

    #[test]
    fn step_times_render_slowest_first_with_run_counts() {
        use std::time::Duration;
        let mut times = vec![
            ("pbfhogg diff".to_owned(), Duration::from_secs(15), 2),
            ("osmium sort".to_owned(), Duration::from_secs(120), 1),
            ("pbfhogg sort".to_owned(), Duration::from_secs(41), 1),
        ];
        let text = render_step_times(&mut times);
        let labels: Vec<&str> = text.split(", ").collect();
        assert_eq!(labels.len(), 3, "{text}");
        assert!(labels[0].starts_with("osmium sort "), "{text}");
        assert!(labels[1].starts_with("pbfhogg sort "), "{text}");
        assert!(
            labels[2].starts_with("pbfhogg diff ") && labels[2].ends_with(" (2 runs)"),
            "{text}"
        );
        assert_eq!(render_step_times(&mut []), "");
    }

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
        assert_eq!(diff_verdict(Some(0), false), Ok(Verdict::Pass));
    }

    #[test]
    fn diff_verdict_listing_is_fail_whatever_the_exit() {
        assert_eq!(diff_verdict(Some(0), true), Ok(Verdict::Fail));
        assert_eq!(diff_verdict(Some(1), true), Ok(Verdict::Fail));
        // Exit 1 with nothing on stdout still reports differences.
        assert_eq!(diff_verdict(Some(1), false), Ok(Verdict::Fail));
    }

    #[test]
    fn diff_verdict_crash_is_an_error_not_identical() {
        // The bug: a crashed diff with empty stdout read as "identical".
        assert!(diff_verdict(None, false).is_err());
        assert!(diff_verdict(Some(2), false).is_err());
        assert!(diff_verdict(Some(101), false).is_err());
    }

    #[test]
    fn only_a_sigkill_is_an_environment_kill() {
        let why = super::reference_kill(Some(libc::SIGKILL), "osmium sort").unwrap();
        assert!(why.contains("osmium sort"), "{why}");
        assert!(why.contains("OOM"), "{why}");
        assert!(super::reference_kill(Some(libc::SIGSEGV), "osmium sort").is_none());
        assert!(super::reference_kill(None, "osmium sort").is_none());
    }

    /// Serialises the tests that write the process-wide failure ledger, so
    /// one test's recorded failure never lands in another's check.
    static LEDGER_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ledger_lock() -> std::sync::MutexGuard<'static, ()> {
        LEDGER_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn a_failure_in_dropped_findings_still_fails_a_clean_return() {
        let _serial = ledger_lock();
        let status =
            super::run_check_status("x", true, || -> Result<(), crate::error::DevError> {
                let mut f = Findings::new();
                f.fail("lost");
                drop(f);
                Ok(())
            });
        assert!(matches!(status, Ok(super::CheckStatus::Failed)));
    }

    #[test]
    fn an_environment_error_cannot_erase_a_recorded_failure() {
        let _serial = ledger_lock();
        // The bug this guards: a check records a mismatch, then a later
        // reference run is OOM-killed and `?` unwinds past the findings.
        let failed = super::run_check_status("x", true, || -> Result<Findings, _> {
            let mut f = Findings::new();
            f.record("ways-only counts", Verdict::Fail);
            drop(f);
            Err(crate::error::DevError::VerifyEnvironment(
                "osmium killed".into(),
            ))
        });
        assert!(matches!(failed, Ok(super::CheckStatus::Failed)));

        let env = super::run_check_status("x", true, || -> Result<Findings, _> {
            Err(crate::error::DevError::VerifyEnvironment(
                "osmium killed".into(),
            ))
        });
        assert!(matches!(env, Ok(super::CheckStatus::Environment)));
        assert!(matches!(
            super::run_check("x", true, || -> Result<(), _> {
                Err(crate::error::DevError::VerifyEnvironment("killed".into()))
            }),
            Err(crate::error::DevError::ExitCode(
                super::ENVIRONMENT_EXIT_CODE
            ))
        ));
    }

    #[test]
    fn findings_fold_to_fail_on_any_recorded_failure() {
        let _serial = ledger_lock();
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
    fn a_claim_rides_a_pass_and_never_a_fail() {
        let _serial = ledger_lock();
        let f = Findings::claiming("order only");
        assert_eq!(f.into_check_result("x").unwrap(), Some("order only"));
        assert_eq!(Findings::new().into_check_result("x").unwrap(), None);
        let mut f = Findings::claiming("order only");
        f.fail("order");
        assert!(f.into_check_result("x").is_err());
        assert_eq!(
            super::pass_line("extract", "5ms", Some("order only")),
            "extract: PASS (5ms): order only"
        );
        assert_eq!(super::pass_line("sort", "5ms", None), "sort: PASS (5ms)");
    }

    #[test]
    fn unit_outcome_folds_to_ok() {
        assert!(().into_check_result("x").is_ok());
    }

    #[test]
    fn run_check_passes_an_interrupt_through_rather_than_failing() {
        let _serial = ledger_lock();
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
}
