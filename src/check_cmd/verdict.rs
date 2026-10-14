// Final verdict, stop precedence, summary schema, and run timings.

use super::{
    ContinuationReport, DoctestAccounting, ExecutionAccounting, PolicyCoverage, RunReport,
    TerminationCause, TerminationSummary, WATCHDOG_EXIT_CODE, render_continuation,
};
#[cfg(test)]
use super::{DoctestObserved, TESTS_FAILED, already_reported};
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

/// Print the summary line, emit the `--json` trailer, and map the claim to
/// the exit contract. The claim decides the word and the exit code: `passed`
/// stays with unclaimed legacy profiles (exactly as trustworthy as before
/// `certifies` existed), `complete` owns the gate verdict, and `partial` may
/// never print a success word a grep could mistake for one - it exits 10 so
/// naive `&& git commit` chaining fails closed. Any failure exits 1, except a
/// cooperative shutdown (`brokkr kill`), which keeps main's exit 130, and a
/// fired time ceiling, which exits 124.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn finish_check(
    outcome: &Result<(), DevError>,
    stop: Option<RunStop>,
    certifies: Option<Certifies>,
    profile_label: &Option<String>,
    sweep_labels: &[&str],
    lints: (&[String], &[SitedAllow]),
    skip_phases: &[String],
    prose_only: bool,
    package: Option<&str>,
    failing_phase: Option<&'static str>,
    mut report: RunReport,
    json: bool,
    started: std::time::Instant,
) -> Result<(), DevError> {
    // Nothing is running any more; drop the status line before the verdict so
    // it is not redrawn under it.
    output::disable_status_line();
    // A fired ceiling or an interrupt is how the RUN stopped, whatever a lane
    // recorded on its way down: it outranks the lane-level account.
    match stop {
        Some(RunStop::Watchdog) => {
            report.termination = Some(TerminationSummary {
                kind: TerminationCause::PhaseDeadline.as_str().to_owned(),
                scope: failing_phase.unwrap_or("run").to_owned(),
            });
        }
        Some(RunStop::Interrupt) => {
            report.termination = Some(TerminationSummary {
                kind: TerminationCause::Interrupt.as_str().to_owned(),
                scope: "run".to_owned(),
            });
        }
        None => {}
    }
    let context = verdict_context(profile_label, sweep_labels.len(), lints.0, lints.1);
    // A stopped run is a failed run whatever its phases returned (see
    // [`run_stop`]): the stop takes the failure path even from `Ok(())`.
    let stopped_ok = DevError::Interrupted;
    let failure = match (outcome, stop) {
        (Ok(()), None) => None,
        (Ok(()), Some(_)) => Some(&stopped_ok),
        (Err(e), _) => Some(e),
    };
    match failure {
        None => {
            // A run that passed has nothing to continue.
            report.continuation = None;
            let (word, scope, suffix, result) = match certifies {
                // A shortened run must not sign off in the same words a full
                // one does - the announcement at the top has scrolled away by
                // the time this line is read.
                None => {
                    let scope = if prose_only {
                        " (markdown only - build phases skipped)"
                    } else {
                        ""
                    };
                    ("passed", scope, String::new(), Ok(()))
                }
                Some(Certifies::Complete) => ("complete", "", String::new(), Ok(())),
                Some(Certifies::Partial) => {
                    let mut narrowed: Vec<String> = Vec::new();
                    if !skip_phases.is_empty() {
                        narrowed.push(format!("skipped phases: {}", skip_phases.join(", ")));
                    }
                    if let Some(p) = package {
                        narrowed.push(format!("scoped to -p {p}"));
                    }
                    let suffix = if narrowed.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", narrowed.join("; "))
                    };
                    ("partial", "", suffix, Err(DevError::ExitCode(10)))
                }
            };
            output::result_msg(&format!(
                "check {word}{scope} in {}{suffix}{context}",
                fmt_wall(started.elapsed())
            ));
            if json {
                emit_json_summary(
                    word,
                    certifies,
                    profile_label,
                    sweep_labels,
                    package,
                    None,
                    prose_only,
                    report,
                    started.elapsed(),
                );
            }
            result
        }
        Some(e) => {
            // A `Reported` failure printed its own detail above. Every other
            // error carries its diagnostic in the message and nobody has shown
            // it yet - this used to assume the former of every failure, and a
            // `cargo metadata` failure or a config refusal found at phase time
            // ended in a bare `check failed` naming nothing.
            // A fired watchdog killed the run's processes and raised the
            // shutdown flag; whatever error that surfaced as (an interrupt, a
            // killed test, a failed build) is its echo, not a cause to print.
            let stopped = stop == Some(RunStop::Watchdog);
            // Under a pending request, a failure is the interrupt's echo too: a
            // child killed by it can surface as a failed build or test before
            // anything polls the flag.
            let interrupted = stop == Some(RunStop::Interrupt);
            if !stopped
                && !interrupted
                && !e.already_printed()
                && !matches!(e, DevError::ExitCode(_))
            {
                output::error(&e.to_string());
            }
            let word = if stopped {
                "stopped by its time ceiling"
            } else if interrupted {
                "interrupted"
            } else {
                "failed"
            };
            output::error(&format!(
                "check {word} in {}{context}",
                fmt_wall(started.elapsed())
            ));
            // After the verdict, before the trailer: what the failed run left
            // unresolved, and the command that runs it again. Said for every
            // way a run fails - a kill, a timeout, a fail-fast, an interrupt.
            if let Some(c) = &report.continuation {
                output::error(&render_continuation(c).join("\n"));
            }
            if json {
                emit_json_summary(
                    "failed",
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
            // A graceful `brokkr kill` keeps its own exit: main maps
            // `Interrupted` to 130 and runs the scratch cleanup the
            // cooperative-shutdown contract promises. Folding it into exit 1
            // skipped both.
            if stopped {
                return Err(DevError::ExitCode(WATCHDOG_EXIT_CODE));
            }
            if interrupted {
                return Err(DevError::Interrupted);
            }
            // Exit 1 without main echoing a second, timing-less `[error]`.
            Err(DevError::ExitCode(1))
        }
    }
}

/// The verdict line's parenthesised context: the profile, how many sweeps
/// ran, and every lint suppression in force.
///
/// The suppressions are named here, once, rather than on each phase's line:
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
    allow: &[String],
    allow_exact: &[SitedAllow],
) -> String {
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
        parts.push(format!("lints allowed: {}", allow.join(", ")));
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
            groups.push(widened.join(", "));
        }
        if !sited.is_empty() {
            groups.push(format!("{} (sited)", sited.join(", ")));
        }
        parts.push(format!("allow_exact: {}", groups.join(", ")));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

/// The version of the `--json` summary. 2 split the old `coverage` object
/// into `policy_coverage` (what the profile selects) and
/// `execution_accounting` (what ran, execution by execution), added
/// `termination` and `doctests`, and dropped `coverage` - a semantic change,
/// since the old `run` count meant "a reached lane listed it".
const SUMMARY_SCHEMA: u32 = 2;

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
    certifies: Option<&'a str>,
    verdict: &'a str,
    profile: Option<&'a str>,
    sweeps: Vec<&'a str>,
    /// The CLI `-p` scope, when one narrowed the run - a consumer must be
    /// able to see that a green covered specific packages, not the
    /// workspace. A multi-package run joins the names with commas.
    package: Option<&'a str>,
    failed_phase: Option<&'a str>,
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
fn emit_json_summary(
    verdict: &str,
    certifies: Option<Certifies>,
    profile: &Option<String>,
    sweeps: &[&str],
    package: Option<&str>,
    failed_phase: Option<&'static str>,
    prose_only: bool,
    report: RunReport,
    elapsed: std::time::Duration,
) {
    let (policy_coverage, execution_accounting, doctests) = match report.accounting {
        Some(b) => (
            Some(b.policy_coverage),
            Some(b.execution_accounting),
            Some(b.doctests),
        ),
        None => (None, None, None),
    };
    let summary = CheckSummary {
        schema: SUMMARY_SCHEMA,
        certifies: certifies.map(|c| match c {
            Certifies::Complete => "complete",
            Certifies::Partial => "partial",
        }),
        verdict,
        profile: profile.as_deref(),
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
    };
    match serde_json::to_string(&summary) {
        // Unprefixed, but through the renderer so the run log records the
        // machine-readable verdict too.
        Ok(line) => output::plain(&line),
        Err(e) => output::error(&format!("--json summary serialization failed: {e}")),
    }
}

/// Format a wall-clock duration as a compact `1m23s` (or `8.4s` under a
/// minute) for the `brokkr check` summary line.
pub(super) fn fmt_wall(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    // Whole seconds straight from the duration - no float->int cast to trip
    // the truncation/sign-loss lints, and sub-second precision is noise at
    // this scale anyway.
    let total = d.as_secs();
    format!("{}m{:02}s", total / 60, total % 60)
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
        CheckSummary, DoctestAccounting, DoctestObserved, ExecutionAccounting, PolicyCoverage,
        SUMMARY_SCHEMA, TerminationSummary,
    };

    fn summary<'a>(verdict: &'a str, failed_phase: Option<&'a str>) -> CheckSummary<'a> {
        CheckSummary {
            schema: SUMMARY_SCHEMA,
            certifies: None,
            verdict,
            profile: Some("tier1"),
            sweeps: vec!["default", "ffi"],
            package: None,
            failed_phase,
            scope: None,
            termination: None,
            policy_coverage: None,
            execution_accounting: None,
            doctests: None,
            diagnostic_continuation: None,
            elapsed_ms: 1234,
        }
    }

    #[test]
    fn summary_carries_schema_and_null_certifies() {
        let mut s = summary("passed", None);
        s.scope = Some("prose_only");
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
        let mut s = summary("failed", Some("clippy"));
        s.profile = None;
        s.package = Some("nautilus-betfair");
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
        let mut s = summary("failed", Some("test"));
        s.termination = Some(TerminationSummary {
            kind: "per_test_timeout".into(),
            scope: "serial".into(),
        });
        s.policy_coverage = Some(PolicyCoverage {
            status: "passed",
            plan_complete: true,
            pairs: 10,
            selected: 9,
            ignored: 1,
            quarantined: 0,
            curated: 0,
            orphaned: 0,
            dead_filters: 0,
        });
        s.execution_accounting = Some(ExecutionAccounting {
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
        });
        s.doctests = Some(DoctestAccounting {
            inventory: "unavailable",
            accounting: "unknown",
            observed: DoctestObserved::default(),
        });
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
        Certifies, RunReport, RunStop, TESTS_FAILED, already_reported, finish_check, run_stop,
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
        finish_check(
            outcome,
            stop,
            certifies,
            &None,
            &[],
            (&[], &[]),
            &[],
            false,
            None,
            Some("test"),
            RunReport::default(),
            false,
            std::time::Instant::now(),
        )
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
    fn only_the_named_sentinel_becomes_reported() {
        assert!(matches!(
            already_reported(DevError::Build(TESTS_FAILED.into()), TESTS_FAILED),
            DevError::Reported(_)
        ));
        // A different message is a diagnostic nobody has printed yet.
        assert!(matches!(
            already_reported(
                DevError::Build("cargo metadata failed".into()),
                TESTS_FAILED
            ),
            DevError::Build(_)
        ));
        assert!(matches!(
            already_reported(DevError::Interrupted, TESTS_FAILED),
            DevError::Interrupted
        ));
    }
}
