// Per-binary wall times carried between runs, so a parallel sweep can hand
// out threads in proportion to how long a binary actually takes.
//
// WHY COUNT IS THE WRONG WEIGHT. `claim_slots`' derivation - minimise
// `max(c_i / k_i)` by holding `c_i / k_i` equal - is right, but `c` has to be
// the binary's COST. Using its test count silently assumes every test costs
// the same, and a suite where that is false pays for the assumption in the
// worst possible place: the binary that is the critical path.
//
// Measured on the tree this was built for. 2224 tests over 35 binaries at
// budget 24, and the sweep's slowest binary held about ten latency-bound
// integration tests - so its count-share computed to `24 * 10 / 2224`, floored
// to ONE slot. Its eight slowest tests then ran end to end for 22.3s of a
// 24.5s sweep, where overlapping them would have cost 10.1s: the duration of
// its own slowest test, and nothing more. Meanwhile a fat unit-test binary
// with hundreds of millisecond tests drew seven slots it had no use for.
//
// The costs in that suite cluster on round numbers - 10.115, 6.015, 5.002,
// 3.006, 1.003 - because they are ceilings and timeouts being waited out
// rather than work being done. That is worth stating plainly: such a suite is
// not competing for cores at all, so thread count is nearly free and the only
// thing the allocation decides is which binary finishes last.
//
// WHY A FILE RATHER THAN A GUESS. Duration cannot be derived from the config
// or from a listing; it has to be measured, which means the first run of a
// new binary has nothing to go on. So the store warms up: a binary with no
// record is estimated from the known binaries' mean cost per test, which is a
// far better prior than counting tests as equal, and the estimate is replaced
// by a measurement as soon as the binary has run once.
//
// The file is brokkr-owned state under `state_root/.brokkr/`, alongside the
// other measurement stores, and is advisory in every direction: a missing,
// unreadable or stale file costs a worse allocation and never a wrong result.
// Nothing here may fail a run.

// `include!` puts every check_cmd file in one namespace, so BTreeMap, Path,
// PathBuf and Duration are already imported by a sibling.

/// `sweep label -> binary label -> seconds`.
type Store = BTreeMap<String, BTreeMap<String, f64>>;

fn store_path(state_root: &Path) -> std::path::PathBuf {
    state_root.join(".brokkr").join("parallel-timings.toml")
}

/// Read the recorded times, or an empty store.
///
/// Every failure path yields an empty store rather than an error: this feeds
/// an allocation heuristic, and a run must never fail because a cache did not
/// parse.
pub(crate) fn timings_load(state_root: &Path) -> Store {
    std::fs::read_to_string(store_path(state_root))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// Merge this sweep's measured times into the store and write it back.
///
/// Merged rather than replaced so a sweep run under a narrowing filter does
/// not discard the times of binaries it did not run this time.
pub(crate) fn timings_record(state_root: &Path, sweep: &str, measured: &[(String, Duration)]) {
    if measured.is_empty() {
        return;
    }
    let mut store = timings_load(state_root);
    let entry = store.entry(sweep.to_owned()).or_default();
    for (label, elapsed) in measured {
        entry.insert(label.clone(), elapsed.as_secs_f64());
    }

    let path = store_path(state_root);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(text) = toml::to_string(&store) {
        // Best effort by design - see the module header. A read-only or full
        // filesystem degrades the next run's allocation and nothing else.
        drop(std::fs::write(&path, text));
    }
}

/// The weight to allocate one binary's slots by: its measured seconds, or an
/// estimate when it has never run.
///
/// `mean_cost` is the known binaries' mean seconds per test. When nothing is
/// known it is `None` and the weight falls back to the test count, which
/// reproduces the count-proportional behaviour exactly - so a first run in a
/// fresh tree behaves as it did before this store existed.
pub(crate) fn timing_weight_for(
    recorded: Option<f64>,
    count: u32,
    mean_cost: Option<f64>,
) -> f64 {
    match (recorded, mean_cost) {
        // A measurement of zero is real (a binary whose tests are instant) but
        // useless as a weight, since a zero share would floor to one slot
        // anyway - so it is left to the estimate rather than pinning the
        // binary at nothing.
        (Some(s), _) if s > 0.0 => s,
        (_, Some(mean)) => f64::from(count) * mean,
        _ => f64::from(count),
    }
}

/// Mean seconds per test across the binaries that have a recorded time.
///
/// `None` when nothing is known, which is what puts the whole plan back on
/// count-proportional weights rather than mixing units.
pub(crate) fn mean_cost_per_test(known: &[(f64, u32)]) -> Option<f64> {
    let (secs, tests): (f64, u64) = known
        .iter()
        .fold((0.0, 0), |(s, t), (sec, c)| (s + sec, t + u64::from(*c)));
    (tests > 0 && secs > 0.0).then(|| secs / tests as f64)
}

#[cfg(test)]
mod binary_timings_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // With nothing recorded the weights ARE the test counts, so a fresh tree
    // allocates exactly as it did before the store existed.
    #[test]
    fn no_history_falls_back_to_counts() {
        assert!(mean_cost_per_test(&[]).is_none());
        assert!((timing_weight_for(None, 10, None) - 10.0).abs() < f64::EPSILON);
    }

    // The regression this exists for: a slow binary with few tests must
    // outweigh a fast binary with many.
    #[test]
    fn a_slow_binary_outweighs_a_bigger_fast_one() {
        let slow = timing_weight_for(Some(22.3), 10, Some(0.01));
        let fast = timing_weight_for(Some(2.0), 900, Some(0.01));
        assert!(slow > fast, "slow {slow} should outweigh fast {fast}");
    }

    // A binary nobody has measured is estimated from what its siblings cost
    // per test - a better prior than treating every test as equal, and one
    // that keeps a single unit across the plan.
    #[test]
    fn an_unmeasured_binary_is_estimated_from_its_siblings() {
        let mean = mean_cost_per_test(&[(10.0, 100), (5.0, 100)]).unwrap();
        assert!((mean - 0.075).abs() < 1e-9);
        assert!((timing_weight_for(None, 40, Some(mean)) - 3.0).abs() < 1e-9);
    }

    // A recorded zero must not pin a binary to no weight at all.
    #[test]
    fn a_zero_measurement_defers_to_the_estimate() {
        let w = timing_weight_for(Some(0.0), 8, Some(0.5));
        assert!((w - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_store_round_trips_through_toml() {
        let mut store: Store = Store::new();
        store
            .entry("default".into())
            .or_default()
            .insert("pkg/some_target".into(), 22.3);
        let text = toml::to_string(&store).unwrap();
        let back: Store = toml::from_str(&text).unwrap();
        assert!((back["default"]["pkg/some_target"] - 22.3).abs() < f64::EPSILON);
    }
}
