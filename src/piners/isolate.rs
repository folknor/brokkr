//! Diagnostic isolation: after an abnormal harness exit, bisect the probes
//! the original run left unreported to find which ones abort on their own.
//!
//! Evidence only, never scoring. Nothing an attempt reports enters the
//! dispositions, the gate or bless; the original run
//! stays the authoritative record, and the diagnosis is attached beside it.
//!
//! The planner here is pure: it decides which subsets to run and what the
//! outcomes mean, and leaves running them to an injected `run` callback
//! (`cmd.rs` supplies one that spawns the built harness; the tests supply a
//! fake). Bisection is breadth-first: a subset that does not complete is
//! split in two and each half run alone; a half that completes is cleared; a
//! singleton that does not complete is filed with how: it aborts when run
//! alone only on an abnormal end, and otherwise is incomplete (exit 0/1, no
//! valid disposition) or protocol-invalid (exit 0/1, a broken report).
//! Both halves of a failing set completing means the abort does not
//! reproduce in either half - it needs probes from both, or it is not
//! deterministic - and that set is reported as such rather than guessed at.
//!
//! Probes overlap inside the harness, so nothing here is causation for the
//! original run: a singleton that aborts alone is a reproduction, and the
//! wording ([`Diagnosis::lines`]) says exactly that and no more.

use std::collections::VecDeque;
use std::path::PathBuf;

use crate::piners::integrity::{HarnessEnd, RefusedProbe};

/// Every diagnostic invocation counts against this, singletons included.
pub const ATTEMPT_CAP: usize = 64;

/// One diagnostic attempt: how it went, and every refusal its captured
/// stream showed. The refusals ride beside the outcome rather than inside a
/// status, so an attempt that aborted, broke protocol or stopped diagnosis
/// keeps them as surely as one that completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub outcome: AttemptOutcome,
    pub refusals: Vec<RefusedProbe>,
}

impl From<AttemptOutcome> for Attempt {
    fn from(outcome: AttemptOutcome) -> Self {
        Self {
            outcome,
            refusals: Vec::new(),
        }
    }
}

/// A refusal one attempt reported, with the attempt it came from: its
/// subset, stdout and stderr are under `attempt-<n>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalObservation {
    pub attempt: usize,
    pub refusal: RefusedProbe,
}

/// How one attempt went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Exit 0 or 1, a valid disposition for every probe in the subset, no
    /// protocol violation and an intact stream. An `unevaluated` probe is a
    /// valid disposition, so a subset holding one completes.
    Completed,
    /// The subset did not complete; `failure` says how. `end` is how the
    /// harness ended.
    Failed {
        end: HarnessEnd,
        failure: Failure,
        stderr: String,
        dir: PathBuf,
    },
    /// Diagnosis cannot continue; the subset is unresolved, never blamed.
    Stop(StopReason),
}

/// How an attempt failed to complete. Only [`Failure::Aborted`] is a
/// spontaneous abort; the other two exited 0/1 and are never called one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// An abnormal end: exit 2, another unexpected code, or a signal.
    Aborted,
    /// Exit 0/1, but some probe of the subset has no valid disposition. The
    /// payload is the reconciliation summary.
    Incomplete(String),
    /// Exit 0/1, but the report broke protocol (invalid, repeated or extra
    /// records) or the stream was cut short. The payload names what.
    ProtocolInvalid(String),
}

impl Failure {
    fn tag(&self) -> &'static str {
        match self {
            Failure::Aborted => "aborts",
            Failure::Incomplete(_) => "incomplete",
            Failure::ProtocolInvalid(_) => "protocol-invalid",
        }
    }
}

/// Why diagnosis stopped before resolving every set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The [`ATTEMPT_CAP`] was reached.
    Cap,
    /// The shared deadline (from the original launch) ran out, or an attempt
    /// was killed at it. A diagnostic timeout is never attributed to a probe.
    Deadline,
    /// Interrupt or requested shutdown.
    Interrupted,
    /// An attempt could not be started.
    SpawnFailed(String),
    /// An attempt's stdout or stderr exceeded its bound, so its evidence was
    /// incomplete and the subset result could not be trusted.
    CaptureIncomplete(String),
    /// An attempt could not be prepared (its dir or manifest).
    Setup(String),
    /// An attempt's captured streams could not be stored, so its evidence
    /// would not survive; diagnosis stops rather than claim it.
    EvidenceStorage(String),
    /// An attempt's harness reported a `run_error` (rendered): a failure of
    /// its own setup, not of the subset, so there is nothing to bisect.
    HarnessFailure(String),
    /// An attempt ended at a `--stop` marker, which no diagnostic attempt
    /// sets: an intentional stop is never an abort to attribute.
    StopMarker,
}

/// The policy result for one diagnostic harness invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptVerdict {
    /// Every selected probe validly reported, the stream was whole, and the
    /// harness ended with an accepted status.
    Completed,
    /// The attempt did not complete and has an attribution or protocol result.
    Failed(Failure),
    /// Diagnosis cannot continue because the harness reported its own error.
    Stop(StopReason),
}

/// Classify one isolation attempt under the policy in the shared assessment.
pub fn classify_attempt(assessment: &crate::piners::integrity::RunAssessment) -> AttemptVerdict {
    assessment.isolation_verdict()
}

impl StopReason {
    pub fn describe(&self) -> String {
        match self {
            StopReason::Cap => format!("the {ATTEMPT_CAP}-attempt cap was reached"),
            StopReason::Deadline => "the shared deadline (the hang backstop, counted from the \
                                     original launch) ran out"
                .to_owned(),
            StopReason::Interrupted => "interrupted".to_owned(),
            StopReason::SpawnFailed(e) => format!("an attempt failed to spawn: {e}"),
            StopReason::CaptureIncomplete(e) => {
                format!("an attempt produced incomplete captured output: {e}")
            }
            StopReason::Setup(e) => format!("an attempt could not be prepared: {e}"),
            StopReason::EvidenceStorage(e) => {
                format!("an attempt's evidence could not be stored: {e}")
            }
            StopReason::HarnessFailure(e) => format!("an attempt's harness failed: {e}"),
            StopReason::StopMarker => "an attempt was ended at a --stop marker".to_owned(),
        }
    }

    /// The one-word status recorded in the run store.
    pub fn status(&self) -> &'static str {
        match self {
            StopReason::Cap => "stopped at the attempt cap",
            StopReason::Deadline => "stopped at the deadline",
            StopReason::Interrupted => "interrupted",
            StopReason::SpawnFailed(_) => "stopped on a spawn failure",
            StopReason::CaptureIncomplete(_) => "stopped on incomplete capture",
            StopReason::Setup(_) => "stopped on a setup failure",
            StopReason::EvidenceStorage(_) => "stopped on an evidence-storage failure",
            StopReason::HarnessFailure(_) => "stopped on a harness run_error",
            StopReason::StopMarker => "stopped on a stop marker",
        }
    }
}

/// A probe that did not complete when run alone, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Singleton {
    pub probe: String,
    pub end: HarnessEnd,
    pub failure: Failure,
    pub stderr: String,
    pub dir: PathBuf,
}

/// A set whose run failed in some way while both of its halves completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotReproduced {
    pub set: Vec<String>,
    /// How the set itself failed (the original run counts as an abort).
    pub failure: Failure,
    /// For a one-element set, the attempt that ran its probe alone and
    /// completed, with what that attempt itself said of it.
    pub alone: Option<Alone>,
}

/// A singleton's own completed attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alone {
    pub attempt: usize,
    /// The probe's refusal in this attempt, when it reported `unevaluated`.
    pub refusal: Option<RefusedProbe>,
}

/// A completed attempt, for the set that ran it.
struct Cleared {
    attempt: usize,
    refusals: Vec<RefusedProbe>,
}

/// What the isolation pass found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    /// The original unreported ids, the set bisection started from.
    pub bisected: Vec<String>,
    /// Diagnostic invocations made.
    pub attempts: usize,
    /// Singletons run alone (each failing one is in `failing`).
    pub singletons_tested: usize,
    /// Probes that did not complete when run alone, in discovery order,
    /// each with how (an abort, incomplete, or protocol-invalid).
    pub failing: Vec<Singleton>,
    /// Failing sets both of whose halves completed: the failure did not
    /// reproduce in either half. A singleton that completed alone is listed
    /// here as a one-element set.
    pub not_reproduced: Vec<NotReproduced>,
    /// Every refusal any attempt reported, completed or not, in attempt
    /// order, each with its attempt: the experiment could not be conducted,
    /// which is not an abort, and never a pass. Never deduplicated, so one
    /// probe refused for two reasons keeps both.
    pub refusals: Vec<RefusalObservation>,
    /// Sets left unexamined when diagnosis stopped.
    pub unresolved: Vec<Vec<String>>,
    /// Why diagnosis stopped early, if it did.
    pub stopped: Option<StopReason>,
}

/// Bisect `unreported`, calling `run(subset, attempt_number)` for each
/// attempt (numbered from 1). Stops at [`ATTEMPT_CAP`] or on the first
/// [`AttemptOutcome::Stop`], leaving what is left as unresolved.
pub fn bisect<A: Into<Attempt>>(
    unreported: Vec<String>,
    mut run: impl FnMut(&[String], usize) -> A,
) -> Diagnosis {
    let mut run = |subset: &[String], n: usize| run(subset, n).into();
    let mut d = Diagnosis {
        bisected: unreported.clone(),
        ..Diagnosis::default()
    };
    if unreported.is_empty() {
        return d;
    }
    // Each queued set carries how it failed; the original run was an abort.
    let mut queue: Queue = VecDeque::from([(unreported, Failure::Aborted)]);
    while let Some((set, failure)) = queue.pop_front() {
        if d.stopped.is_some() {
            d.unresolved.push(set);
            continue;
        }
        if set.len() == 1 {
            // Only reached when the original unreported set is one probe;
            // halves of size one are examined directly below.
            if let Some(cleared) = examine(&mut d, &mut run, set.clone(), &mut queue) {
                let alone = alone(&set[0], cleared);
                d.not_reproduced.push(NotReproduced {
                    set,
                    failure,
                    alone: Some(alone),
                });
            }
            continue;
        }
        let mid = set.len() / 2;
        let first = examine(&mut d, &mut run, set[..mid].to_vec(), &mut queue);
        let second = examine(&mut d, &mut run, set[mid..].to_vec(), &mut queue);
        if first.is_some() && second.is_some() {
            d.not_reproduced.push(NotReproduced {
                set,
                failure,
                alone: None,
            });
        }
    }
    d
}

/// What a singleton's own completed attempt said of its probe.
fn alone(probe: &str, cleared: Cleared) -> Alone {
    Alone {
        attempt: cleared.attempt,
        refusal: cleared.refusals.into_iter().find(|r| r.probe == probe),
    }
}

/// Sets awaiting a split, each with how it failed.
type Queue = VecDeque<(Vec<String>, Failure)>;

/// Run `subset` as one attempt, under the cap, and file the outcome: a
/// failing multi-probe subset is queued for splitting, a failing singleton is
/// a failing probe (filed with how it failed), a stop parks the subset as
/// unresolved. Every refusal the attempt reported is kept, whatever its
/// outcome. Returns the completed attempt, or `None` when the subset did not
/// complete.
fn examine(
    d: &mut Diagnosis,
    run: &mut impl FnMut(&[String], usize) -> Attempt,
    subset: Vec<String>,
    queue: &mut Queue,
) -> Option<Cleared> {
    if d.stopped.is_some() {
        d.unresolved.push(subset);
        return None;
    }
    if d.attempts >= ATTEMPT_CAP {
        d.stopped = Some(StopReason::Cap);
        d.unresolved.push(subset);
        return None;
    }
    d.attempts += 1;
    let attempt = d.attempts;
    let Attempt { outcome, refusals } = run(&subset, attempt);
    d.refusals
        .extend(refusals.iter().cloned().map(|refusal| RefusalObservation { attempt, refusal }));
    match outcome {
        AttemptOutcome::Completed => {
            if subset.len() == 1 {
                d.singletons_tested += 1;
            }
            Some(Cleared { attempt, refusals })
        }
        AttemptOutcome::Failed {
            end,
            failure,
            stderr,
            dir,
        } => {
            if subset.len() == 1 {
                d.singletons_tested += 1;
                d.failing.push(Singleton {
                    probe: subset[0].clone(),
                    end,
                    failure,
                    stderr,
                    dir,
                });
            } else {
                queue.push_back((subset, failure));
            }
            None
        }
        AttemptOutcome::Stop(reason) => {
            d.stopped = Some(reason);
            d.unresolved.push(subset);
            None
        }
    }
}

/// Observations of one probe refused for one reason and detail, with every
/// attempt that reported it, in first-seen order.
struct RefusalGroup<'a> {
    refusal: &'a RefusedProbe,
    attempts: Vec<usize>,
}

fn group_refusals(observations: &[RefusalObservation]) -> Vec<RefusalGroup<'_>> {
    let mut groups: Vec<RefusalGroup<'_>> = Vec::new();
    for o in observations {
        match groups.iter_mut().find(|g| *g.refusal == o.refusal) {
            Some(g) => g.attempts.push(o.attempt),
            None => groups.push(RefusalGroup {
                refusal: &o.refusal,
                attempts: vec![o.attempt],
            }),
        }
    }
    groups
}

fn attempt_list(attempts: &[usize]) -> String {
    let numbers: Vec<String> = attempts.iter().map(usize::to_string).collect();
    let noun = if attempts.len() == 1 {
        "attempt"
    } else {
        "attempts"
    };
    format!("{noun} {}", numbers.join(", "))
}

impl Diagnosis {
    /// The `[corpus]` lines reporting the diagnosis. Claims only what the
    /// attempts showed: a probe aborts when run alone, an abort did not
    /// reproduce in either half of a set, every tested singleton aborts.
    /// Only a spontaneous abort is called one. Each failing singleton's
    /// stderr follows it whole; identical stderr
    /// is printed once, with the probes it belongs to.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!(
            "isolation: bisecting the {} the original run left unreported \
             (diagnostic only - nothing here is scored); {} made",
            crate::output::count(self.bisected.len(), "selected probe"),
            crate::output::count(self.attempts, "attempt")
        ));

        // Only spontaneous aborts count here: a singleton that exited 0/1
        // without a valid or protocol-clean report did not abort.
        let aborts = self
            .failing
            .iter()
            .filter(|s| s.failure == Failure::Aborted)
            .count();
        if self.singletons_tested >= 2 && aborts == self.singletons_tested {
            out.push(format!("every tested singleton aborts ({aborts})"));
        }
        // Group by identical stderr so N copies of one startup error print once.
        let mut groups: Vec<(&str, Vec<&Singleton>)> = Vec::new();
        for s in &self.failing {
            match groups.iter_mut().find(|(e, _)| *e == s.stderr.as_str()) {
                Some((_, members)) => members.push(s),
                None => groups.push((s.stderr.as_str(), vec![s])),
            }
        }
        for (stderr, members) in &groups {
            for s in members {
                out.push(format!(
                    "probe {} ({}; artefacts {})",
                    singleton_line(s),
                    s.end.short(),
                    s.dir.display()
                ));
            }
            let whose = if members.len() == 1 {
                format!("stderr of {} run alone", members[0].probe)
            } else {
                format!("stderr (identical for {} singletons)", members.len())
            };
            if stderr.trim().is_empty() {
                out.push(format!("{whose}: (empty)"));
            } else {
                out.push(format!("{whose}:"));
                out.extend(stderr.lines().map(|l| format!("  {l}")));
            }
        }
        for nr in &self.not_reproduced {
            let what = failure_noun(&nr.failure);
            match (&nr.alone, nr.set.as_slice()) {
                (Some(Alone { refusal: Some(r), attempt }), [probe]) => out.push(format!(
                    "probe {probe} reported unevaluated ({}) when run alone in attempt \
                     {attempt} ({what} did not reproduce)",
                    r.reason.label()
                )),
                (_, [probe]) => out.push(format!(
                    "probe {probe} completed when run alone ({what} did not reproduce)"
                )),
                _ => out.push(format!(
                    "{what} did not reproduce in either half of {{{}}}",
                    nr.set.join(", ")
                )),
            }
        }
        for g in group_refusals(&self.refusals) {
            out.push(format!(
                "unevaluated (the experiment could not be conducted; not an abort, never a \
                 pass): {} in {}: {}",
                g.refusal.short(),
                attempt_list(&g.attempts),
                g.refusal.error
            ));
        }
        if let Some(reason) = &self.stopped {
            out.push(format!("isolation stopped: {}", reason.describe()));
            for set in &self.unresolved {
                out.push(format!("  unresolved: {{{}}}", set.join(", ")));
            }
        }
        if self.failing.is_empty()
            && self.not_reproduced.is_empty()
            && self.refusals.is_empty()
            && self.stopped.is_none()
        {
            out.push("isolation found nothing to report".to_owned());
        }
        out
    }

    /// The compact note stored beside the original run's `fail_reason`.
    pub fn note(&self, artefacts: &std::path::Path) -> String {
        let status = match &self.stopped {
            None => "completed".to_owned(),
            Some(r) => r.status().to_owned(),
        };
        let mut out = format!(
            "isolation {status} after {} over {}",
            crate::output::count(self.attempts, "attempt"),
            crate::output::count(self.bisected.len(), "unreported probe")
        );
        // One clause per failure kind, so the stored note never calls an
        // incomplete or protocol-invalid singleton an abort.
        for (kind, label) in [
            ("aborts", "aborts when run alone"),
            ("incomplete", "no valid disposition when run alone"),
            (
                "protocol-invalid",
                "broke the report protocol when run alone",
            ),
        ] {
            let named: Vec<String> = self
                .failing
                .iter()
                .filter(|s| s.failure.tag() == kind)
                .map(|s| format!("{} ({})", s.probe, s.end.short()))
                .collect();
            if !named.is_empty() {
                out.push_str(&format!("; {label}: {}", named.join(", ")));
            }
        }
        let refused: Vec<String> = group_refusals(&self.refusals)
            .iter()
            .map(|g| format!("{} in {}", g.refusal.short(), attempt_list(&g.attempts)))
            .collect();
        if !refused.is_empty() {
            out.push_str(&format!("; unevaluated: {}", refused.join(", ")));
        }
        if !self.not_reproduced.is_empty() {
            out.push_str(&format!(
                "; did not reproduce in {}",
                crate::output::count(self.not_reproduced.len(), "set")
            ));
        }
        if !self.unresolved.is_empty() {
            out.push_str(&format!(
                "; {} unresolved",
                crate::output::count(self.unresolved.len(), "set")
            ));
        }
        // A stop that carries a message keeps it in the note.
        if let Some(reason) = &self.stopped
            && !matches!(
                reason,
                StopReason::Cap | StopReason::Deadline | StopReason::Interrupted
            )
        {
            out.push_str(&format!("; {}", reason.describe()));
        }
        out.push_str(&format!("; artefacts {}", crate::output::path(artefacts)));
        out
    }
}

/// `X aborts when run alone` only for a spontaneous abort; the exit-0/1
/// failures say what they were instead.
fn singleton_line(s: &Singleton) -> String {
    match &s.failure {
        Failure::Aborted => format!("{} aborts when run alone", s.probe),
        Failure::Incomplete(why) => {
            format!(
                "{} reported no valid disposition when run alone ({why})",
                s.probe
            )
        }
        Failure::ProtocolInvalid(why) => {
            format!(
                "{} broke the report protocol when run alone ({why})",
                s.probe
            )
        }
    }
}

/// What failed to reproduce, by how the set had failed.
fn failure_noun(f: &Failure) -> &'static str {
    match f {
        Failure::Aborted => "abort",
        Failure::Incomplete(_) => "incomplete report",
        Failure::ProtocolInvalid(_) => "protocol violation",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::collections::HashSet;

    use super::*;

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("p{i}")).collect()
    }

    /// An abnormal `end` fails as an abort.
    fn failed(end: HarnessEnd, stderr: &str, n: usize) -> AttemptOutcome {
        failed_with(end, Failure::Aborted, stderr, n)
    }

    fn failed_with(end: HarnessEnd, failure: Failure, stderr: &str, n: usize) -> AttemptOutcome {
        AttemptOutcome::Failed {
            end,
            failure,
            stderr: stderr.to_owned(),
            dir: PathBuf::from(format!("run-1/attempt-{n}")),
        }
    }

    /// A fake harness: a subset aborts iff it contains any of `crashers`.
    fn crash_on(crashers: &[&str]) -> impl FnMut(&[String], usize) -> AttemptOutcome {
        let set: HashSet<String> = crashers.iter().map(|s| (*s).to_owned()).collect();
        move |subset: &[String], n: usize| {
            if subset.iter().any(|p| set.contains(p)) {
                failed(HarnessEnd::Code(2), "boom", n)
            } else {
                AttemptOutcome::Completed
            }
        }
    }

    #[test]
    fn bisection_finds_a_single_crasher() {
        let d = bisect(ids(16), crash_on(&["p11"]));
        assert_eq!(d.failing.len(), 1);
        assert_eq!(d.failing[0].probe, "p11");
        assert!(d.stopped.is_none());
        assert!(d.not_reproduced.is_empty());
        // log2(16) levels, two attempts each.
        assert_eq!(d.attempts, 8);
        let lines = d.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("probe p11 aborts when run alone (exit 2;")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "stderr of p11 run alone:"),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l == "  boom"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("every tested singleton")));
    }

    #[test]
    fn bisection_finds_two_crashers_in_different_halves() {
        let d = bisect(ids(8), crash_on(&["p1", "p6"]));
        let found: Vec<&str> = d.failing.iter().map(|s| s.probe.as_str()).collect();
        assert_eq!(found, vec!["p1", "p6"]);
    }

    #[test]
    fn an_interaction_crash_is_reported_as_not_reproducing() {
        // Aborts only when p0 and p3 run together: each half completes.
        let mut run = |subset: &[String], n: usize| {
            let has = |p: &str| subset.iter().any(|s| s == p);
            if has("p0") && has("p3") {
                failed(HarnessEnd::Signal(11), "segv", n)
            } else {
                AttemptOutcome::Completed
            }
        };
        let d = bisect(ids(4), &mut run);
        assert!(d.failing.is_empty());
        assert_eq!(
            d.not_reproduced,
            vec![NotReproduced {
                set: ids(4),
                failure: Failure::Aborted,
                alone: None
            }]
        );
        let lines = d.lines();
        assert!(
            lines
                .iter()
                .any(|l| l == "abort did not reproduce in either half of {p0, p1, p2, p3}"),
            "{lines:?}"
        );
    }

    #[test]
    fn every_singleton_aborting_is_said_without_calling_it_shared() {
        let d = bisect(ids(4), |_, n| failed(HarnessEnd::Code(2), "no feed", n));
        assert_eq!(d.singletons_tested, 4);
        assert_eq!(d.failing.len(), 4);
        let lines = d.lines();
        assert!(
            lines
                .iter()
                .any(|l| l == "every tested singleton aborts (4)"),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("shared")), "{lines:?}");
        // Identical stderr printed once, attributed to all four.
        assert_eq!(
            lines.iter().filter(|l| *l == "  no feed").count(),
            1,
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "stderr (identical for 4 singletons):")
        );
    }

    #[test]
    fn a_lone_unreported_probe_is_run_by_itself() {
        let d = bisect(ids(1), |_, n| failed(HarnessEnd::Signal(6), "abort", n));
        assert_eq!(d.attempts, 1);
        assert_eq!(d.failing[0].probe, "p0");
        assert!(
            d.lines()
                .iter()
                .any(|l| l.starts_with("probe p0 aborts when run alone (signal 6;"))
        );
        let ok = bisect(ids(1), |_, _| AttemptOutcome::Completed);
        assert!(
            ok.lines()
                .iter()
                .any(|l| l == "probe p0 completed when run alone (abort did not reproduce)")
        );
    }

    #[test]
    fn a_singleton_that_exits_clean_without_reporting_is_not_called_an_abort() {
        let d = bisect(ids(1), |_, n| {
            failed_with(
                HarnessEnd::Code(0),
                Failure::Incomplete("1 selected, 0 scored".to_owned()),
                "",
                n,
            )
        });
        let lines = d.lines();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("probe p0 reported no valid disposition when run alone")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("aborts")), "{lines:?}");
        let note = d.note(std::path::Path::new("run-1"));
        assert!(
            note.contains("; no valid disposition when run alone: p0 (exit 0)"),
            "{note}"
        );
        assert!(!note.contains("aborts"), "{note}");
    }

    #[test]
    fn incomplete_and_protocol_invalid_singletons_are_not_counted_as_aborts() {
        // Two singletons: one aborts, one exits 0 with an extra record.
        let d = bisect(ids(2), |subset: &[String], n: usize| {
            if subset[0] == "p0" {
                failed(HarnessEnd::Signal(11), "segv", n)
            } else {
                failed_with(
                    HarnessEnd::Code(0),
                    Failure::ProtocolInvalid("1 report-only extra".to_owned()),
                    "",
                    n,
                )
            }
        });
        assert_eq!(d.singletons_tested, 2);
        let lines = d.lines();
        assert!(
            !lines.iter().any(|l| l.contains("every tested singleton")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("probe p0 aborts when run alone (signal 11;"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("probe p1 broke the report protocol when run alone"))
        );
        let note = d.note(std::path::Path::new("run-1"));
        assert!(
            note.contains("; aborts when run alone: p0 (signal 11)"),
            "{note}"
        );
        assert!(
            note.contains("; broke the report protocol when run alone: p1 (exit 0)"),
            "{note}"
        );
        // Every singleton exiting 0 without a report is not "every ... aborts".
        let all_incomplete = bisect(ids(2), |_, n| {
            failed_with(
                HarnessEnd::Code(0),
                Failure::Incomplete(String::new()),
                "",
                n,
            )
        });
        assert!(
            !all_incomplete
                .lines()
                .iter()
                .any(|l| l.contains("every tested singleton"))
        );
    }

    #[test]
    fn a_harness_run_error_stops_diagnosis_and_is_never_bisected() {
        let mut calls = 0;
        let d = bisect(ids(4), |_, _| {
            calls += 1;
            AttemptOutcome::Stop(StopReason::HarnessFailure(
                "run_error at stage feed_load: oom".to_owned(),
            ))
        });
        assert_eq!(calls, 1);
        assert!(d.failing.is_empty());
        let note = d.note(std::path::Path::new("r"));
        assert!(
            note.starts_with("isolation stopped on a harness run_error"),
            "{note}"
        );
        assert!(note.contains("run_error at stage feed_load: oom"), "{note}");
        assert!(!d.lines().iter().any(|l| l.contains("aborts")));
    }

    #[test]
    fn evidence_storage_failure_stops_with_its_reason() {
        let d = bisect(ids(4), |_, _| {
            AttemptOutcome::Stop(StopReason::EvidenceStorage("disk full".to_owned()))
        });
        assert!(d.failing.is_empty());
        assert!(d.lines().iter().any(
            |l| l == "isolation stopped: an attempt's evidence could not be stored: disk full"
        ));
        assert!(
            d.note(std::path::Path::new("r"))
                .starts_with("isolation stopped on an evidence-storage failure")
        );
    }

    #[test]
    fn the_cap_stops_diagnosis_and_lists_the_unresolved_sets() {
        // Everything aborts: a full binary tree over 100 probes needs more
        // than the cap allows.
        let d = bisect(ids(100), |_, n| failed(HarnessEnd::Code(2), "x", n));
        assert_eq!(d.attempts, ATTEMPT_CAP);
        assert_eq!(d.stopped, Some(StopReason::Cap));
        assert!(!d.unresolved.is_empty());
        let lines = d.lines();
        assert!(
            lines.iter().any(|l| l.contains("64-attempt cap")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.starts_with("  unresolved: {")));
    }

    #[test]
    fn a_stop_is_never_attributed_and_ends_the_pass() {
        let mut calls = 0;
        let d = bisect(ids(8), |_, n| {
            calls += 1;
            if n == 2 {
                AttemptOutcome::Stop(StopReason::Deadline)
            } else {
                failed(HarnessEnd::Code(2), "x", n)
            }
        });
        // Attempt 1 (first half) failed, attempt 2 hit the deadline; nothing more ran.
        assert_eq!(calls, 2);
        assert_eq!(d.stopped, Some(StopReason::Deadline));
        assert!(d.failing.is_empty());
        // The deadline half is unresolved; the failed half queued for splitting
        // is unresolved too.
        assert_eq!(d.unresolved.len(), 2, "{:?}", d.unresolved);
        assert!(
            d.note(std::path::Path::new("run-1"))
                .starts_with("isolation stopped at the deadline after 2 attempts")
        );
    }

    #[test]
    fn nothing_unreported_runs_nothing() {
        let d = bisect(Vec::new(), |_, _| -> AttemptOutcome { panic!("must not run") });
        assert_eq!(d.attempts, 0);
    }

    fn refused(probe: &str, reason: crate::piners::report::RefusalReason) -> RefusedProbe {
        RefusedProbe {
            probe: probe.to_owned(),
            reason,
            error: format!("{probe} detail"),
        }
    }

    #[test]
    fn refusals_are_kept_with_their_attempt_whatever_it_ended_as() {
        use crate::piners::report::RefusalReason;
        // Attempt 1 over {p0, p1}: p0 refuses, then the harness crashes on
        // p1. Attempt 3 runs p0 alone, where it evaluates.
        let d = bisect(ids(4), |subset, n| {
            let has = |p: &str| subset.iter().any(|s| s == p);
            if subset.len() == 2 && has("p0") {
                Attempt {
                    outcome: failed(HarnessEnd::Signal(11), "segv", n),
                    refusals: vec![refused("p0", RefusalReason::InvalidEvidence)],
                }
            } else if has("p1") {
                failed(HarnessEnd::Signal(11), "segv", n).into()
            } else {
                AttemptOutcome::Completed.into()
            }
        });
        assert_eq!(
            d.refusals,
            vec![RefusalObservation {
                attempt: 1,
                refusal: refused("p0", RefusalReason::InvalidEvidence),
            }]
        );
        let lines = d.lines();
        assert!(
            lines.iter().any(|l| l.ends_with("p0 (invalid_evidence) in attempt 1: p0 detail")),
            "{lines:?}"
        );
        assert!(
            d.note(std::path::Path::new("r"))
                .contains("; unevaluated: p0 (invalid_evidence) in attempt 1"),
        );
        // An attempt that stops diagnosis - evidence storage failed, or the
        // deadline - keeps its refusals too.
        for stop in [
            StopReason::EvidenceStorage("disk full".to_owned()),
            StopReason::Deadline,
        ] {
            let d = bisect(ids(4), |_, _| Attempt {
                outcome: AttemptOutcome::Stop(stop.clone()),
                refusals: vec![refused("p2", RefusalReason::NoOverlap)],
            });
            assert_eq!(d.stopped, Some(stop.clone()));
            assert_eq!(
                d.refusals,
                vec![RefusalObservation {
                    attempt: 1,
                    refusal: refused("p2", RefusalReason::NoOverlap),
                }]
            );
            assert!(d.note(std::path::Path::new("r")).contains("p2 (no_overlap) in attempt 1"));
        }
    }

    #[test]
    fn a_singleton_refused_alone_is_never_called_completed() {
        use crate::piners::report::RefusalReason;
        let d = bisect(ids(1), |_, _| Attempt {
            outcome: AttemptOutcome::Completed,
            refusals: vec![refused("p0", RefusalReason::NoOverlap)],
        });
        assert_eq!(
            d.not_reproduced[0].alone,
            Some(Alone {
                attempt: 1,
                refusal: Some(refused("p0", RefusalReason::NoOverlap)),
            })
        );
        let lines = d.lines();
        assert!(
            lines.iter().any(|l| l
                == "probe p0 reported unevaluated (no_overlap) when run alone in attempt 1 \
                    (abort did not reproduce)"),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("completed when run alone")), "{lines:?}");
        // A clean singleton keeps its attempt number too.
        let ok = bisect(ids(1), |_, _| AttemptOutcome::Completed);
        assert_eq!(
            ok.not_reproduced[0].alone,
            Some(Alone {
                attempt: 1,
                refusal: None
            })
        );
    }

    #[test]
    fn one_probe_refused_for_two_reasons_keeps_both() {
        use crate::piners::report::RefusalReason;
        // p0 refuses one way beside p1 and another way alone; every attempt
        // holding it also crashes.
        let d = bisect(ids(4), |subset, n| {
            if !subset.iter().any(|s| s == "p0") {
                return AttemptOutcome::Completed.into();
            }
            let reason = if subset.len() == 1 {
                RefusalReason::WitnessContract
            } else {
                RefusalReason::FeedCoverage
            };
            Attempt {
                outcome: failed(HarnessEnd::Code(2), "x", n),
                refusals: vec![refused("p0", reason)],
            }
        });
        let reasons: Vec<RefusalReason> = d.refusals.iter().map(|o| o.refusal.reason).collect();
        assert_eq!(
            reasons,
            vec![RefusalReason::FeedCoverage, RefusalReason::WitnessContract]
        );
    }
}
