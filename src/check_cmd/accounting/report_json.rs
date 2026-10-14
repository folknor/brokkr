// Schema-2 JSON blocks derived from reconciliation and policy coverage.

use super::journal::{Termination, TerminationScope};
use super::reconcile::{DoctestObserved, Outcome, Reconciliation};

// ----- the `--json` summary, schema 2 -----

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct TerminationSummary {
    pub(crate) kind: String,
    pub(crate) scope: String,
}

impl TerminationSummary {
    /// A recorded termination, with its scope named for a reader: the lane's
    /// label for a lane or stream, `run` otherwise.
    pub(crate) fn of(t: &Termination, label: impl Fn(usize) -> Option<String>) -> Self {
        let scope = match (t.scope, t.lane) {
            (TerminationScope::Run, _) | (_, None) => "run".to_owned(),
            (_, Some(l)) => label(l).unwrap_or_else(|| format!("lane {l}")),
        };
        Self { kind: t.cause.as_str().to_owned(), scope }
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

