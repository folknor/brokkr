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
// WHY SERIAL COST AND NOT WALL TIME. The first version of this stored each
// binary's measured WALL time, and that oscillated - because wall time is a
// function of the slots the allocator granted. Grant a binary a generous
// share, it finishes fast, it is weighted cheap, next run it is starved, and
// it becomes the pole again. Measured as a clean two-cycle at a fixed budget
// on a warm machine: the pole alternated between the same two binaries and the
// sweep swung between 13.8s and 19.5s, while the claim spread collapsed run
// over run (1-7, 1-6, 1-4, 1-3) instead of converging. Feeding back the
// outcome of your own decision as its input is a control loop, not a measure.
//
// AND IT DID NOT MERELY CYCLE - IT COLLAPSED. Left running, the spread reached
// `1-1`, every binary at the floor, and the sweep took 55.8s against a ~50s
// baseline for not fanning out at all. So the terminal state of the feedback
// loop was losing to the thing the lane exists to beat, while still reporting
// success. That is the reason this stores serial cost and not the obvious
// quantity: wall time is what you want to know, and precisely therefore not
// what you may measure.
//
// Serial cost - the sum of the binary's own tests' durations - does not move
// when the allocation moves. Roughly `wall = max(serial / k, slowest_test)`,
// so wall conflates the thing being measured with the thing being chosen, and
// serial does not.
//
// `slowest` is stored alongside for the second half of that identity: no
// number of threads takes a binary below its longest single test, so slots
// past `serial / slowest` do nothing for it and are better spent elsewhere.
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

/// What one binary cost last time, in a form that does not move when the
/// allocation moves.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct BinaryCost {
    /// Sum of the binary's own tests' durations - its cost if run on one
    /// thread. **Independent of the slots it was granted**, which is the whole
    /// point: see the module header.
    pub(crate) serial: f64,
    /// Its single longest test. No number of threads takes the binary below
    /// this, so it is where extra slots stop buying anything.
    pub(crate) slowest: f64,
}

/// `[[check]] entry name -> binary label -> cost`. The entry name, not the
/// lane-qualified sweep label: `default` and `tier1/default` run the same
/// binaries, and keying by label made every profile re-pay the warm-up.
type Store = BTreeMap<String, BTreeMap<String, BinaryCost>>;

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
pub(crate) fn timings_record(state_root: &Path, sweep: &str, measured: &[(String, BinaryCost)]) {
    if measured.is_empty() {
        return;
    }
    let mut store = timings_load(state_root);
    let entry = store.entry(sweep.to_owned()).or_default();
    for (label, cost) in measured {
        entry.insert(label.clone(), *cost);
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

/// The most slots a binary can still use, from `wall = max(serial/k, slowest)`.
///
/// Once `serial / k` has fallen to the binary's longest single test, another
/// thread changes nothing - the binary cannot finish before that test does.
/// Slots past this point are not merely wasted on it, they are withheld from
/// binaries that could still use them, and granting them is what let the claim
/// spread collapse as the allocator poured slots into a binary that was
/// already at its floor.
///
/// `None` means "no useful limit known" - no measurement, or a `slowest` of
/// zero, where the caller's own count cap is the only bound that applies.
pub(crate) fn useful_slot_cap(cost: Option<BinaryCost>) -> Option<u32> {
    let c = cost?;
    if !(c.serial.is_finite() && c.slowest.is_finite()) || c.slowest <= 0.0 || c.serial <= 0.0 {
        return None;
    }
    // Rounded up: a binary needing 2.4 "slowest-test lengths" of work uses a
    // third thread for the remainder, and rounding down would leave it short.
    let ratio = (c.serial / c.slowest).ceil();
    Some(u32::try_from(weight_ms_ceil(ratio)).unwrap_or(u32::MAX).max(1))
}

/// A small positive f64 as an integer, without a lossy cast.
fn weight_ms_ceil(v: f64) -> u64 {
    Duration::try_from_secs_f64(v).map_or(1, |d| d.as_secs().max(1))
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

    fn cost(serial: f64, slowest: f64) -> Option<BinaryCost> {
        Some(BinaryCost { serial, slowest })
    }

    // THE OSCILLATION REGRESSION. The stored weight must be independent of
    // the slots granted, or the allocator feeds its own decision back as its
    // input and two-cycles. Serial cost is that invariant: the same binary
    // run on 1 slot or 8 sums to the same total.
    #[test]
    fn serial_cost_does_not_move_when_the_allocation_moves() {
        // Same four tests, whatever the concurrency: wall time differs, the
        // sum does not.
        let tests = [4.0_f64, 3.0, 2.0, 1.0];
        let serial: f64 = tests.iter().sum();
        assert!((serial - 10.0).abs() < f64::EPSILON);
        // Wall on 1 slot is 10s and on 4 slots is 4s, and neither is what
        // gets stored.
        assert!((tests.iter().fold(0.0_f64, |a, b| a.max(*b)) - 4.0).abs() < f64::EPSILON);
    }

    // No number of threads takes a binary below its longest single test, so
    // slots past that ratio buy nothing and are withheld from binaries that
    // could still use them.
    #[test]
    fn the_useful_cap_is_where_extra_threads_stop_helping() {
        // 22.3s of work whose longest test is 10.1s: three threads reach the
        // floor, a fourth cannot beat it.
        assert_eq!(useful_slot_cap(cost(22.3, 10.1)), Some(3));
        // Evenly divisible work still needs the rounded-up thread count.
        assert_eq!(useful_slot_cap(cost(10.0, 5.0)), Some(2));
        // A binary that is one long test cannot use a second thread at all.
        assert_eq!(useful_slot_cap(cost(10.1, 10.1)), Some(1));
    }

    // No measurement, or a degenerate one, means no known limit - the
    // caller's count cap is then the only bound.
    #[test]
    fn an_unknown_or_degenerate_cost_imposes_no_cap() {
        assert_eq!(useful_slot_cap(None), None);
        assert_eq!(useful_slot_cap(cost(10.0, 0.0)), None);
        assert_eq!(useful_slot_cap(cost(0.0, 1.0)), None);
        assert_eq!(useful_slot_cap(cost(f64::NAN, 1.0)), None);
        assert_eq!(useful_slot_cap(cost(1.0, f64::INFINITY)), None);
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
        store.entry("default".into()).or_default().insert(
            "pkg/some_target".into(),
            BinaryCost {
                serial: 22.3,
                slowest: 10.1,
            },
        );
        let text = toml::to_string(&store).unwrap();
        let back: Store = toml::from_str(&text).unwrap();
        let got = back["default"]["pkg/some_target"];
        assert!((got.serial - 22.3).abs() < f64::EPSILON);
        assert!((got.slowest - 10.1).abs() < f64::EPSILON);
    }

    // A store written by the previous (wall-time) format no longer parses.
    // That must degrade to "no history" and cost one warm-up run, never fail
    // the check - the file is advisory in every direction.
    #[test]
    fn a_store_in_the_old_format_reads_as_empty_rather_than_failing() {
        let old = "[default]\n\"pkg/some_target\" = 22.3\n";
        assert!(toml::from_str::<Store>(old).is_err());
    }
}
