//! Classify a probe's two [`DiagSet`]s into a disposition.
//!
//! Inputs are each validator's parse *result*: `Ok(set)` of normalized
//! diagnostics or `Err(reason)` when the tool produced no parsable output.
//! The disposition is the gated unit ([`super::DISPOSITION_LABELS`]); the
//! [`Signature`] refines a `divergent` outcome for the breakdown but is never
//! gated - mirroring how `corpus` gates the tier and keeps `count_tier`
//! diagnostic.

use std::collections::BTreeSet;

use super::DiagSet;

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

/// The classified result for one probe.
#[derive(Debug, Clone)]
pub struct LintOutcome {
    /// The gated disposition label (one of [`super::DISPOSITION_LABELS`]).
    pub disposition: &'static str,
    /// Divergence breakdown; `Some` only when `disposition == "divergent"`.
    pub signature: Option<Signature>,
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
            disposition: "piners_error",
            signature: None,
            piners_count: 0,
            lint_count: lint.map(BTreeSet::len).unwrap_or(0),
            error: Some(e.to_owned()),
        },
        (_, Err(e)) => LintOutcome {
            disposition: "lint_error",
            signature: None,
            piners_count: piners.map(BTreeSet::len).unwrap_or(0),
            lint_count: 0,
            error: Some(e.to_owned()),
        },
        (Ok(p), Ok(l)) => {
            let (disposition, signature) = if p == l {
                if p.is_empty() {
                    ("agree_clean", None)
                } else {
                    ("agree_flagged", None)
                }
            } else {
                ("divergent", Some(signature(p, l)))
            };
            LintOutcome {
                disposition,
                signature,
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
        assert_eq!(out.disposition, "agree_clean");
        assert!(out.signature.is_none());
    }

    #[test]
    fn identical_nonempty_is_agree_flagged() {
        let s = set(&[(3, 5, Severity::Error)]);
        let out = classify(Ok(&s), Ok(&s));
        assert_eq!(out.disposition, "agree_flagged");
    }

    #[test]
    fn piners_superset_is_piners_only() {
        let p = set(&[(3, 5, Severity::Error), (4, 1, Severity::Warning)]);
        let l = set(&[(3, 5, Severity::Error)]);
        let out = classify(Ok(&p), Ok(&l));
        assert_eq!(out.disposition, "divergent");
        assert_eq!(out.signature, Some(Signature::PinersOnly));
    }

    #[test]
    fn lint_superset_is_lint_only() {
        let p = set(&[(3, 5, Severity::Error)]);
        let l = set(&[(3, 5, Severity::Error), (4, 1, Severity::Warning)]);
        let out = classify(Ok(&p), Ok(&l));
        assert_eq!(out.signature, Some(Signature::LintOnly));
    }

    #[test]
    fn same_position_different_severity_is_severity_mismatch() {
        let p = set(&[(3, 5, Severity::Error)]);
        let l = set(&[(3, 5, Severity::Warning)]);
        let out = classify(Ok(&p), Ok(&l));
        assert_eq!(out.signature, Some(Signature::SeverityMismatch));
    }

    #[test]
    fn disjoint_positions_are_mixed() {
        let p = set(&[(3, 5, Severity::Error)]);
        let l = set(&[(9, 2, Severity::Error)]);
        let out = classify(Ok(&p), Ok(&l));
        assert_eq!(out.signature, Some(Signature::Mixed));
    }

    #[test]
    fn piners_error_takes_precedence() {
        let l = set(&[(3, 5, Severity::Error)]);
        let out = classify(Err("boom"), Ok(&l));
        assert_eq!(out.disposition, "piners_error");
        assert_eq!(out.error.as_deref(), Some("boom"));
        assert_eq!(out.lint_count, 1);
    }

    #[test]
    fn lint_error_when_piners_ok() {
        let p = set(&[(3, 5, Severity::Error)]);
        let out = classify(Ok(&p), Err("no json"));
        assert_eq!(out.disposition, "lint_error");
        assert_eq!(out.piners_count, 1);
    }
}
