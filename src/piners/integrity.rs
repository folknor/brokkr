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

use std::collections::{BTreeSet, HashSet};

use crate::output;
use crate::piners::report::HarnessReport;

/// The parsed report reconciled against the selection.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// How many probes were selected (handed to the harness).
    pub selected: usize,
    /// Selected ids with a valid disposition: the selected identity plus an
    /// outcome and tier that together form a gate label
    /// (`report::valid_label`, the one validator the gate, bless and the
    /// ceiling share). Only these are scored.
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
            if p.valid_disposition().is_some() {
                valid.insert(p.probe.clone());
            } else {
                let tier = p.acceptance.as_ref().map_or("", |a| a.tier.as_str());
                invalid.push(format!(
                    "{} (outcome '{}' with tier '{tier}' is not a valid disposition)",
                    p.probe, p.outcome
                ));
            }
        }
        for rec in &report.invalid {
            match &rec.probe {
                Some(id) => {
                    if selected.contains(id.as_str()) {
                        seen.insert(id.as_str());
                    }
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
        ids.iter().filter(|id| !self.valid.contains(*id)).cloned().collect()
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
            let shown: Vec<&str> =
                self.missing.iter().take(MAX_LISTED).map(String::as_str).collect();
            let more = self.missing.len().saturating_sub(MAX_LISTED);
            let tail = if more > 0 { format!(" (and {more} more)") } else { String::new() };
            lines.push(format!("missing (selected, no record): {}{tail}", shown.join(", ")));
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
}

impl HarnessEnd {
    /// Classify a captured exit status. `killed_on_deadline` wins over the
    /// SIGKILL it produced.
    pub fn from_status(status: &std::process::ExitStatus, killed_on_deadline: bool) -> Self {
        use std::os::unix::process::ExitStatusExt;
        if killed_on_deadline {
            return Self::Backstop;
        }
        match (status.code(), status.signal()) {
            (Some(c), _) => Self::Code(c),
            (None, Some(s)) => Self::Signal(s),
            // Neither: not reachable on Unix; read as the generic failure.
            (None, None) => Self::Code(-1),
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
        }
    }
}

/// The line printed when the harness ended abnormally: how it ended, against
/// how much of the selection it had reported. Distinguishes an abort after
/// complete reporting (a teardown crash, say), where every probe is scored
/// and only the exit is wrong.
pub fn abnormal_end_line(end: HarnessEnd, rec: &Reconciliation) -> String {
    if rec.complete() {
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
    /// `--no-isolate`.
    Disabled,
}

/// Decide whether to isolate. Only an abnormal end - exit 2, any other
/// unexpected code, or a spontaneous signal - with selected probes left
/// unreported qualifies.
pub fn isolation_trigger(end: HarnessEnd, rec: &Reconciliation, no_isolate: bool) -> Trigger {
    match end {
        HarnessEnd::Interrupted => Trigger::Interrupted,
        HarnessEnd::SpawnFailed => Trigger::SpawnFailed,
        HarnessEnd::Backstop => Trigger::Backstop,
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
        assert!(r.invalid[0].starts_with("b (outcome 'parity' with tier ''"), "{:?}", r.invalid);
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
            br#"{"probe":"a","outcome":"no_tv_data"}
{"probe":"zz","outcome":"parity","acceptance":{"tier":"accepted"}}
"#,
        );
        let r = Reconciliation::reconcile(&ids(&["a"]), &report, Vec::new());
        assert!(r.complete());
        assert_eq!(r.valid.len(), 1);
        assert_eq!(r.extras, ids(&["zz"]));
        assert_eq!(r.protocol_violations(), 1);
        assert_eq!(r.summary(), "1 selected, 1 scored, 0 missing, 1 report-only extra");
    }

    #[test]
    fn an_empty_report_reads_as_everything_missing() {
        let r = Reconciliation::reconcile(&ids(&["a", "b"]), &parse(b""), Vec::new());
        assert_eq!(r.summary(), "2 selected, 0 scored, 2 missing");
        assert_eq!(r.protocol_violations(), 0);
        let lines = r.detail_lines();
        assert_eq!(lines, vec!["missing (selected, no record): a, b".to_owned()]);
    }

    #[test]
    fn the_missing_list_is_capped_with_a_count() {
        let sel: Vec<String> = (0..25).map(|i| format!("p{i}")).collect();
        let r = Reconciliation::reconcile(&sel, &parse(b""), Vec::new());
        assert!(r.detail_lines()[0].ends_with("(and 5 more)"), "{:?}", r.detail_lines());
    }

    fn incomplete() -> Reconciliation {
        Reconciliation::reconcile(&ids(&["a", "b"]), &parse(b""), Vec::new())
    }

    fn complete() -> Reconciliation {
        let report = parse(
            br#"{"probe":"a","outcome":"no_tv_data"}
"#,
        );
        Reconciliation::reconcile(&ids(&["a"]), &report, Vec::new())
    }

    #[test]
    fn isolation_triggers_only_on_an_abnormal_end_with_unreported_probes() {
        let inc = incomplete();
        assert_eq!(isolation_trigger(HarnessEnd::Code(2), &inc, false), Trigger::Isolate);
        assert_eq!(isolation_trigger(HarnessEnd::Code(101), &inc, false), Trigger::Isolate);
        assert_eq!(isolation_trigger(HarnessEnd::Signal(11), &inc, false), Trigger::Isolate);
        assert_eq!(isolation_trigger(HarnessEnd::Code(-1), &inc, false), Trigger::Isolate);
        // Completed statuses never isolate, even when probes are missing.
        assert_eq!(isolation_trigger(HarnessEnd::Code(0), &inc, false), Trigger::CompletedStatus);
        assert_eq!(isolation_trigger(HarnessEnd::Code(1), &inc, false), Trigger::CompletedStatus);
        assert_eq!(isolation_trigger(HarnessEnd::Interrupted, &inc, false), Trigger::Interrupted);
        assert_eq!(isolation_trigger(HarnessEnd::SpawnFailed, &inc, false), Trigger::SpawnFailed);
        assert_eq!(isolation_trigger(HarnessEnd::Backstop, &inc, false), Trigger::Backstop);
        assert_eq!(isolation_trigger(HarnessEnd::Code(2), &inc, true), Trigger::Disabled);
        // A teardown crash after complete reporting has nothing to bisect.
        assert_eq!(
            isolation_trigger(HarnessEnd::Signal(6), &complete(), false),
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
        assert!(after.starts_with("harness killed by signal 6 after reporting all 1 selected"), "{after}");
        assert!(after.contains("teardown"));
    }

    #[test]
    fn status_classification_prefers_the_backstop_flag() {
        use std::os::unix::process::ExitStatusExt;
        let killed = std::process::ExitStatus::from_raw(9);
        assert_eq!(HarnessEnd::from_status(&killed, true), HarnessEnd::Backstop);
        assert_eq!(HarnessEnd::from_status(&killed, false), HarnessEnd::Signal(9));
        let two = std::process::ExitStatus::from_raw(2 << 8);
        assert_eq!(HarnessEnd::from_status(&two, false), HarnessEnd::Code(2));
    }
}
