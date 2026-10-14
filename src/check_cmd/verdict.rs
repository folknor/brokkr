// Final verdict, stop precedence, summary schema, and run timings.

use super::{
    ContinuationReport, DoctestAccounting, ExecutionAccounting, Phase, PolicyCoverage, RunReport,
    SummaryScope, TerminationCause, TerminationSummary, WATCHDOG_EXIT_CODE, fmt_wall,
    render_continuation,
};
#[cfg(test)]
use super::{DoctestObserved, PhaseFailure};
use crate::config::{Certifies, SitedAllow};
use crate::error::DevError;
use crate::output;

/// How the RUN was stopped, when something stopped it - decided from the
/// stop flags, independently of what the phases returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunStop {
    /// A time ceiling fired: exit 124.
    Watchdog,
    /// A cooperative interrupt (`brokkr kill`, Ctrl-C): exit 130.
    Interrupt,
}

/// The stop that decides the verdict. Independent of the phase result on
/// purpose: a watchdog can fire, or an interrupt arrive, after the last test
/// finished and the audit accepted the journal, and every phase after it then
/// spawns nothing that could fail - so the outcome reaching the verdict is
/// `Ok(())`. Branching on that outcome signed off a stopped run as `complete`
/// with exit 0.
pub(super) fn run_stop(
    outcome: &Result<(), DevError>,
    watchdog: bool,
    shutdown: bool,
) -> Option<RunStop> {
    if watchdog {
        Some(RunStop::Watchdog)
    } else if shutdown || matches!(outcome, Err(DevError::Interrupted)) {
        Some(RunStop::Interrupt)
    } else {
        None
    }
}

/// Why a check invocation failed after applying stop precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    Error,
    Deadline,
    Interrupt,
}

/// The human, JSON and exit-code forms of one completed check invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Passed { prose_only: bool },
    Complete,
    Partial { narrowed: Vec<String> },
    Failed(FailureKind),
}

impl Verdict {
    fn decide(
        certifies: Option<Certifies>,
        failure: Option<FailureKind>,
        skip_phases: &[String],
        prose_only: bool,
        package: Option<&str>,
    ) -> Self {
        if let Some(failure) = failure {
            return Self::Failed(failure);
        }
        match certifies {
            None => Self::Passed { prose_only },
            Some(Certifies::Complete) => Self::Complete,
            Some(Certifies::Partial) => {
                let mut narrowed = Vec::new();
                if !skip_phases.is_empty() {
                    narrowed.push(format!("skipped phases: {}", skip_phases.join(", ")));
                }
                if let Some(package) = package {
                    narrowed.push(format!("scoped to -p {package}"));
                }
                Self::Partial { narrowed }
            }
        }
    }

    fn word(&self) -> &'static str {
        match self {
            Self::Passed { .. } => "passed",
            Self::Complete => "complete",
            Self::Partial { .. } => "partial",
            Self::Failed(FailureKind::Error) => "failed",
            Self::Failed(FailureKind::Deadline) => "stopped by its time ceiling",
            Self::Failed(FailureKind::Interrupt) => "interrupted",
        }
    }

    fn json_word(&self) -> &'static str {
        match self {
            Self::Failed(_) => "failed",
            other => other.word(),
        }
    }

    fn partial_suffix(&self) -> String {
        let Self::Partial { narrowed } = self else {
            return String::new();
        };
        if narrowed.is_empty() {
            String::new()
        } else {
            format!(" ({})", narrowed.join("; "))
        }
    }

    fn exit(&self) -> Result<(), DevError> {
        match self {
            Self::Passed { .. } | Self::Complete => Ok(()),
            Self::Partial { .. } => Err(DevError::ExitCode(10)),
            Self::Failed(FailureKind::Error) => Err(DevError::ExitCode(1)),
            Self::Failed(FailureKind::Deadline) => Err(DevError::ExitCode(WATCHDOG_EXIT_CODE)),
            Self::Failed(FailureKind::Interrupt) => Err(DevError::Interrupted),
        }
    }
}

pub(super) struct FinishInput<'a> {
    pub(super) outcome: &'a Result<(), DevError>,
    pub(super) stop: Option<RunStop>,
    pub(super) certifies: Option<Certifies>,
    pub(super) profile_label: &'a Option<String>,
    pub(super) sweep_labels: &'a [&'a str],
    pub(super) lints: &'a crate::config::LintsConfig,
    pub(super) skip_phases: &'a [String],
    pub(super) prose_only: bool,
    pub(super) package: Option<&'a str>,
    pub(super) failing_phase: Option<Phase>,
    pub(super) report: RunReport,
    pub(super) json: bool,
    pub(super) started: std::time::Instant,
}

/// Print the summary line, emit the `--json` trailer, and return the enum's
/// exit contract. All run facts arrive together so the human and JSON forms
/// cannot select different outcomes.
#[allow(clippy::too_many_lines)]
pub(super) fn finish_check(input: FinishInput<'_>) -> Result<(), DevError> {
    let FinishInput {
        outcome,
        stop,
        certifies,
        profile_label,
        sweep_labels,
        lints,
        skip_phases,
        prose_only,
        package,
        failing_phase,
        mut report,
        json,
        started,
    } = input;

    // Nothing is running any more; drop the status line before the verdict so
    // it is not redrawn under it.
    output::disable_status_line();
    // A fired ceiling or an interrupt is how the RUN stopped, whatever a lane
    // recorded on its way down: it outranks the lane-level account.
    match stop {
        Some(RunStop::Watchdog) => {
            report.termination = Some(TerminationSummary {
                kind: TerminationCause::PhaseDeadline,
                scope: failing_phase.map_or(SummaryScope::Run, SummaryScope::Phase),
            });
        }
        Some(RunStop::Interrupt) => {
            report.termination = Some(TerminationSummary {
                kind: TerminationCause::Interrupt,
                scope: SummaryScope::Run,
            });
        }
        None => {}
    }
    let context = verdict_context(profile_label, sweep_labels.len(), lints);
    // A stopped run is a failed run whatever its phases returned (see
    // [`run_stop`]): the stop takes the failure path even from `Ok(())`.
    let stopped_ok = DevError::Interrupted;
    let failure = match (outcome, stop) {
        (Ok(()), None) => None,
        (Ok(()), Some(_)) => Some(&stopped_ok),
        (Err(error), _) => Some(error),
    };
    let failure_kind = failure.map(|_| match stop {
        Some(RunStop::Watchdog) => FailureKind::Deadline,
        Some(RunStop::Interrupt) => FailureKind::Interrupt,
        None => FailureKind::Error,
    });
    let verdict = Verdict::decide(certifies, failure_kind, skip_phases, prose_only, package);

    match (&verdict, failure) {
        (Verdict::Passed { prose_only }, None) => {
            // A run that passed has nothing to continue.
            report.continuation = None;
            let scope = if *prose_only {
                " (markdown only - build phases skipped)"
            } else {
                ""
            };
            output::result_msg(&format!(
                "check {}{scope} in {}{context}",
                verdict.word(),
                fmt_wall(started.elapsed())
            ));
            if json {
                emit_json_summary(
                    &verdict,
                    certifies,
                    profile_label,
                    sweep_labels,
                    package,
                    None,
                    *prose_only,
                    report,
                    started.elapsed(),
                );
            }
            verdict.exit()
        }
        (Verdict::Complete, None) => {
            report.continuation = None;
            output::result_msg(&format!(
                "check {} in {}{context}",
                verdict.word(),
                fmt_wall(started.elapsed())
            ));
            if json {
                emit_json_summary(
                    &verdict,
                    certifies,
                    profile_label,
                    sweep_labels,
                    package,
                    None,
                    false,
                    report,
                    started.elapsed(),
                );
            }
            verdict.exit()
        }
        (Verdict::Partial { .. }, None) => {
            report.continuation = None;
            output::result_msg(&format!(
                "check {} in {}{}{context}",
                verdict.word(),
                fmt_wall(started.elapsed()),
                verdict.partial_suffix()
            ));
            if json {
                emit_json_summary(
                    &verdict,
                    certifies,
                    profile_label,
                    sweep_labels,
                    package,
                    None,
                    false,
                    report,
                    started.elapsed(),
                );
            }
            verdict.exit()
        }
        (Verdict::Failed(kind), Some(error)) => {
            // A `Reported` failure printed its own detail above. Every other
            // error carries its diagnostic in the message and nobody has shown
            // it yet - the summary names it before the verdict.
            let stopped = *kind == FailureKind::Deadline;
            let interrupted = *kind == FailureKind::Interrupt;
            if !stopped
                && !interrupted
                && error.rendering_policy() != crate::error::RenderingPolicy::AlreadyPrinted
                && !matches!(error, DevError::ExitCode(_))
            {
                output::error(&error.to_string());
            }
            output::error(&format!(
                "check {} in {}{context}",
                verdict.word(),
                fmt_wall(started.elapsed())
            ));
            if let Some(continuation) = &report.continuation {
                output::error(&render_continuation(continuation).join("\n"));
            }
            if json {
                emit_json_summary(
                    &verdict,
                    certifies,
                    profile_label,
                    sweep_labels,
                    package,
                    failing_phase,
                    prose_only,
                    report,
                    started.elapsed(),
                );
            }
            verdict.exit()
        }
        _ => unreachable!("a failed verdict has a cause and a green verdict does not"),
    }
}

/// The verdict line's parenthesised context: the profile, how many sweeps
/// ran, and a bounded preview of the lint suppressions in force.
///
/// The suppressions are summarized here, once, rather than on each phase's line:
/// they narrow clippy (`-A`), rustdoc (dropped at ingestion) and every
/// compiling phase (test, coverage enumeration, install-feature, through
/// rustflags) alike, so a clause on any one phase's line would let the others
/// read as unsuppressed - and this is the one line every run prints, green or
/// red. `allow_exact` lists only the lint names: file-scoped in the diagnostic
/// phases, but the compiling phases have no per-file mechanism, so there it
/// applies build-wide - a fixed fact that lives in `docs/commands/check.md`,
/// not on the line. `cargo::` lints never reach a build, so they are marked
/// `(sited)`.
///
/// Empty when there is nothing to say (no profile, one sweep, no allows).
pub(super) fn verdict_context(
    profile: &Option<String>,
    sweeps: usize,
    lints: &crate::config::LintsConfig,
) -> String {
    let allow_exact: &[SitedAllow] = &lints.allow_exact;
    let allow: Vec<&str> = lints
        .allow
        .iter()
        .map(crate::config::check::LintName::as_str)
        .collect();
    let mut parts: Vec<String> = Vec::new();
    let mut run = Vec::new();
    if let Some(p) = profile {
        run.push(format!("profile {p}"));
    }
    if sweeps > 1 || (profile.is_some() && sweeps > 0) {
        run.push(output::count(sweeps, "sweep"));
    }
    if !run.is_empty() {
        parts.push(run.join(", "));
    }
    if !allow.is_empty() {
        parts.push(format!("lints allowed: {}", summarize_lint_names(&allow)));
    }
    if !allow_exact.is_empty() {
        // Only rustc's own lints reach a build (`test_phase_allow_flags`), so
        // only they widen there - each group gets its own scope.
        let mut widened: Vec<&str> = Vec::new();
        let mut sited: Vec<&str> = Vec::new();
        for s in allow_exact {
            let group = if crate::config::check::reaches_test_build(&s.lint) {
                &mut widened
            } else {
                &mut sited
            };
            if !group.contains(&s.lint.as_str()) {
                group.push(s.lint.as_str());
            }
        }
        let mut groups: Vec<String> = Vec::new();
        if !widened.is_empty() {
            // Where each is sited is constant: `brokkr man check` (the
            // `allow_exact` scope), not worth repeating on every verdict.
            groups.push(summarize_lint_names(&widened));
        }
        if !sited.is_empty() {
            groups.push(format!("{} (sited)", summarize_lint_names(&sited)));
        }
        parts.push(format!("allow_exact: {}", groups.join(", ")));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

fn summarize_lint_names(names: &[&str]) -> String {
    const LIMIT: usize = 6;
    let shown = names
        .iter()
        .take(LIMIT)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let omitted = names.len().saturating_sub(LIMIT);
    if omitted == 0 {
        shown
    } else {
        format!("{shown} (+{})", output::count(omitted, "additional lint"))
    }
}

#[cfg(test)]
mod verdict_context_tests {
    use super::summarize_lint_names;

    #[test]
    fn lint_context_caps_names_and_reports_the_omitted_count() {
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        assert_eq!(
            summarize_lint_names(&names),
            "a, b, c, d, e, f (+2 additional lints)"
        );
    }
}

/// The version of the `--json` summary. 2 split the old `coverage` object
/// into `policy_coverage` (what the profile selects) and
/// `execution_accounting` (what ran, execution by execution), added
/// `termination` and `doctests`, and dropped `coverage` - a semantic change,
/// since the old `run` count meant "a reached lane listed it".
const SUMMARY_SCHEMA: u32 = 2;

impl serde::Serialize for Certifies {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
        })
    }
}

/// The `--json` summary object: one line, last on stdout. Versioned and
/// additive within a version: fields are only ever added, consumers must
/// tolerate unknown ones, and a bump is reserved for renames or semantic
/// changes. `certifies` mirrors the resolved profile's claim (`null` for
/// unclaimed profiles); `verdict` is `passed`/`complete`/`partial`/`failed`,
/// paired with exit codes 0/0/10/1 - except a cooperatively interrupted run,
/// which reports `failed` and exits 130, and a run its time ceiling stopped,
/// which reports `failed` and exits 124.
#[derive(serde::Serialize)]
struct CheckSummary<'a> {
    schema: u32,
    certifies: Option<Certifies>,
    verdict: &'a str,
    profile: Option<&'a str>,
    sweeps: Vec<&'a str>,
    /// The CLI `-p` scope, when one narrowed the run - a consumer must be
    /// able to see that a green covered specific packages, not the
    /// workspace. A multi-package run joins the names with commas.
    package: Option<&'a str>,
    failed_phase: Option<Phase>,
    /// `"prose_only"` when the markdown-only shortcut skipped the build
    /// phases, `null` on a full run. Without it a shortened run's `passed`
    /// was indistinguishable from a full one to a machine reader, which is
    /// the one reader that never sees the human verdict line's
    /// "(markdown only - build phases skipped)".
    scope: Option<&'a str>,
    /// How the run stopped, when something stopped it: a time ceiling, an
    /// interrupt, a per-test timeout, the fail-fast after a failing lane.
    termination: Option<TerminationSummary>,
    /// Complete profiles only: whether every pair the profile could run is
    /// selected or justified. Present on a failed run too.
    policy_coverage: Option<PolicyCoverage>,
    /// Complete profiles only: every expected execution's outcome, counted.
    execution_accounting: Option<ExecutionAccounting>,
    /// Complete profiles only: what the doctest streams said. No inventory,
    /// so no accounting.
    doctests: Option<DoctestAccounting>,
    /// Failed runs that have an inventory, certifying or not: what the run
    /// left unresolved (every candidate's full execution identity, outcome
    /// and detail) and whether the inventory is whole. Diagnostic: it
    /// certifies nothing, and says so. `null` on a run that passed, one that left
    /// nothing unresolved, and one that failed before `prepare`.
    diagnostic_continuation: Option<ContinuationReport>,
    elapsed_ms: u64,
}

#[allow(clippy::too_many_arguments)]
fn check_summary<'a>(
    verdict: &Verdict,
    certifies: Option<Certifies>,
    profile: Option<&'a str>,
    sweeps: &[&'a str],
    package: Option<&'a str>,
    failed_phase: Option<Phase>,
    prose_only: bool,
    report: RunReport,
    elapsed: std::time::Duration,
) -> CheckSummary<'a> {
    let (policy_coverage, execution_accounting, doctests) = match report.accounting {
        Some(blocks) => (
            Some(blocks.policy_coverage),
            Some(blocks.execution_accounting),
            Some(blocks.doctests),
        ),
        None => (None, None, None),
    };
    CheckSummary {
        schema: SUMMARY_SCHEMA,
        certifies,
        verdict: verdict.json_word(),
        profile,
        sweeps: sweeps.to_vec(),
        package,
        failed_phase,
        scope: prose_only.then_some("prose_only"),
        termination: report.termination,
        policy_coverage,
        execution_accounting,
        doctests,
        diagnostic_continuation: report.continuation,
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_json_summary(
    verdict: &Verdict,
    certifies: Option<Certifies>,
    profile: &Option<String>,
    sweeps: &[&str],
    package: Option<&str>,
    failed_phase: Option<Phase>,
    prose_only: bool,
    report: RunReport,
    elapsed: std::time::Duration,
) {
    let summary = check_summary(
        verdict,
        certifies,
        profile.as_deref(),
        sweeps,
        package,
        failed_phase,
        prose_only,
        report,
        elapsed,
    );
    match serde_json::to_string(&summary) {
        // Unprefixed, but through the renderer so the run log records the
        // machine-readable verdict too.
        Ok(line) => output::plain(&line),
        Err(e) => output::error(&format!("--json summary serialization failed: {e}")),
    }
}

/// One test timing observation, tagged with its sweep label so the merged
/// descending list can show which sweep an entry came from.
pub(crate) struct TestTiming {
    pub(crate) sweep: String,
    pub(crate) name: String,
    pub(crate) elapsed: std::time::Duration,
}

pub(super) fn emit_timings(timings: &[TestTiming], multi_sweep: bool) {
    if timings.is_empty() {
        output::run_msg("timings: no tests ran");
        return;
    }

    let mut sorted: Vec<&TestTiming> = timings.iter().collect();
    sorted.sort_by_key(|t| std::cmp::Reverse(t.elapsed));

    let mut msg = format!(
        "timings: {}, slowest first\n",
        output::count(sorted.len(), "test")
    );
    for t in sorted {
        let secs = t.elapsed.as_secs_f64();
        if multi_sweep {
            msg.push_str(&format!("  {secs:>7.3}s [{}] {}\n", t.sweep, t.name));
        } else {
            msg.push_str(&format!("  {secs:>7.3}s {}\n", t.name));
        }
    }
    output::run_msg(msg.trim_end());
}

#[cfg(test)]
mod json_summary_tests {
    #![allow(clippy::unwrap_used)]

    use super::{
        CheckSummary, DoctestAccounting, DoctestObserved, ExecutionAccounting, FailureKind, Phase,
        PolicyCoverage, RunReport, SummaryScope, TerminationCause, TerminationSummary, Verdict,
        check_summary,
    };
    use crate::check_cmd::AccountingBlocks;
    use crate::config::Certifies;

    fn summary<'a>(
        verdict: &Verdict,
        profile: Option<&'a str>,
        package: Option<&'a str>,
        failed_phase: Option<Phase>,
        prose_only: bool,
        report: RunReport,
    ) -> CheckSummary<'a> {
        check_summary(
            verdict,
            None,
            profile,
            &["default", "ffi"],
            package,
            failed_phase,
            prose_only,
            report,
            std::time::Duration::from_millis(1234),
        )
    }

    #[test]
    fn summary_carries_schema_and_null_certifies() {
        let verdict = Verdict::Passed { prose_only: true };
        let s = summary(
            &verdict,
            Some("tier1"),
            None,
            None,
            true,
            RunReport::default(),
        );
        let line = serde_json::to_string(&s).unwrap();
        assert!(line.contains("\"scope\":\"prose_only\""), "{line}");
        // The two contract-critical fields: the version consumers key on,
        // and `certifies` present-but-null until certification exists.
        assert!(line.contains("\"schema\":2"), "{line}");
        assert!(line.contains("\"certifies\":null"), "{line}");
        assert!(line.contains("\"verdict\":\"passed\""), "{line}");
        assert!(line.contains("\"sweeps\":[\"default\",\"ffi\"]"), "{line}");
        // Unknown accounting is null, never zero.
        assert!(line.contains("\"execution_accounting\":null"), "{line}");
    }

    #[test]
    fn failed_summary_names_the_phase() {
        let verdict = Verdict::Failed(FailureKind::Error);
        let s = summary(
            &verdict,
            None,
            Some("nautilus-betfair"),
            Some(Phase::Clippy),
            false,
            RunReport::default(),
        );
        let line = serde_json::to_string(&s).unwrap();
        assert!(line.contains("\"verdict\":\"failed\""), "{line}");
        assert!(line.contains("\"failed_phase\":\"clippy\""), "{line}");
        assert!(line.contains("\"profile\":null"), "{line}");
        // The `-p` scope must be visible to consumers - a green that
        // covered one package may not be mistaken for a workspace green.
        assert!(line.contains("\"package\":\"nautilus-betfair\""), "{line}");
    }

    /// Schema 2's separate objects: policy coverage, execution accounting,
    /// the doctest block, and the termination - each its own claim.
    #[test]
    fn schema_two_keeps_policy_and_execution_apart() {
        let verdict = Verdict::Failed(FailureKind::Error);
        let report = RunReport {
            termination: Some(TerminationSummary {
                kind: TerminationCause::PerTestDeadline,
                scope: SummaryScope::Lane("serial".into()),
            }),
            accounting: Some(AccountingBlocks {
                policy_coverage: PolicyCoverage {
                    status: "passed",
                    plan_complete: true,
                    pairs: 10,
                    selected: 9,
                    ignored: 1,
                    quarantined: 0,
                    curated: 0,
                    orphaned: 0,
                    dead_filters: 0,
                },
                execution_accounting: ExecutionAccounting {
                    scope: "binary_tests",
                    status: "incomplete",
                    expected_executions: 9,
                    passed: 4,
                    failed: 0,
                    timed_out: 1,
                    interrupted: 1,
                    ignored: 0,
                    unobserved: 3,
                    anomalies: 0,
                },
                doctests: DoctestAccounting {
                    inventory: "unavailable",
                    accounting: "unknown",
                    observed: DoctestObserved::default(),
                },
            }),
            continuation: None,
        };
        let s = check_summary(
            &verdict,
            Some(Certifies::Complete),
            Some("tier1"),
            &["default", "ffi"],
            None,
            Some(Phase::Test),
            false,
            report,
            std::time::Duration::from_millis(1234),
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(v["termination"]["kind"], "per_test_timeout");
        assert_eq!(v["policy_coverage"]["status"], "passed");
        assert_eq!(v["policy_coverage"]["selected"], 9);
        assert_eq!(v["execution_accounting"]["status"], "incomplete");
        assert_eq!(v["execution_accounting"]["timed_out"], 1);
        assert_eq!(v["doctests"]["inventory"], "unavailable");
        assert!(v.get("coverage").is_none(), "the schema-1 object is gone");
    }
}

#[cfg(test)]
mod failure_exit_tests {
    use super::{
        Certifies, FinishInput, Phase, PhaseFailure, RunReport, RunStop, finish_check, run_stop,
    };
    use crate::error::DevError;

    fn finish(outcome: &Result<(), DevError>) -> Result<(), DevError> {
        finish_stopped(outcome, run_stop(outcome, false, false), None)
    }

    fn finish_stopped(
        outcome: &Result<(), DevError>,
        stop: Option<RunStop>,
        certifies: Option<Certifies>,
    ) -> Result<(), DevError> {
        finish_check(FinishInput {
            outcome,
            stop,
            certifies,
            profile_label: &None,
            sweep_labels: &[],
            lints: &crate::config::LintsConfig::default(),
            skip_phases: &[],
            prose_only: false,
            package: None,
            failing_phase: Some(Phase::Test),
            report: RunReport::default(),
            json: false,
            started: std::time::Instant::now(),
        })
    }

    /// A graceful `brokkr kill` must reach main as `Interrupted`, which is
    /// what buys exit 130 and the scratch cleanup - not a plain exit 1.
    #[test]
    fn an_interrupted_run_keeps_its_exit_path() {
        assert!(matches!(
            finish(&Err(DevError::Interrupted)),
            Err(DevError::Interrupted)
        ));
    }

    /// A watchdog that fires after the last test finished and the audit
    /// accepted the journal leaves the phases returning `Ok(())`. The stop
    /// decides the verdict anyway: exit 124, never `complete` with exit 0 -
    /// and an interrupt the same way, exit 130, complete claim or not.
    #[test]
    fn a_late_stop_fails_a_run_whose_phases_succeeded() {
        let ok: Result<(), DevError> = Ok(());
        assert_eq!(run_stop(&ok, true, false), Some(RunStop::Watchdog));
        assert_eq!(run_stop(&ok, false, true), Some(RunStop::Interrupt));
        assert_eq!(run_stop(&ok, false, false), None);
        for certifies in [Some(Certifies::Complete), Some(Certifies::Partial), None] {
            assert!(matches!(
                finish_stopped(&ok, Some(RunStop::Watchdog), certifies),
                Err(DevError::ExitCode(124))
            ));
            assert!(matches!(
                finish_stopped(&ok, Some(RunStop::Interrupt), certifies),
                Err(DevError::Interrupted)
            ));
        }
        assert!(finish_stopped(&ok, None, Some(Certifies::Complete)).is_ok());
    }

    #[test]
    fn any_other_failure_exits_one() {
        for e in [
            DevError::Reported("gremlins found".into()),
            DevError::Config("printed by the summary".into()),
        ] {
            assert!(matches!(finish(&Err(e)), Err(DevError::ExitCode(1))));
        }
    }

    #[test]
    fn typed_findings_keep_their_phase_at_the_command_boundary() {
        assert!(matches!(
            PhaseFailure::Findings(Phase::Clippy).into_dev_error(),
            DevError::Reported(message) if message == "clippy failed"
        ));
        assert!(matches!(
            PhaseFailure::from(DevError::Build("cargo metadata failed".into())),
            PhaseFailure::Error(DevError::Build(message)) if message == "cargo metadata failed"
        ));
    }
}
