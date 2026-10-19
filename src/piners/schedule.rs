//! The order a corpus run hands its probes to the harness.
//!
//! The harness fills its job slots from the front of the manifest, so a slow
//! probe listed late holds up the end of the run while the other slots sit
//! idle. brokkr owns the run history, so it schedules: descending estimated
//! scan duration, each estimate a probe's latest qualifying `runtime_ms`
//! under the same execution context
//! ([`crate::piners::corpus_db::CorpusDb::scheduling_runtimes`]). An estimate
//! is advisory - a wrong one costs wall time, never a verdict - but the order
//! itself can shape a run (co-scheduled heavy probes contend for memory), so
//! the run records the order it chose.

use std::collections::HashMap;

use crate::piners::registry::VerifiedProbe;

/// How the schedule was derived, for the run's message.
#[derive(Debug, PartialEq, Eq)]
pub struct Schedule {
    /// Probes with a recorded estimate.
    pub observed: usize,
    /// Probes without one, placed at the upper median of the observed. Zero
    /// when nothing was observed: there is no median, and nothing moved.
    pub imputed: usize,
}

/// Reorder `verified` slowest first by `estimates` (ms). A probe with no
/// estimate is placed at the upper median of the selection's observed
/// estimates - a bounded prior, where unknown-first would let many cheap
/// unknowns delay a known giant. On equal estimates an observed probe goes
/// before an imputed one; otherwise ties, and a selection with nothing
/// observed, keep selection order.
pub fn slowest_first(verified: &mut [VerifiedProbe], estimates: &HashMap<String, f64>) -> Schedule {
    let mut observed: Vec<f64> = verified
        .iter()
        .filter_map(|v| estimates.get(&v.id).copied())
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
        .collect();
    let count = observed.len();
    if count == 0 {
        return Schedule {
            observed: 0,
            imputed: 0,
        };
    }
    observed.sort_by(f64::total_cmp);
    let median = observed[count / 2];
    // (estimate, observed): an imputed probe ties at the median by
    // construction, so on a tie the observed probe goes first - otherwise a
    // lone known giant, being the median, would wait behind every unknown.
    let estimate = |v: &VerifiedProbe| {
        match estimates
            .get(&v.id)
            .copied()
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
        {
            Some(ms) => (ms, true),
            None => (median, false),
        }
    };
    verified.sort_by(|a, b| {
        let ((ma, oa), (mb, ob)) = (estimate(a), estimate(b));
        mb.total_cmp(&ma).then(ob.cmp(&oa))
    });
    Schedule {
        observed: count,
        imputed: verified.len() - count,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::path::PathBuf;

    use super::*;
    use crate::piners::registry::{FilePin, ProbeConfig};

    fn probe(id: &str) -> VerifiedProbe {
        VerifiedProbe {
            id: id.to_owned(),
            pine: FilePin {
                path: PathBuf::from(format!("validation/{id}/strategy.pine")),
                xxh128: "aa".into(),
            },
            inputs: None,
            csv: None,
            record: None,
            config: ProbeConfig::default(),
        }
    }

    fn order(ids: &[&str], estimates: &[(&str, f64)]) -> (Vec<String>, Schedule) {
        let mut v: Vec<VerifiedProbe> = ids.iter().map(|id| probe(id)).collect();
        let est: HashMap<String, f64> =
            estimates.iter().map(|(k, ms)| ((*k).to_owned(), *ms)).collect();
        let s = slowest_first(&mut v, &est);
        (v.into_iter().map(|p| p.id).collect(), s)
    }

    #[test]
    fn slowest_first_with_unknowns_at_the_upper_median_and_stable_ties() {
        // Observed sorted: 10, 500, 500, 9000 -> upper median 500.
        let (ids, s) = order(
            &["fast", "new", "tie-1", "slow", "tie-2"],
            &[("fast", 10.0), ("slow", 9000.0), ("tie-1", 500.0), ("tie-2", 500.0)],
        );
        assert_eq!(ids, ["slow", "tie-1", "tie-2", "new", "fast"]);
        assert_eq!(s, Schedule { observed: 4, imputed: 1 });
    }

    #[test]
    fn a_known_giant_is_not_held_behind_cheap_unknowns() {
        let (ids, _) = order(&["u1", "u2", "u3", "giant"], &[("giant", 100_000.0)]);
        assert_eq!(ids[0], "giant");
    }

    #[test]
    fn nothing_observed_keeps_selection_order() {
        let (ids, s) = order(&["b", "a", "c"], &[("elsewhere", 5.0), ("a", f64::NAN)]);
        assert_eq!(ids, ["b", "a", "c"]);
        assert_eq!(s, Schedule { observed: 0, imputed: 0 });
    }
}
