//! Report integrity: did the harness report what it was asked to, and how did
//! it end?
//!
//! Independent of the expected-disposition gate and of `--no-gate`. The gate
//! asks whether each probe sits on its pin; this asks the question before it -
//! whether there is a usable disposition for each selected probe at all, and
//! whether the report carries anything it should not. A run that loses probes
//! fails here even on exit 0 and even under `--no-gate`: a missing probe is
//! not a probe that passed.
//!
//! [`Reconciliation`] reconciles the parsed report against the selected id
//! set exactly, in separate categories (valid, missing, invalid, duplicate,
//! report-only extra). [`HarnessEnd`] classifies how the harness process ended,
//! and [`isolation_trigger`] decides from both whether the diagnostic
//! isolation pass (`crate::piners::isolate`) runs.
//!
//! [`RunAssessment`] is the one shared judgement of a harness invocation:
//! process termination, stream integrity, the contract
//! (`crate::piners::contract`) and the exact selection reconciliation,
//! built by [`assess`]. Ordinary runs, bless, measured iterations and isolation
//! attempts each read it through a verdict method with the matching return
//! type: [`RunAssessment::parity_verdict`],
//! [`RunAssessment::measured_verdict`] or
//! [`RunAssessment::isolation_verdict`].

use std::collections::{BTreeSet, HashSet};

use crate::output;
use crate::piners::contract::{self, ContractAssessment, HarnessExitCode};
use crate::piners::registry::PinnedDisposition;
use crate::piners::report::{self, HarnessReport, ProbeOutcome, RefusalReason};

/// A selected probe one harness invocation validly reported `unevaluated`:
/// an experiment that was not conducted, with its reason and detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedProbe {
    pub probe: String,
    pub reason: RefusalReason,
    pub error: String,
}

impl RefusedProbe {
    /// `id (reason)`.
    pub fn short(&self) -> String {
        format!("{} ({})", self.probe, self.reason.label())
    }
}

/// One harness invocation, judged once.
#[derive(Debug)]
pub struct RunAssessment {
    pub end: HarnessEnd,
    /// The output did not close after the process exited: stdout may be
    /// truncated.
    pub output_cut: bool,
    /// The parsed report, repeats collapsed.
    pub report: HarnessReport,
    pub rec: Reconciliation,
    pub contract: ContractAssessment,
    /// A final unterminated line dropped as a fragment cut off by an
    /// abnormal end (not counted as an invalid record), if any.
    pub truncated_tail: Option<String>,
    /// Every valid `unevaluated` occurrence the stream carried, sorted by id:
    /// gathered before repeats collapse, whether or not the probe was
    /// selected, scored or tainted by another occurrence, and whatever the
    /// ending, so neither scoring nor a classification of how the invocation
    /// ended can hide one. Each fails a parity run, disqualifies a measured
    /// iteration and is kept by diagnostic isolation.
    pub refusals: Vec<RefusedProbe>,
}

/// Parse `stdout` and judge it against `ids` and how the process ended.
pub fn assess(ids: &[String], stdout: &[u8], end: HarnessEnd, output_cut: bool) -> RunAssessment {
    let mut report = report::parse(stdout);
    // A process killed mid-write leaves its last line cut off: on an
    // abnormal end that fragment is evidence of the kill, not a malformed
    // record. Only a genuine fragment - unterminated and not parseable as
    // JSON at all; a complete JSON line with invalid fields that merely
    // lacks its newline stays an invalid record.
    let mut truncated_tail = None;
    if !end.completed_status()
        && report.unterminated
        && let Some(last) = report.invalid.last()
        && last.not_json
        && Some(last.line) == report.last_line
    {
        truncated_tail = report.invalid.pop().map(|r| r.reason);
    }
    let duplicates = report.take_duplicates();
    let rec = Reconciliation::reconcile(ids, &report, duplicates);
    let contract = contract::assess_contract(&report, ids, end);
    // Evidence, not scoring: every valid refusal the stream carried, before
    // repeats collapsed and whether or not another occurrence tainted its
    // probe. An identical repeat is one observation.
    let mut refusals: Vec<RefusedProbe> = Vec::new();
    for (probe, r) in &report.refusals {
        let observed = RefusedProbe {
            probe: probe.clone(),
            reason: r.reason,
            error: r.error.clone(),
        };
        if !refusals.contains(&observed) {
            refusals.push(observed);
        }
    }
    refusals.sort_by(|a, b| a.probe.cmp(&b.probe));
    RunAssessment {
        end,
        output_cut,
        report,
        rec,
        contract,
        truncated_tail,
        refusals,
    }
}

impl RunAssessment {
    /// Reconciliation protocol violations plus contract violations.
    pub fn violations(&self) -> usize {
        self.rec.protocol_violations() + self.contract.violations.len()
    }

    /// Every selected probe validly reported, nothing broke protocol, the
    /// stream is whole, no `run_error`, the process exited 0 or 1, and - once
    /// the contract is observed - the stream carries its `run_end`. The
    /// completion standard isolation attempts and measured iterations hold.
    pub fn completed(&self) -> bool {
        self.end.completed_status()
            && self.rec.complete()
            && self.violations() == 0
            && !self.output_cut
            && self.contract.run_error.is_none()
            && (!self.contract.observed || self.contract.run_end.is_some())
    }

    /// Scored probes the harness reported as `harness_abort`, sorted.
    pub fn harness_aborts(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .report
            .probes
            .iter()
            // `rec.valid` excludes ids tainted by any invalid occurrence; the
            // outcome is the line's own parse-time verdict, so a raw outcome
            // never counts as an abort on its own.
            .filter(|p| {
                p.outcome() == Some(&ProbeOutcome::Evaluated(PinnedDisposition::HarnessAbort))
                    && self.rec.valid.contains(&p.probe)
            })
            .map(|p| p.probe.as_str())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Scored probes reported with any break, refusals included, sorted.
    fn breaks(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .report
            .probes
            .iter()
            .filter(|p| {
                self.rec.valid.contains(&p.probe) && p.outcome().is_some_and(ProbeOutcome::is_break)
            })
            .map(|p| p.probe.as_str())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The refusals as `id (reason)`, sorted by id.
    pub fn refusal_names(&self) -> Vec<String> {
        self.refusals.iter().map(RefusedProbe::short).collect()
    }

    /// The integrity count stored on the run row: every violation plus one
    /// for a cut stream.
    pub fn stored_violations(&self) -> usize {
        self.violations() + usize::from(self.output_cut)
    }

    /// Parity and bless acceptance of the harness side, independent of the
    /// per-probe gate. `exit1_explained` comes from that gate's pinned breaks.
    pub fn parity_verdict(&self, exit1_explained: bool) -> Result<(), Vec<String>> {
        let reasons = self.parity_failure_reasons(exit1_explained);
        if reasons.is_empty() {
            Ok(())
        } else {
            Err(reasons)
        }
    }

    /// Whether an iteration is a valid timing sample and, if not, why.
    pub fn measured_verdict(&self) -> Result<(), String> {
        self.measured_failure()
    }

    /// Classify one diagnostic isolation attempt under its acceptance rule.
    /// Its refusals are not part of the verdict: they ride beside it
    /// ([`Self::refusals`]), whatever the attempt's status.
    pub fn isolation_verdict(&self) -> crate::piners::isolate::AttemptVerdict {
        use crate::piners::isolate::{AttemptVerdict, Failure, StopReason};

        if let Some(error) = &self.contract.run_error {
            AttemptVerdict::Stop(StopReason::HarnessFailure(error.render()))
        } else if self.end == HarnessEnd::Stopped {
            // An intentional stop is never an abort to attribute.
            AttemptVerdict::Stop(StopReason::StopMarker)
        } else if self.completed() {
            AttemptVerdict::Completed
        } else if self.violations() > 0 || self.output_cut {
            let mut what = Vec::new();
            if self.rec.protocol_violations() > 0 {
                what.push(self.rec.summary());
            }
            what.extend(self.contract.violations.iter().cloned());
            if self.output_cut {
                what.push("stdout cut short".to_owned());
            }
            AttemptVerdict::Failed(Failure::ProtocolInvalid(what.join("; ")))
        } else if !self.end.completed_status() {
            AttemptVerdict::Failed(Failure::Aborted)
        } else {
            AttemptVerdict::Failed(Failure::Incomplete(self.rec.summary()))
        }
    }

    /// Produce the detailed failure reasons for parity and bless callers.
    fn parity_failure_reasons(&self, exit1_explained: bool) -> Vec<String> {
        let mut reasons = Vec::new();
        let rec = &self.rec;
        // A run_error fails the run unconditionally and stands first; its
        // readable projection is the fail reason.
        if let Some(e) = &self.contract.run_error {
            reasons.push(e.render());
            if !rec.complete() {
                reasons.push(format!(
                    "{} of {} selected probes unfinished (the run_error ended the run)",
                    rec.unscored(),
                    rec.selected
                ));
            }
        }
        match self.end {
            HarnessEnd::Code(0) => {}
            HarnessEnd::Code(1) if exit1_explained => {}
            HarnessEnd::Code(1) => reasons.push("parity breaks".to_owned()),
            // The run_error already said why the process exited 2.
            HarnessEnd::Code(2) if self.contract.run_error.is_some() => {}
            HarnessEnd::Backstop => reasons.push(format!(
                "harness killed at the {}s hang backstop",
                crate::piners::cmd::HARNESS_HANG_BACKSTOP.as_secs()
            )),
            // A parity run never sets a stop marker; one that ends this way
            // is not a completed run.
            HarnessEnd::Stopped => reasons.push("harness stopped at its --stop marker".to_owned()),
            other => reasons.push(abnormal_reason(other, rec)),
        }
        if self.output_cut {
            reasons.push(
                "harness output did not close after exit (stdout may be truncated)".to_owned(),
            );
        }
        // An abnormal end or a run_error already counted what went unreported.
        if !rec.complete()
            && self.contract.run_error.is_none()
            && (self.end.completed_status()
                || matches!(self.end, HarnessEnd::Backstop | HarnessEnd::Stopped))
        {
            reasons.push(format!(
                "{} of {} selected probes without a valid disposition",
                rec.unscored(),
                rec.selected
            ));
        }
        if !rec.invalid.is_empty() {
            reasons.push(output::count(rec.invalid.len(), "invalid harness record"));
        }
        if !rec.duplicates.is_empty() {
            reasons.push(output::count(
                rec.duplicates.len(),
                "repeated harness record",
            ));
        }
        if !rec.extras.is_empty() {
            reasons.push(output::count(rec.extras.len(), "report-only extra record"));
        }
        if !self.contract.violations.is_empty() {
            reasons.push(output::count(
                self.contract.violations.len(),
                "contract violation",
            ));
        }
        // An experiment that could not be conducted fails the run on its own,
        // gated, --no-gate and bless alike: it never depends on what a pin
        // expects, and no explained exit 1 covers it.
        if !self.refusals.is_empty() {
            // One probe refused for two reasons is one probe, both reasons.
            let probes: BTreeSet<&str> = self.refusals.iter().map(|r| r.probe.as_str()).collect();
            reasons.push(format!(
                "{} ({})",
                output::count(probes.len(), "unevaluated probe"),
                self.refusal_names().join(", ")
            ));
        }
        reasons
    }

    /// Produce the measured-run failure reason, if the assessment is not a
    /// sample. A sample is an exit-0 workload, so observed failure evidence
    /// disqualifies an iteration however it ended. An iteration ended at its
    /// `--stop` marker is excused only what the stop interrupted - full
    /// coverage and a terminal record - never what its stream already shows.
    fn measured_failure(&self) -> Result<(), String> {
        let aborts = self.harness_aborts();
        if let Some(error) = &self.contract.run_error {
            return Err(error.render());
        }
        if !aborts.is_empty() {
            return Err(format!(
                "harness_abort in the run ({}); a crashed probe is no timing sample",
                aborts.join(", ")
            ));
        }
        if !self.refusals.is_empty() {
            return Err(format!(
                "unevaluated in the run ({}); an experiment that was not conducted is no \
                 timing sample",
                self.refusal_names().join(", ")
            ));
        }
        if self.end == HarnessEnd::Stopped {
            return self.stopped_failure();
        }
        if self.end == HarnessEnd::Code(0) && self.completed() {
            return Ok(());
        }
        Err(if self.end == HarnessEnd::Code(0) {
            format!(
                "harness exited 0 without reporting exactly its selection ({})",
                self.rec.summary()
            )
        } else {
            format!("harness {} ({})", self.end.describe(), self.rec.summary())
        })
    }

    /// The measured policy for a stream ended at its `--stop` marker: what
    /// was observed must be clean - no break line, no `run_end` saying 1, no
    /// protocol or contract violation, an intact stream.
    fn stopped_failure(&self) -> Result<(), String> {
        let breaks = self.breaks();
        if !breaks.is_empty() {
            return Err(format!(
                "breaks reported before the --stop marker fired ({}); a failing workload is \
                 no timing sample",
                breaks.join(", ")
            ));
        }
        if self.contract.run_end == Some(HarnessExitCode::Breaks) {
            return Err(
                "run_end exit 1 reported before the --stop marker fired; a failing workload is \
                 no timing sample"
                    .to_owned(),
            );
        }
        if self.violations() > 0 || self.output_cut {
            let mut what = Vec::new();
            if self.rec.protocol_violations() > 0 {
                what.push(self.rec.summary());
            }
            what.extend(self.contract.violations.iter().cloned());
            if self.output_cut {
                what.push("stdout cut short".to_owned());
            }
            return Err(format!(
                "the stream before the --stop marker broke the report protocol ({})",
                what.join("; ")
            ));
        }
        Ok(())
    }

    /// The `[corpus]` lines naming what the assessment found beyond the
    /// summary: reconciliation members, contract violations, and whether a
    /// contract was observed at all.
    pub fn detail_lines(&self) -> Vec<String> {
        let mut lines = self.rec.detail_lines();
        if !self.contract.observed {
            lines.push(
                "no contract observed (no setup, lifecycle or terminal lines): judged on the \
                 exit status and selection coverage alone"
                    .to_owned(),
            );
        }
        for v in &self.contract.violations {
            lines.push(format!("contract violation: {v}"));
        }
        let aborts = self.harness_aborts();
        if !aborts.is_empty() {
            lines.push(format!(
                "harness_abort (the harness saw these probe processes die): {}",
                aborts.join(", ")
            ));
        }
        if !self.refusals.is_empty() {
            lines.push(format!(
                "unevaluated (the experiment could not be conducted; never a pass, no pin can \
                 hold it): {}",
                self.refusal_names().join(", ")
            ));
        }
        lines
    }

    /// Context for an abnormal end or a run_error: what was still in flight
    /// and where setup was. Never a cause - probes overlap.
    pub fn context_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.contract.outstanding.is_empty() {
            lines.push(format!(
                "started, no end observed: {}",
                self.contract.outstanding.join(", ")
            ));
        }
        if let Some(stage) = self.contract.stage_context() {
            lines.push(stage);
        }
        if let Some(tail) = &self.truncated_tail {
            lines.push(format!("final line cut off by the abnormal end: {tail}"));
        }
        lines
    }
}

/// `harness exited 2 before reporting K of N` / `harness killed by signal 11
/// after reporting all N` - the short form stored as the fail reason.
pub fn abnormal_reason(end: HarnessEnd, rec: &Reconciliation) -> String {
    if end == HarnessEnd::Stopped {
        abnormal_end_line(end, rec)
    } else if rec.complete() {
        format!(
            "harness {} after reporting all {}",
            end.describe(),
            rec.selected
        )
    } else {
        format!(
            "harness {} before reporting {} of {}",
            end.describe(),
            rec.unscored(),
            rec.selected
        )
    }
}

/// The parsed report reconciled against the selection.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// How many probes were selected (handed to the harness).
    pub selected: usize,
    /// Selected ids with a valid disposition: the selected identity plus a
    /// record whose parse-time verdict is a `report::ProbeOutcome` (outcome
    /// and tier form a label, and the harness's own `disposition` field, if present,
    /// agrees) - the one validator the gate and bless share.
    /// Only these are scored.
    pub valid: BTreeSet<String>,
    /// Selected ids the report carries no record for at all, in selection
    /// order.
    pub missing: Vec<String>,
    /// Records that are not usable dispositions: a line for a selected id
    /// whose disposition is not a gate label, a disposition line that did not
    /// deserialize, or a stdout line that is not JSON. One description each.
    pub invalid: Vec<String>,
    /// Repeated records (`HarnessReport::take_duplicates`), as named there.
    pub duplicates: Vec<String>,
    /// Ids the report carries a disposition line for that were not selected.
    /// Never scored, whatever they say.
    pub extras: Vec<String>,
}

impl Reconciliation {
    /// Reconcile `report` (already collapsed by `take_duplicates`, whose
    /// return is `duplicates`) against the selected `ids`.
    pub fn reconcile(ids: &[String], report: &HarnessReport, duplicates: Vec<String>) -> Self {
        let selected: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut valid = BTreeSet::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut invalid = Vec::new();
        let mut extras = BTreeSet::new();

        for p in &report.probes {
            if !selected.contains(p.probe.as_str()) {
                extras.insert(p.probe.clone());
                continue;
            }
            seen.insert(p.probe.as_str());
            // A defective record was already put in `report.invalid` by the
            // parser, for every occurrence before repeats collapsed; it is
            // counted there, below, and never scored here.
            if p.outcome().is_some() {
                valid.insert(p.probe.clone());
            }
        }
        for rec in &report.invalid {
            match &rec.probe {
                Some(id) => {
                    if selected.contains(id.as_str()) {
                        seen.insert(id.as_str());
                    } else {
                        // Every invalid record naming an unselected probe is
                        // also an extra, parseable or not, so the counts do
                        // not depend on which way the record was broken.
                        extras.insert(id.clone());
                    }
                    // A defective occurrence unscores the probe even when a
                    // later repeat of it is clean.
                    valid.remove(id);
                    invalid.push(format!("{id} ({})", rec.reason));
                }
                None => invalid.push(rec.reason.clone()),
            }
        }
        let missing = ids
            .iter()
            .filter(|id| !seen.contains(id.as_str()))
            .cloned()
            .collect();
        Self {
            selected: ids.len(),
            valid,
            missing,
            invalid,
            duplicates,
            extras: extras.into_iter().collect(),
        }
    }

    /// Selected ids without a valid disposition (missing, or present only as
    /// an invalid record), in selection order. What the isolation pass
    /// bisects.
    pub fn unreported(&self, ids: &[String]) -> Vec<String> {
        ids.iter()
            .filter(|id| !self.valid.contains(*id))
            .cloned()
            .collect()
    }

    /// Every selected probe has a valid disposition.
    pub fn complete(&self) -> bool {
        self.valid.len() == self.selected
    }

    /// Records the harness contract forbids: invalid records, repeats, and
    /// report-only extras. Any one fails the run.
    pub fn protocol_violations(&self) -> usize {
        self.invalid.len() + self.duplicates.len() + self.extras.len()
    }

    /// Selected minus scored.
    pub fn unscored(&self) -> usize {
        self.selected - self.valid.len()
    }

    /// `N selected, M scored, K missing`, plus the invalid, duplicate and
    /// extra counts when nonzero. The summary line leads with this.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} selected, {} scored, {} missing",
            self.selected,
            self.valid.len(),
            self.missing.len()
        );
        if !self.invalid.is_empty() {
            out.push_str(&format!(", {} invalid", self.invalid.len()));
        }
        if !self.duplicates.is_empty() {
            out.push_str(&format!(", {} duplicate", self.duplicates.len()));
        }
        if !self.extras.is_empty() {
            out.push_str(&format!(", {} report-only extra", self.extras.len()));
        }
        out
    }

    /// One `[corpus]` line per nonempty category naming its members, so a
    /// failed reconciliation names the probes it concerns. The missing list
    /// is capped: a run that died at startup misses every probe, and the
    /// count already says so.
    pub fn detail_lines(&self) -> Vec<String> {
        const MAX_LISTED: usize = 20;
        let mut lines = Vec::new();
        if !self.missing.is_empty() {
            let shown: Vec<&str> = self
                .missing
                .iter()
                .take(MAX_LISTED)
                .map(String::as_str)
                .collect();
            let more = self.missing.len().saturating_sub(MAX_LISTED);
            let tail = if more > 0 {
                format!(" (and {more} more)")
            } else {
                String::new()
            };
            lines.push(format!(
                "missing (selected, no record): {}{tail}",
                shown.join(", ")
            ));
        }
        for i in &self.invalid {
            lines.push(format!("invalid record: {i}"));
        }
        if !self.duplicates.is_empty() {
            lines.push(format!(
                "repeated records (kept the last of each): {}",
                self.duplicates.join(", ")
            ));
        }
        if !self.extras.is_empty() {
            lines.push(format!(
                "report-only extras (not selected, never scored): {}",
                self.extras.join(", ")
            ));
        }
        lines
    }
}

/// How the harness process ended, as far as the integrity check and the
/// isolation trigger care. An interrupt and a spawn failure never produce a
/// capture to classify (the runner returns an error instead), but they are
/// modelled so the trigger's refusal of them is explicit and tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessEnd {
    /// Exited on its own with this code.
    Code(i32),
    /// Died of this signal, not one brokkr sent.
    Signal(i32),
    /// Killed by brokkr at the hang backstop.
    Backstop,
    /// Stopped by an interrupt or a requested shutdown.
    Interrupted,
    /// Never started.
    SpawnFailed,
    /// Ended by brokkr when the measured iteration's `--stop` marker fired:
    /// on purpose, so never an abort, and with no natural exit code. Only a
    /// measured run sets a stop marker; it is not a completed status.
    Stopped,
}

impl HarnessEnd {
    /// Classify how the supervisor saw the harness end. A deadline kill is the
    /// backstop, not the SIGKILL it produced.
    pub fn from_ending(ending: &crate::output::Ending) -> Self {
        use crate::output::Ending;
        match ending {
            Ending::Exited(c) => Self::Code(*c),
            Ending::Signalled(s) => Self::Signal(*s),
            Ending::Deadline(_) => Self::Backstop,
            Ending::Interrupted => Self::Interrupted,
            Ending::StopMarker(_) => Self::Stopped,
            // The child could not be waited on: the generic failure.
            Ending::WaitFailed(_) => Self::Code(-1),
        }
    }

    /// 0 (clean) or 1 (finished with breaks): the harness's two completed
    /// statuses.
    pub fn completed_status(self) -> bool {
        matches!(self, Self::Code(0 | 1))
    }

    /// `exited 2` / `killed by signal 11` / ... - the status as a phrase.
    pub fn describe(self) -> String {
        match self {
            Self::Code(c) => format!("exited {c}"),
            Self::Signal(s) => format!("killed by signal {s}"),
            Self::Backstop => "was killed at the hang backstop".to_owned(),
            Self::Interrupted => "was interrupted".to_owned(),
            Self::SpawnFailed => "failed to spawn".to_owned(),
            Self::Stopped => "was stopped at its --stop marker".to_owned(),
        }
    }

    /// `exit 2` / `signal 11` - the short form used in isolation verdicts.
    pub fn short(self) -> String {
        match self {
            Self::Code(c) => format!("exit {c}"),
            Self::Signal(s) => format!("signal {s}"),
            Self::Backstop => "hang backstop".to_owned(),
            Self::Interrupted => "interrupted".to_owned(),
            Self::SpawnFailed => "spawn failure".to_owned(),
            Self::Stopped => "stop marker".to_owned(),
        }
    }
}

/// The line printed when the harness ended abnormally: how it ended, against
/// how much of the selection it had reported. Distinguishes an abort after
/// complete reporting (a teardown crash, say), where every probe is scored
/// and only the exit is wrong. An intentional stop is never called a crash.
pub fn abnormal_end_line(end: HarnessEnd, rec: &Reconciliation) -> String {
    if end == HarnessEnd::Stopped {
        format!(
            "harness {} with {} of {} selected probes reported",
            end.describe(),
            rec.valid.len(),
            rec.selected
        )
    } else if rec.complete() {
        format!(
            "harness {} after reporting all {} selected probes (an abort after complete \
             reporting, e.g. a teardown crash; nothing to isolate)",
            end.describe(),
            rec.selected
        )
    } else {
        format!(
            "harness {} before reporting {} of {}",
            end.describe(),
            rec.unscored(),
            rec.selected
        )
    }
}

/// Whether the diagnostic isolation pass runs, and if not, why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Isolate,
    /// Exit 0 or 1: the harness finished. Missing probes still fail the run;
    /// they are a harness contract breach, not a crash to bisect.
    CompletedStatus,
    /// Interrupt or requested shutdown: the user asked brokkr to stop.
    Interrupted,
    /// Nothing ran.
    SpawnFailed,
    /// The hang backstop already spent the time budget, and a hang is not
    /// attributable by bisection within it.
    Backstop,
    /// Every selected probe was validly reported: the abort came after the
    /// work (a teardown crash), so there is nothing to bisect.
    ReportComplete,
    /// The harness reported a `run_error`: a shared-setup or harness failure
    /// it already named. Nothing to bisect.
    RunError,
    /// brokkr ended the harness at a `--stop` marker: an intentional stop,
    /// never a crash to bisect.
    Stopped,
    /// `--no-isolate`.
    Disabled,
}

/// Decide whether to isolate. Only an abnormal end - exit 2, any other
/// unexpected code, or a spontaneous signal - with no `run_error` and
/// selected probes left unreported qualifies.
pub fn isolation_trigger(
    end: HarnessEnd,
    rec: &Reconciliation,
    run_error: bool,
    no_isolate: bool,
) -> Trigger {
    match end {
        HarnessEnd::Interrupted => Trigger::Interrupted,
        HarnessEnd::SpawnFailed => Trigger::SpawnFailed,
        HarnessEnd::Backstop => Trigger::Backstop,
        HarnessEnd::Stopped => Trigger::Stopped,
        _ if run_error => Trigger::RunError,
        e if e.completed_status() => Trigger::CompletedStatus,
        _ if rec.complete() => Trigger::ReportComplete,
        _ if no_isolate => Trigger::Disabled,
        _ => Trigger::Isolate,
    }
}

/// Print the harness stderr whole, under a header, through the corpus
/// printer. It is the only evidence of an abort, so it is never truncated.
pub fn print_stderr(label: &str, stderr: &str) {
    if stderr.trim().is_empty() {
        output::corpus_msg(&format!("{label}: (empty)"));
        return;
    }
    output::corpus_msg(&format!("{label}:"));
    for line in stderr.lines() {
        output::corpus_msg(&format!("  {line}"));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::report::parse;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn parity_reasons(a: &RunAssessment, exit1_explained: bool) -> Vec<String> {
        match a.parity_verdict(exit1_explained) {
            Ok(()) => Vec::new(),
            Err(reasons) => reasons,
        }
    }

    #[test]
    fn reconciliation_separates_every_category() {
        let mut report = parse(
            br#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}
{"probe":"b","outcome":"parity"}
{"probe":"x","outcome":"parity","acceptance":{"tier":"accepted"}}
{"probe":"c","outcome":"compile_fail","error":"e"}
{"probe":"c","outcome":"compile_fail","error":"e"}
{"probe":"d","outcome":false}
not json at all
"#,
        );
        let dups = report.take_duplicates();
        let sel = ids(&["a", "b", "c", "d", "e"]);
        let r = Reconciliation::reconcile(&sel, &report, dups);
        assert_eq!(r.selected, 5);
        let want: BTreeSet<String> = ids(&["a", "c"]).into_iter().collect();
        assert_eq!(r.valid, want);
        // `e` has no record at all; `b` and `d` have invalid ones.
        assert_eq!(r.missing, ids(&["e"]));
        assert_eq!(r.invalid.len(), 3, "{:?}", r.invalid);
        assert!(
            r.invalid[0].starts_with("b (outcome 'parity' with tier ''"),
            "{:?}",
            r.invalid
        );
        assert!(r.invalid[1].starts_with("d (unparsable"));
        assert!(r.invalid[2].starts_with("not JSON"));
        assert_eq!(r.duplicates.len(), 1);
        assert_eq!(r.extras, ids(&["x"]));
        assert_eq!(r.unreported(&sel), ids(&["b", "d", "e"]));
        assert!(!r.complete());
        assert_eq!(r.protocol_violations(), 5);
        assert_eq!(
            r.summary(),
            "5 selected, 2 scored, 1 missing, 3 invalid, 1 duplicate, 1 report-only extra"
        );
    }

    #[test]
    fn a_label_posing_as_the_wrong_field_is_not_scored() {
        let report = parse(
            br#"{"probe":"a","outcome":"accepted"}
{"probe":"b","outcome":"parity","acceptance":{"tier":"runtime_fail"}}
{"probe":"c","outcome":"compile_fail","acceptance":{"tier":"accepted"}}
"#,
        );
        let r = Reconciliation::reconcile(&ids(&["a", "b", "c"]), &report, Vec::new());
        assert!(r.valid.is_empty());
        assert_eq!(r.invalid.len(), 3, "{:?}", r.invalid);
        assert_eq!(r.unreported(&ids(&["a", "b", "c"])).len(), 3);
    }

    #[test]
    fn an_extra_is_never_scored_and_a_clean_report_is_complete() {
        let report = parse(
            br#"{"probe":"a","outcome":"compile_fail","error":"e"}
{"probe":"zz","outcome":"parity","acceptance":{"tier":"accepted"}}
"#,
        );
        let r = Reconciliation::reconcile(&ids(&["a"]), &report, Vec::new());
        assert!(r.complete());
        assert_eq!(r.valid.len(), 1);
        assert_eq!(r.extras, ids(&["zz"]));
        assert_eq!(r.protocol_violations(), 1);
        assert_eq!(
            r.summary(),
            "1 selected, 1 scored, 0 missing, 1 report-only extra"
        );
    }

    #[test]
    fn an_empty_report_reads_as_everything_missing() {
        let r = Reconciliation::reconcile(&ids(&["a", "b"]), &parse(b""), Vec::new());
        assert_eq!(r.summary(), "2 selected, 0 scored, 2 missing");
        assert_eq!(r.protocol_violations(), 0);
        let lines = r.detail_lines();
        assert_eq!(
            lines,
            vec!["missing (selected, no record): a, b".to_owned()]
        );
    }

    #[test]
    fn the_missing_list_is_capped_with_a_count() {
        let sel: Vec<String> = (0..25).map(|i| format!("p{i}")).collect();
        let r = Reconciliation::reconcile(&sel, &parse(b""), Vec::new());
        assert!(
            r.detail_lines()[0].ends_with("(and 5 more)"),
            "{:?}",
            r.detail_lines()
        );
    }

    fn incomplete() -> Reconciliation {
        Reconciliation::reconcile(&ids(&["a", "b"]), &parse(b""), Vec::new())
    }

    fn complete() -> Reconciliation {
        let report = parse(
            br#"{"probe":"a","outcome":"compile_fail","error":"e"}
"#,
        );
        Reconciliation::reconcile(&ids(&["a"]), &report, Vec::new())
    }

    #[test]
    fn isolation_triggers_only_on_an_abnormal_end_with_unreported_probes() {
        let inc = incomplete();
        assert_eq!(
            isolation_trigger(HarnessEnd::Code(2), &inc, false, false),
            Trigger::Isolate
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::Code(101), &inc, false, false),
            Trigger::Isolate
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::Signal(11), &inc, false, false),
            Trigger::Isolate
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::Code(-1), &inc, false, false),
            Trigger::Isolate
        );
        // Completed statuses never isolate, even when probes are missing.
        assert_eq!(
            isolation_trigger(HarnessEnd::Code(0), &inc, false, false),
            Trigger::CompletedStatus
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::Code(1), &inc, false, false),
            Trigger::CompletedStatus
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::Interrupted, &inc, false, false),
            Trigger::Interrupted
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::SpawnFailed, &inc, false, false),
            Trigger::SpawnFailed
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::Backstop, &inc, false, false),
            Trigger::Backstop
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::Code(2), &inc, false, true),
            Trigger::Disabled
        );
        // A run_error is the harness naming its own failure: never bisected.
        assert_eq!(
            isolation_trigger(HarnessEnd::Code(2), &inc, true, false),
            Trigger::RunError
        );
        assert_eq!(
            isolation_trigger(HarnessEnd::Interrupted, &inc, true, false),
            Trigger::Interrupted
        );
        // A teardown crash after complete reporting has nothing to bisect.
        assert_eq!(
            isolation_trigger(HarnessEnd::Signal(6), &complete(), false, false),
            Trigger::ReportComplete
        );
    }

    #[test]
    fn the_abnormal_end_line_counts_against_the_selection() {
        assert_eq!(
            abnormal_end_line(HarnessEnd::Code(2), &incomplete()),
            "harness exited 2 before reporting 2 of 2"
        );
        assert_eq!(
            abnormal_end_line(HarnessEnd::Signal(11), &incomplete()),
            "harness killed by signal 11 before reporting 2 of 2"
        );
        let after = abnormal_end_line(HarnessEnd::Signal(6), &complete());
        assert!(
            after.starts_with("harness killed by signal 6 after reporting all 1 selected"),
            "{after}"
        );
        assert!(after.contains("teardown"));
    }

    #[test]
    fn ending_classification_names_the_backstop_not_its_sigkill() {
        use crate::output::Ending;
        assert_eq!(
            HarnessEnd::from_ending(&Ending::Deadline(std::time::Duration::from_secs(1))),
            HarnessEnd::Backstop
        );
        assert_eq!(
            HarnessEnd::from_ending(&Ending::Signalled(9)),
            HarnessEnd::Signal(9)
        );
        assert_eq!(
            HarnessEnd::from_ending(&Ending::Exited(2)),
            HarnessEnd::Code(2)
        );
        assert_eq!(
            HarnessEnd::from_ending(&Ending::Interrupted),
            HarnessEnd::Interrupted
        );
    }

    const V1: &str = "\"version\":1";

    fn clean_stream(probes: &[&str], outcome: &str, code: i32) -> String {
        let mut s =
            format!("{{\"kind\":\"run_start\",{V1}}}\n{{\"kind\":\"setup_complete\",{V1}}}\n");
        for p in probes {
            s.push_str(&format!(
                "{{\"kind\":\"probe_start\",{V1},\"probe\":\"{p}\"}}\n"
            ));
            s.push_str(&format!(
                "{{\"probe\":\"{p}\",\"outcome\":\"{outcome}\"}}\n"
            ));
            s.push_str(&format!(
                "{{\"kind\":\"probe_end\",{V1},\"probe\":\"{p}\"}}\n"
            ));
        }
        s.push_str(&format!("{{\"kind\":\"run_end\",{V1},\"exit\":{code}}}\n"));
        s
    }

    #[test]
    fn a_clean_contract_stream_completes_and_a_harness_abort_counts() {
        let a = assess(
            &ids(&["a", "b"]),
            clean_stream(&["a", "b"], "harness_abort", 1).as_bytes(),
            HarnessEnd::Code(1),
            false,
        );
        assert!(a.completed(), "{:?}", a.contract.violations);
        assert_eq!(a.harness_aborts(), vec!["a", "b"]);
        // Exit 1 explained by pinned breaks is acceptable.
        assert!(parity_reasons(&a, true).is_empty());
        // The same stream without its run_end does not complete.
        let cut = clean_stream(&["a"], "compile_fail", 0)
            .replace(&format!("{{\"kind\":\"run_end\",{V1},\"exit\":0}}\n"), "");
        let a = assess(&ids(&["a"]), cut.as_bytes(), HarnessEnd::Code(0), false);
        assert!(!a.completed());
        assert!(
            parity_reasons(&a, false)
                .iter()
                .any(|r| r == "1 contract violation")
        );
    }

    #[test]
    fn an_unevaluated_probe_completes_the_stream_but_never_passes() {
        let stream = clean_stream(&["a", "b"], "harness_abort", 1).replace(
            r#"{"probe":"a","outcome":"harness_abort"}"#,
            r#"{"probe":"a","outcome":"unevaluated","reason":"witness_contract","error":"w"}"#,
        );
        let a = assess(
            &ids(&["a", "b"]),
            stream.as_bytes(),
            HarnessEnd::Code(1),
            false,
        );
        assert!(a.completed(), "{:?}", a.contract.violations);
        assert_eq!(a.refusal_names(), vec!["a (witness_contract)".to_owned()]);
        assert_eq!(a.refusals[0].error, "w");
        assert_eq!(a.harness_aborts(), vec!["b"]);
        // Even with exit 1 explained, the run fails on the refusal alone.
        assert_eq!(
            parity_reasons(&a, true),
            vec!["1 unevaluated probe (a (witness_contract))".to_owned()]
        );
        assert!(
            a.detail_lines()
                .iter()
                .any(|l| l.starts_with("unevaluated (") && l.ends_with("a (witness_contract)"))
        );
        // Never a timing sample, on exit 0 as on exit 1.
        let solo = clean_stream(&["a"], "unevaluated", 0).replace(
            r#""outcome":"unevaluated""#,
            r#""outcome":"unevaluated","reason":"no_overlap","error":"disjoint""#,
        );
        let a = assess(&ids(&["a"]), solo.as_bytes(), HarnessEnd::Code(0), false);
        assert!(a.completed());
        assert!(
            a.measured_verdict()
                .unwrap_err()
                .starts_with("unevaluated in the run (a (no_overlap))")
        );
    }

    /// A prefix of a contract stream, cut where a `--stop` marker fired:
    /// start, setup, then each of `probes` started with `line` as its
    /// disposition, no end and no terminal record.
    fn stopped_prefix(probes: &[(&str, &str)]) -> String {
        let mut s =
            format!("{{\"kind\":\"run_start\",{V1}}}\n{{\"kind\":\"setup_complete\",{V1}}}\n");
        for (p, line) in probes {
            s.push_str(&format!(
                "{{\"kind\":\"probe_start\",{V1},\"probe\":\"{p}\"}}\n{line}\n"
            ));
        }
        s
    }

    #[test]
    fn a_stop_excuses_only_what_it_interrupted() {
        let ok = r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#;
        let sel = ids(&["a", "b"]);
        // Clean prefix: coverage and the terminal record are excused.
        let clean = stopped_prefix(&[("a", ok)]);
        let a = assess(&sel, clean.as_bytes(), HarnessEnd::Stopped, false);
        assert!(
            a.contract.violations.is_empty(),
            "{:?}",
            a.contract.violations
        );
        assert!(a.measured_verdict().is_ok(), "{:?}", a.measured_verdict());
        // A fragment the stop cut off is context, not an invalid record.
        let cut = format!("{clean}{{\"probe\":\"b\",\"outc");
        let a = assess(&sel, cut.as_bytes(), HarnessEnd::Stopped, false);
        assert!(a.measured_verdict().is_ok(), "{:?}", a.measured_verdict());
        assert!(a.truncated_tail.is_some());
        // Observed failure evidence is never excused.
        let broke = stopped_prefix(&[
            ("a", ok),
            ("b", r#"{"probe":"b","outcome":"compile_fail"}"#),
        ]);
        let err = assess(&sel, broke.as_bytes(), HarnessEnd::Stopped, false)
            .measured_verdict()
            .unwrap_err();
        assert!(
            err.starts_with("breaks reported before the --stop marker fired (b)"),
            "{err}"
        );
        let refused = stopped_prefix(&[(
            "a",
            r#"{"probe":"a","outcome":"unevaluated","reason":"no_overlap","error":"d"}"#,
        )]);
        let err = assess(&sel, refused.as_bytes(), HarnessEnd::Stopped, false)
            .measured_verdict()
            .unwrap_err();
        assert!(
            err.starts_with("unevaluated in the run (a (no_overlap))"),
            "{err}"
        );
        let ended_one = format!(
            "{}{{\"kind\":\"probe_end\",{V1},\"probe\":\"a\"}}\n{{\"kind\":\"run_end\",{V1},\"exit\":1}}\n",
            stopped_prefix(&[("a", ok)])
        );
        let err = assess(
            &ids(&["a"]),
            ended_one.as_bytes(),
            HarnessEnd::Stopped,
            false,
        )
        .measured_verdict()
        .unwrap_err();
        assert!(err.starts_with("run_end exit 1 reported"), "{err}");
        // A defect the prefix already establishes fails too: a duplicate start.
        let dup = format!("{clean}{{\"kind\":\"probe_start\",{V1},\"probe\":\"a\"}}\n");
        let err = assess(&sel, dup.as_bytes(), HarnessEnd::Stopped, false)
            .measured_verdict()
            .unwrap_err();
        assert!(err.contains("duplicate probe_start"), "{err}");
        // So does a malformed run_error, which a blanket waiver would pass.
        let bad_error = format!("{clean}{{\"kind\":\"run_error\",{V1}}}\n");
        assert!(
            assess(&sel, bad_error.as_bytes(), HarnessEnd::Stopped, false)
                .measured_verdict()
                .is_err()
        );
        // A whole clean stream with run_end 0 the stop then cut off is
        // accepted: with no process exit, run_end is compared with nothing.
        let whole = clean_stream(&["a"], "compile_fail", 0).replace(
            r#""outcome":"compile_fail""#,
            r#""outcome":"parity","acceptance":{"tier":"accepted"}"#,
        );
        let a = assess(&ids(&["a"]), whole.as_bytes(), HarnessEnd::Stopped, false);
        assert!(
            a.contract.violations.is_empty(),
            "{:?}",
            a.contract.violations
        );
        assert!(a.measured_verdict().is_ok(), "{:?}", a.measured_verdict());
    }

    #[test]
    fn a_refusal_is_kept_as_evidence_whatever_else_its_probe_reported() {
        let refusal =
            r#"{"probe":"a","outcome":"unevaluated","reason":"invalid_evidence","error":"e"}"#;
        let ok = r#"{"probe":"a","outcome":"parity","acceptance":{"tier":"accepted"}}"#;
        let bad = r#"{"probe":"a","outcome":"parity"}"#;
        // A refusal then an evaluated repeat (collapse keeps the last), and a
        // refusal beside an invalid occurrence (which unscores the probe).
        for nd in [format!("{refusal}\n{ok}\n"), format!("{refusal}\n{bad}\n")] {
            let a = assess(&ids(&["a"]), nd.as_bytes(), HarnessEnd::Code(1), false);
            assert_eq!(
                a.refusal_names(),
                vec!["a (invalid_evidence)".to_owned()],
                "{nd}"
            );
            assert!(!a.completed(), "{nd}");
        }
        // An identical repeat is one observation.
        let twice = format!("{refusal}\n{refusal}\n");
        let a = assess(&ids(&["a"]), twice.as_bytes(), HarnessEnd::Code(1), false);
        assert_eq!(a.refusals.len(), 1);
        // Two reasons for one probe: both kept, counted as one probe.
        let other = refusal.replace("invalid_evidence", "feed_coverage");
        let two = format!("{refusal}\n{other}\n");
        let a = assess(&ids(&["a"]), two.as_bytes(), HarnessEnd::Code(1), false);
        assert_eq!(a.refusals.len(), 2);
        assert!(
            parity_reasons(&a, true)
                .iter()
                .any(|r| r
                    .starts_with("1 unevaluated probe (a (invalid_evidence), a (feed_coverage))")),
            "{:?}",
            parity_reasons(&a, true)
        );
    }

    #[test]
    fn a_stop_is_never_isolated_or_called_a_crash() {
        let inc = incomplete();
        assert_eq!(
            isolation_trigger(HarnessEnd::Stopped, &inc, false, false),
            Trigger::Stopped
        );
        let a = assess(&ids(&["a", "b"]), b"", HarnessEnd::Stopped, false);
        assert_eq!(
            a.isolation_verdict(),
            crate::piners::isolate::AttemptVerdict::Stop(
                crate::piners::isolate::StopReason::StopMarker
            )
        );
        let line = abnormal_end_line(HarnessEnd::Stopped, &complete());
        assert!(!line.contains("teardown"), "{line}");
        assert!(
            parity_reasons(&a, true)
                .iter()
                .any(|r| r == "harness stopped at its --stop marker")
        );
    }

    #[test]
    fn malformed_harness_abort_is_not_scored_as_an_abort() {
        let stream = clean_stream(&["a"], "harness_abort", 1).replace(
            r#""outcome":"harness_abort""#,
            r#""outcome":"harness_abort","acceptance":{"tier":"accepted"}"#,
        );
        let a = assess(&ids(&["a"]), stream.as_bytes(), HarnessEnd::Code(1), false);
        assert!(a.rec.valid.is_empty());
        assert!(a.harness_aborts().is_empty());
    }

    #[test]
    fn a_run_error_fails_first_and_names_the_unfinished_work() {
        let nd = format!(
            "{{\"kind\":\"run_start\",{V1}}}\n\
             {{\"kind\":\"setup_stage\",{V1},\"stage\":\"feed_load\",\"feed\":\"eth\"}}\n\
             {{\"kind\":\"run_error\",{V1},\"stage\":\"feed_load\",\"feed\":\"eth\",\"error\":\"oom\"}}\n"
        );
        let a = assess(&ids(&["a", "b"]), nd.as_bytes(), HarnessEnd::Code(2), false);
        let r = parity_reasons(&a, false);
        assert_eq!(r[0], "run_error at stage feed_load (feed eth): oom");
        assert_eq!(
            r[1],
            "2 of 2 selected probes unfinished (the run_error ended the run)"
        );
        // Unfinished work is not a protocol violation, nor a second abnormal-exit reason.
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(a.violations(), 0);
        assert!(!a.completed());
    }

    #[test]
    fn a_fragment_cut_by_a_signal_is_context_not_a_violation() {
        let nd = format!(
            "{{\"kind\":\"run_start\",{V1}}}\n{{\"kind\":\"setup_complete\",{V1}}}\n{{\"kind\":\"probe_start\",{V1},\"probe\":\"a\"}}\n{{\"probe\":\"a\",\"outc"
        );
        let a = assess(&ids(&["a"]), nd.as_bytes(), HarnessEnd::Signal(9), false);
        assert_eq!(
            a.violations(),
            0,
            "{:?} {:?}",
            a.rec.invalid,
            a.contract.violations
        );
        let ctx = a.context_lines();
        assert!(
            ctx.iter().any(|l| l == "started, no end observed: a"),
            "{ctx:?}"
        );
        assert!(
            ctx.iter().any(|l| l.starts_with("final line cut off")),
            "{ctx:?}"
        );
        // On a clean exit the same fragment is an invalid record.
        let a = assess(&ids(&["a"]), nd.as_bytes(), HarnessEnd::Code(0), false);
        assert_eq!(a.rec.invalid.len(), 1);
        // Complete JSON with invalid fields, unterminated, on an abnormal
        // end, is not a fragment: it stays an invalid record.
        let a = assess(
            &ids(&["a"]),
            b"{\"probe\":\"a\"}",
            HarnessEnd::Code(2),
            false,
        );
        assert_eq!(a.rec.invalid.len(), 1, "{:?}", a.truncated_tail);
        assert!(a.truncated_tail.is_none());
    }

    #[test]
    fn the_harness_disposition_field_must_agree_when_present() {
        let sel = ids(&["a"]);
        let base = "\"probe\":\"a\",\"outcome\":\"parity\",\"acceptance\":{\"tier\":\"accepted\"}";
        // Missing: tolerated. Agreeing: scored.
        for extra in ["", ",\"disposition\":\"accepted\""] {
            let nd = format!("{{{base}{extra}}}\n");
            let a = assess(&sel, nd.as_bytes(), HarnessEnd::Code(0), false);
            assert!(a.completed(), "{extra}: {:?}", a.rec.invalid);
        }
        // Contradicting, null, number, object, empty string: invalid, unscored.
        for (extra, shown) in [
            (",\"disposition\":\"byte_exact\"", "\"byte_exact\""),
            (",\"disposition\":null", "null"),
            (",\"disposition\":3", "3"),
            (",\"disposition\":{}", "{}"),
            (",\"disposition\":\"\"", "\"\""),
        ] {
            let nd = format!("{{{base}{extra}}}\n");
            let a = assess(&sel, nd.as_bytes(), HarnessEnd::Code(0), false);
            assert!(a.rec.valid.is_empty(), "{extra}");
            assert_eq!(a.rec.invalid.len(), 1, "{extra}");
            assert!(
                a.rec.invalid[0].contains(&format!(
                    "its disposition field {shown} contradicts the derived label \"accepted\""
                )),
                "{extra}: {:?}",
                a.rec.invalid
            );
            assert!(a.report.probes[0].outcome().is_none());
        }
        // An invalid outcome/tier stays invalid even when the field agrees
        // with the derived fallback.
        let nd = "{\"probe\":\"a\",\"outcome\":\"parity\",\"disposition\":\"parity\"}\n";
        assert!(
            assess(&sel, nd.as_bytes(), HarnessEnd::Code(0), false)
                .rec
                .valid
                .is_empty()
        );
    }

    #[test]
    fn an_earlier_contradiction_is_not_hidden_by_a_later_clean_repeat() {
        let sel = ids(&["a"]);
        let nd = "{\"probe\":\"a\",\"outcome\":\"compile_fail\",\"disposition\":\"accepted\"}\n\
                  {\"probe\":\"a\",\"outcome\":\"compile_fail\",\"disposition\":\"compile_fail\"}\n";
        let a = assess(&sel, nd.as_bytes(), HarnessEnd::Code(0), false);
        assert_eq!(a.rec.invalid.len(), 1, "{:?}", a.rec.invalid);
        assert_eq!(a.rec.duplicates.len(), 1);
        assert!(a.rec.valid.is_empty(), "the probe must not be scored");
        assert!(!a.completed());
    }

    #[test]
    fn an_invalid_record_for_an_unselected_probe_is_an_extra_either_way() {
        let sel = ids(&["a"]);
        let a_ok = "{\"probe\":\"a\",\"outcome\":\"compile_fail\"}\n";
        // Unparsable, and parseable but contradictory: both invalid plus extra.
        for bad in [
            "{\"probe\":\"zz\"}\n",
            "{\"probe\":\"zz\",\"outcome\":\"compile_fail\",\"disposition\":\"x\"}\n",
        ] {
            let nd = format!("{a_ok}{bad}");
            let a = assess(&sel, nd.as_bytes(), HarnessEnd::Code(0), false);
            assert_eq!(a.rec.invalid.len(), 1, "{bad}");
            assert_eq!(a.rec.extras, ids(&["zz"]), "{bad}");
        }
    }

    #[test]
    fn no_contract_is_said_and_keeps_the_old_rules() {
        let a = assess(
            &ids(&["a"]),
            b"{\"probe\":\"a\",\"outcome\":\"compile_fail\"}\n",
            HarnessEnd::Code(0),
            false,
        );
        assert!(a.completed());
        assert!(
            a.detail_lines()
                .iter()
                .any(|l| l.starts_with("no contract observed"))
        );
        assert!(!a.detail_lines().iter().any(|l| l.contains("old harness")));
    }
}
