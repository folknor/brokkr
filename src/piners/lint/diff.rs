//! Classify a probe's two [`DiagSet`]s into a disposition.
//!
//! Inputs are each validator's parse *result*: `Ok(set)` of normalized
//! diagnostics or `Err(reason)` when the tool produced no parsable output.
//! The three agreement outcomes are pinnable; tool failures are typed
//! separately and cannot satisfy a pin. A divergence signature is carried by
//! the `Divergent` variant instead of an optional side field.

use std::collections::BTreeSet;

use super::{DiagSet, PinnableDisposition};

/// Which validator failed to produce a comparable diagnostic set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validator {
    Piners,
    PineLint,
}

/// The classified result for one probe. Only the three agreement states are
/// pinnable; a validator failure carries its source and cannot accidentally
/// compare equal to a registry expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintDisposition {
    AgreeClean,
    AgreeFlagged,
    Divergent(Signature),
    ToolError(Validator),
}

impl LintDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgreeClean => "agree_clean",
            Self::AgreeFlagged => "agree_flagged",
            Self::Divergent(_) => "divergent",
            Self::ToolError(Validator::Piners) => "piners_error",
            Self::ToolError(Validator::PineLint) => "lint_error",
        }
    }

    pub fn pinnable(self) -> Option<PinnableDisposition> {
        match self {
            Self::AgreeClean => Some(PinnableDisposition::AgreeClean),
            Self::AgreeFlagged => Some(PinnableDisposition::AgreeFlagged),
            Self::Divergent(_) => Some(PinnableDisposition::Divergent),
            Self::ToolError(_) => None,
        }
    }

    pub fn signature(self) -> Option<Signature> {
        match self {
            Self::Divergent(signature) => Some(signature),
            Self::AgreeClean | Self::AgreeFlagged | Self::ToolError(_) => None,
        }
    }

    pub fn is_tool_error(self) -> bool {
        matches!(self, Self::ToolError(_))
    }
}

/// How a `divergent` probe's two diagnostic sets disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signature {
    /// pine-lint's diagnostics are a strict subset of piners' (piners flags
    /// extra). `L ⊂ P`.
    PinersOnly,
    /// piners' diagnostics are a strict subset of pine-lint's. `P ⊂ L`.
    LintOnly,
    /// Same set of `(line, col)` positions, but at least one severity
    /// differs.
    SeverityMismatch,
    /// Anything else: the position sets overlap partially or disjointly.
    Mixed,
}

impl Signature {
    pub fn as_str(self) -> &'static str {
        match self {
            Signature::PinersOnly => "piners_only",
            Signature::LintOnly => "lint_only",
            Signature::SeverityMismatch => "severity_mismatch",
            Signature::Mixed => "mixed",
        }
    }
}

/// The classified result and diagnostic details for one probe.
#[derive(Debug, Clone)]
pub struct LintOutcome {
    pub disposition: LintDisposition,
    /// Count of piners diagnostics (error+warning), 0 on a piners error.
    pub piners_count: usize,
    /// Count of pine-lint diagnostics (error+warning), 0 on a lint error.
    pub lint_count: usize,
    /// The failure reason when a validator produced no parsable output.
    pub error: Option<String>,
}

/// Classify one probe. A validator error short-circuits: piners' takes
/// precedence (it is the tool under test, and a broken piners makes the
/// pine-lint comparison meaningless).
pub fn classify(piners: Result<&DiagSet, &str>, lint: Result<&DiagSet, &str>) -> LintOutcome {
    match (piners, lint) {
        (Err(e), _) => LintOutcome {
            disposition: LintDisposition::ToolError(Validator::Piners),
            piners_count: 0,
            lint_count: lint.map(BTreeSet::len).unwrap_or(0),
            error: Some(e.to_owned()),
        },
        (_, Err(e)) => LintOutcome {
            disposition: LintDisposition::ToolError(Validator::PineLint),
            piners_count: piners.map(BTreeSet::len).unwrap_or(0),
            lint_count: 0,
            error: Some(e.to_owned()),
        },
        (Ok(p), Ok(l)) => {
            let disposition = if p == l {
                if p.is_empty() {
                    LintDisposition::AgreeClean
                } else {
                    LintDisposition::AgreeFlagged
                }
            } else {
                LintDisposition::Divergent(signature(p, l))
            };
            LintOutcome {
                disposition,
                piners_count: p.len(),
                lint_count: l.len(),
                error: None,
            }
        }
    }
}

/// Refine a `divergent` pair into a [`Signature`]. Precondition: `p != l`.
fn signature(p: &DiagSet, l: &DiagSet) -> Signature {
    if l.is_subset(p) {
        return Signature::PinersOnly;
    }
    if p.is_subset(l) {
        return Signature::LintOnly;
    }
    // Same positions, differing severities => severity mismatch.
    let p_pos: BTreeSet<(usize, Option<usize>)> = p.iter().map(|k| (k.line, k.col)).collect();
    let l_pos: BTreeSet<(usize, Option<usize>)> = l.iter().map(|k| (k.line, k.col)).collect();
    if p_pos == l_pos {
        Signature::SeverityMismatch
    } else {
        Signature::Mixed
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::piners::lint::{DiagKey, Severity};

    fn set(keys: &[(usize, usize, Severity)]) -> DiagSet {
        keys.iter()
            .map(|&(line, col, severity)| DiagKey {
                line,
                col: Some(col),
                severity,
            })
            .collect()
    }

    #[test]
    fn both_empty_is_agree_clean() {
        let out = classify(Ok(&DiagSet::new()), Ok(&DiagSet::new()));
        assert_eq!(out.disposition, LintDisposition::AgreeClean);
        assert_eq!(out.disposition.signature(), None);
    }

    #[test]
    fn identical_nonempty_is_agree_flagged() {
        let s = set(&[(3, 5, Severity::Error)]);
        let out = classify(Ok(&s), Ok(&s));
        assert_eq!(out.disposition, LintDisposition::AgreeFlagged);
    }

    #[test]
    fn piners_superset_is_piners_only() {
        let p = set(&[(3, 5, Severity::Error), (4, 1, Severity::Warning)]);
        let l = set(&[(3, 5, Severity::Error)]);
        let out = classify(Ok(&p), Ok(&l));
        assert_eq!(
            out.disposition,
            LintDisposition::Divergent(Signature::PinersOnly)
        );
    }

    #[test]
    fn lint_superset_is_lint_only() {
        let p = set(&[(3, 5, Severity::Error)]);
        let l = set(&[(3, 5, Severity::Error), (4, 1, Severity::Warning)]);
        let out = classify(Ok(&p), Ok(&l));
        assert_eq!(out.disposition.signature(), Some(Signature::LintOnly));
    }

    #[test]
    fn same_position_different_severity_is_severity_mismatch() {
        let p = set(&[(3, 5, Severity::Error)]);
        let l = set(&[(3, 5, Severity::Warning)]);
        let out = classify(Ok(&p), Ok(&l));
        assert_eq!(
            out.disposition.signature(),
            Some(Signature::SeverityMismatch)
        );
    }

    #[test]
    fn disjoint_positions_are_mixed() {
        let p = set(&[(3, 5, Severity::Error)]);
        let l = set(&[(9, 2, Severity::Error)]);
        let out = classify(Ok(&p), Ok(&l));
        assert_eq!(out.disposition.signature(), Some(Signature::Mixed));
    }

    #[test]
    fn piners_error_takes_precedence() {
        let l = set(&[(3, 5, Severity::Error)]);
        let out = classify(Err("boom"), Ok(&l));
        assert_eq!(
            out.disposition,
            LintDisposition::ToolError(Validator::Piners)
        );
        assert_eq!(out.error.as_deref(), Some("boom"));
        assert_eq!(out.lint_count, 1);
    }

    #[test]
    fn lint_error_when_piners_ok() {
        let p = set(&[(3, 5, Severity::Error)]);
        let out = classify(Ok(&p), Err("no json"));
        assert_eq!(
            out.disposition,
            LintDisposition::ToolError(Validator::PineLint)
        );
        assert_eq!(out.piners_count, 1);
    }
}
