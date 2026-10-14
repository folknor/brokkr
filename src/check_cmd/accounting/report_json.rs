// Schema-2 JSON blocks derived from reconciliation and policy coverage.

use super::journal::{Termination, TerminationCause, TerminationScope};
use super::reconcile::{DoctestObserved, Outcome, Reconciliation};

// ----- the `--json` summary, schema 2 -----

/// How a run stopped, for the trailer. Serialized as two strings (`kind` the
/// cause's snake_case name, `scope` `run`, a lane label or a phase name), so
/// the JSON shape is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct TerminationSummary {
    /// Serialized by its trailer name (`TerminationCause::as_str`), which is
    /// not always the journal's serde spelling.
    #[serde(serialize_with = "serialize_cause")]
    pub(crate) kind: TerminationCause,
    pub(crate) scope: SummaryScope,
}

fn serialize_cause<S: serde::Serializer>(cause: &TerminationCause, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(cause.as_str())
}

/// What a stop covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SummaryScope {
    Run,
    /// The lane, by its label.
    Lane(String),
    /// The check phase a ceiling fired in.
    Phase(crate::check_cmd::Phase),
}

impl SummaryScope {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Run => "run",
            Self::Lane(label) => label,
            Self::Phase(phase) => phase.as_str(),
        }
    }
}

impl serde::Serialize for SummaryScope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl TerminationSummary {
    /// A recorded termination, with its scope named for a reader: the lane's
    /// label for a lane or stream, `run` otherwise.
    pub(crate) fn of(t: &Termination, label: impl Fn(usize) -> Option<String>) -> Self {
        let scope = match (t.scope, t.lane) {
            (TerminationScope::Run, _) | (_, None) => SummaryScope::Run,
            (_, Some(l)) => SummaryScope::Lane(label(l).unwrap_or_else(|| format!("lane {l}"))),
        };
        Self { kind: t.cause, scope }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PolicyCoverage {
    pub(crate) status: &'static str,
    pub(crate) plan_complete: bool,
    pub(crate) pairs: usize,
    pub(crate) selected: usize,
    pub(crate) ignored: usize,
    pub(crate) quarantined: usize,
    pub(crate) curated: usize,
    pub(crate) orphaned: usize,
    pub(crate) dead_filters: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ExecutionAccounting {
    /// Accounting covers enumerable binary tests only; doctests are their own
    /// block.
    pub(crate) scope: &'static str,
    pub(crate) status: &'static str,
    pub(crate) expected_executions: usize,
    pub(crate) passed: usize,
    pub(crate) failed: usize,
    pub(crate) timed_out: usize,
    pub(crate) interrupted: usize,
    pub(crate) ignored: usize,
    pub(crate) unobserved: usize,
    pub(crate) anomalies: usize,
}

impl ExecutionAccounting {
    pub(crate) fn of(r: &Reconciliation) -> Self {
        Self {
            scope: "binary_tests",
            status: r.status().as_str(),
            expected_executions: r.accounted.len(),
            passed: r.count(Outcome::Passed),
            failed: r.count(Outcome::Failed),
            timed_out: r.count(Outcome::TimedOut),
            interrupted: r.count(Outcome::Interrupted),
            ignored: r.count(Outcome::Ignored),
            unobserved: r.count(Outcome::Unobserved),
            anomalies: r.anomalies.len(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DoctestAccounting {
    /// Always `unavailable`: doctests cannot be enumerated, so there is no
    /// inventory - and an unknown inventory is never reported as zero.
    pub(crate) inventory: &'static str,
    /// Always `unknown`, for the same reason.
    pub(crate) accounting: &'static str,
    pub(crate) observed: DoctestObserved,
}

/// The schema-2 blocks a complete-profile run reports.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct AccountingBlocks {
    pub(crate) policy_coverage: PolicyCoverage,
    pub(crate) execution_accounting: ExecutionAccounting,
    pub(crate) doctests: DoctestAccounting,
}

