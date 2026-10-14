// Per-binary costs carried between runs, so one pure allocator can hand out
// threads in proportion to how long a binary actually takes.
//
// WHY COUNT IS THE WRONG WEIGHT. The allocation's derivation - minimise
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
    #[serde(with = "duration_millis")]
    pub(crate) serial: Duration,
    /// Its single longest test. No number of threads takes the binary below
    /// this, so it is where extra slots stop buying anything.
    #[serde(with = "duration_millis")]
    pub(crate) slowest: Duration,
}

/// Keep the on-disk cache compact and independent of serde's representation
/// for `Duration`: both fields are integer milliseconds.
mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}

/// `[[check]] entry name -> package/kind:target identity -> cost`. The entry
/// name, not the lane-qualified sweep label: `default` and `tier1/default` run
/// the same binaries, and keying by label made every profile re-pay the warm-up.
type Store = BTreeMap<String, BTreeMap<String, BinaryCost>>;

fn store_path(state_root: &Path) -> std::path::PathBuf {
    crate::clean::layout::ProjectState::new(state_root).parallel_timings_file()
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

/// Allocate each binary a proportional share of the test budget and order
/// the result largest-first. Test counts estimate missing history; recorded
/// serial durations replace those estimates. The function is pure so a whole
/// allocation shape can be checked without launching a test process.
pub(crate) fn allocate(
    binaries: &[(String, u32)],
    history: Option<&BTreeMap<String, BinaryCost>>,
    budget: u32,
) -> Vec<(String, u32)> {
    let budget = budget.max(1);
    let (known_nanos, known_tests) = binaries
        .iter()
        .filter_map(|(key, count)| {
            let serial = history?.get(key)?.serial;
            (serial > Duration::ZERO && *count > 0)
                .then_some((serial.as_nanos(), u128::from(*count)))
        })
        .fold((0_u128, 0_u128), |(nanos, tests), (cost, count)| {
            (nanos.saturating_add(cost), tests.saturating_add(count))
        });

    let allocated = binaries
        .iter()
        .map(|(key, count)| {
            let cost = history.and_then(|store| store.get(key)).copied();
            let weight = cost
                .filter(|cost| cost.serial > Duration::ZERO)
                .map(|cost| cost.serial.as_nanos())
                .unwrap_or_else(|| {
                    known_nanos
                        .saturating_mul(u128::from(*count))
                        .checked_div(known_tests)
                        .map_or_else(|| u128::from(*count), |average| average.max(1))
                });
            (key.clone(), *count, weight, cost)
        })
        .collect::<Vec<_>>();
    let total_weight = allocated
        .iter()
        .map(|(_, _, weight, _)| *weight)
        .fold(0_u128, u128::saturating_add);

    let mut result = allocated
        .into_iter()
        .map(|(key, count, weight, cost)| {
            let share = u128::from(budget)
                .saturating_mul(weight)
                .checked_div(total_weight)
                .unwrap_or(1);
            let slots = u32::try_from(share).unwrap_or(budget).clamp(1, budget);
            let slots = slots
                .min(count.max(1))
                .min(useful_slot_cap(cost).unwrap_or(u32::MAX).max(1));
            (key, slots)
        })
        .collect::<Vec<_>>();
    result.sort_by(|(key_a, slots_a), (key_b, slots_b)| {
        slots_b.cmp(slots_a).then_with(|| key_a.cmp(key_b))
    });
    result
}

/// Merge this sweep's measured times into the store and write it back.
///
/// Merged rather than replaced so a sweep run under a narrowing filter does
/// not discard the times of binaries it did not run this time.
pub(crate) fn timings_record(state_root: &Path, sweep: &str, measured: &[(String, BinaryCost)]) {
    let mut store = timings_load(state_root);
    // The old `<package>/<target>` identity cannot distinguish a library
    // harness from a same-named bin or integration target. It is no longer
    // consulted, so drop those stale cache keys on the next recording pass.
    let mut pruned_legacy = false;
    for entries in store.values_mut() {
        entries.retain(|key, _| {
            let keep = !is_legacy_timing_key(key);
            pruned_legacy |= !keep;
            keep
        });
    }
    if measured.is_empty() && !pruned_legacy {
        return;
    }
    if !measured.is_empty() {
        let entry = store.entry(sweep.to_owned()).or_default();
        for (key, cost) in measured {
            entry.insert(key.clone(), *cost);
        }
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

fn is_legacy_timing_key(key: &str) -> bool {
    key.rsplit_once('/')
        .is_some_and(|(_, target)| !target.contains(':'))
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
    if c.slowest.is_zero() || c.serial.is_zero() {
        return None;
    }
    let serial = c.serial.as_nanos();
    let slowest = c.slowest.as_nanos();
    let quotient = serial / slowest;
    let ratio = if serial % slowest == 0 {
        quotient
    } else {
        quotient + 1
    };
    Some(u32::try_from(ratio).unwrap_or(u32::MAX).max(1))
}

#[cfg(test)]
mod binary_timings_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // With nothing recorded the weights ARE the test counts, so a fresh tree
    // allocates exactly as it did before the store existed.
    #[test]
    fn no_history_falls_back_to_counts() {
        assert_eq!(
            allocate(&[("one".into(), 10)], None, 8),
            vec![("one".into(), 8)]
        );
    }

    fn cost(serial_ms: u64, slowest_ms: u64) -> Option<BinaryCost> {
        Some(BinaryCost {
            serial: Duration::from_millis(serial_ms),
            slowest: Duration::from_millis(slowest_ms),
        })
    }

    // No number of threads takes a binary below its longest single test, so
    // slots past that ratio buy nothing and are withheld from binaries that
    // could still use them.
    #[test]
    fn the_useful_cap_is_where_extra_threads_stop_helping() {
        // 22.3s of work whose longest test is 10.1s: three threads reach the
        // floor, a fourth cannot beat it.
        assert_eq!(useful_slot_cap(cost(22_300, 10_100)), Some(3));
        // Evenly divisible work still needs the rounded-up thread count.
        assert_eq!(useful_slot_cap(cost(10_000, 5_000)), Some(2));
        // A binary that is one long test cannot use a second thread at all.
        assert_eq!(useful_slot_cap(cost(10_100, 10_100)), Some(1));
    }

    // No measurement, or a degenerate one, means no known limit - the
    // caller's count cap is then the only bound.
    #[test]
    fn an_unknown_or_degenerate_cost_imposes_no_cap() {
        assert_eq!(useful_slot_cap(None), None);
        assert_eq!(useful_slot_cap(cost(10_000, 0)), None);
        assert_eq!(useful_slot_cap(cost(0, 1_000)), None);
    }

    // THE WHOLE ALLOCATION REGRESSION. The earlier helpers passed in
    // isolation, while their composition starved the small critical-path
    // binary. Its useful cap is three threads, but count weighting would give
    // it only one.
    #[test]
    fn allocation_weights_the_slow_small_binary_and_caps_its_claim() {
        let history = BTreeMap::from([
            (
                "slow".into(),
                BinaryCost {
                    serial: Duration::from_millis(22_300),
                    slowest: Duration::from_millis(10_100),
                },
            ),
            (
                "fast".into(),
                BinaryCost {
                    serial: Duration::from_millis(38_000),
                    slowest: Duration::from_millis(20),
                },
            ),
        ]);
        let binaries = [("slow".into(), 10), ("fast".into(), 2214)];
        let allocated = allocate(&binaries, Some(&history), 24);
        assert_eq!(allocated, vec![("fast".into(), 15), ("slow".into(), 3)]);

        // Under count weighting, ten tests out of 2224 get exactly one slot.
        let by_count = allocate(&binaries, None, 24);
        assert_eq!(
            by_count
                .iter()
                .find(|(key, _)| key == "slow")
                .unwrap()
                .1,
            1
        );
    }

    // A binary nobody has measured is estimated from its siblings' mean cost
    // per test, in the same duration unit as recorded costs.
    #[test]
    fn allocation_estimates_missing_history_from_known_binaries() {
        let history = BTreeMap::from([
            (
                "a".into(),
                BinaryCost {
                    serial: Duration::from_secs(10),
                    slowest: Duration::from_secs(1),
                },
            ),
            (
                "b".into(),
                BinaryCost {
                    serial: Duration::from_secs(5),
                    slowest: Duration::from_secs(1),
                },
            ),
        ]);
        let allocated = allocate(
            &[("a".into(), 100), ("b".into(), 100), ("new".into(), 40)],
            Some(&history),
            18,
        );
        assert_eq!(
            allocated,
            vec![("a".into(), 10), ("b".into(), 5), ("new".into(), 3)]
        );
    }

    // Every claim remains satisfiable, even if the budget is smaller than the
    // binary count or a caller passes zero.
    #[test]
    fn allocation_claims_are_clamped_to_a_usable_budget() {
        let binaries: Vec<(String, u32)> = [1_u32, 3, 4, 9, 100, u32::MAX]
            .into_iter()
            .enumerate()
            .map(|(index, count)| (index.to_string(), count))
            .collect();
        for (key, slots) in allocate(&binaries, None, 4) {
            let count = binaries
                .iter()
                .find(|(candidate, _)| candidate == &key)
                .unwrap()
                .1;
            assert!((1..=4).contains(&slots), "count {count}");
        }
        assert_eq!(
            allocate(&[("empty".into(), 0)], None, 0),
            vec![("empty".into(), 1)]
        );
    }

    // Fat binaries keep room for each other; no one binary claims the pool
    // merely because it contains at least as many tests as the budget.
    #[test]
    fn allocation_leaves_room_for_multiple_large_binaries() {
        let binaries: Vec<(String, u32)> = (0..8).map(|i| (i.to_string(), 300)).collect();
        let allocated = allocate(&binaries, None, 24);
        assert!(allocated.iter().all(|(_, slots)| *slots == 3));
        assert_eq!(allocated.iter().map(|(_, slots)| *slots).sum::<u32>(), 24);
    }

    #[test]
    fn a_store_round_trips_through_toml() {
        let mut store: Store = Store::new();
        store.entry("default".into()).or_default().insert(
            "pkg/some_target".into(),
            BinaryCost {
                serial: Duration::from_millis(22_300),
                slowest: Duration::from_millis(10_100),
            },
        );
        let text = toml::to_string(&store).unwrap();
        let back: Store = toml::from_str(&text).unwrap();
        let got = back["default"]["pkg/some_target"];
        assert_eq!(got.serial, Duration::from_millis(22_300));
        assert_eq!(got.slowest, Duration::from_millis(10_100));
        assert!(text.contains("serial = 22300"));
        assert!(text.contains("slowest = 10100"));
    }

    // A store written with float seconds no longer parses.
    // That must degrade to "no history" and cost one warm-up run, never fail
    // the check - the file is advisory in every direction.
    #[test]
    fn a_store_written_with_float_seconds_no_longer_parses() {
        let old = "[default.\"pkg/some_target\"]\nserial = 22.3\nslowest = 10.1\n";
        assert!(toml::from_str::<Store>(old).is_err());
    }
}
