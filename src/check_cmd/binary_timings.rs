// Per-test costs carried between runs, so one pure allocator can hand out
// threads in proportion to how long a binary will actually take THIS run.
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
// 24.5s sweep, where overlapping them would have cost 10.1s. Meanwhile a fat
// unit-test binary with hundreds of millisecond tests drew seven slots it had
// no use for.
//
// WHY SERIAL COST AND NOT WALL TIME. The first version of this stored each
// binary's measured WALL time, and that oscillated - because wall time is a
// function of the slots the allocator granted. Grant a binary a generous
// share, it finishes fast, it is weighted cheap, next run it is starved, and
// it becomes the pole again. Measured as a clean two-cycle at a fixed budget
// on a warm machine: the pole alternated between the same two binaries and the
// sweep swung between 13.8s and 19.5s, while the claim spread collapsed run
// over run instead of converging, and left running reached `1-1` and lost to
// not fanning out at all. Serial cost - the sum of the tests' own durations -
// has no direct dependence on the grant. (Contention between tests under a
// wider grant can still move a test's own duration; that is a second-order
// effect, and the store does not pretend to remove it.)
//
// WHY PER TEST AND NOT PER BINARY. A binary's cost depends on WHICH of its
// tests a run executes. One store per binary was overwritten by whatever
// selection ran last: a plain check that leaves a binary's `#[ignore]`d socket
// tests out recorded it as cheap, and the next gate, which includes them,
// scheduled its real pole from that figure and starved it (brokkr#224). The
// same happened under any skip or only filter. So each test's duration is
// recorded under its own name and merged, and a binary's cost for a run is the
// sum over the names that run will execute. A run never overwrites a test it
// did not execute, so no selection can corrupt another's estimate.
//
// NO USEFUL-SLOT CAP. An earlier allocator capped a claim at
// `ceil(serial / slowest)`, read off `wall = max(serial / k, slowest)`. That
// identity is a LOWER BOUND on the makespan, not its value, so the cap could
// withhold a slot that would have helped (tests of 10, 6, 6, 6 seconds: the
// cap says three, three take 12s, four take 10s). An exact list-scheduling
// simulation is only valid when the dispatch order is known for every grant,
// which a custom test runner behind a default-harness manifest can break, and
// a single binary alone in a plan lost slots nobody else could use. The cap's
// original reason was the collapse under wall-time feedback, which serial
// cost already removed, so it is gone.
//
// WHAT "ADVISORY" MEANS HERE. History never decides which tests are selected
// or how a test's result is judged, and a missing, unreadable or old-format
// file is read as empty and never fails a run. But a bad allocation can
// lengthen the test phase, and the phase ceiling judges the lengthened phase,
// so a stale estimate is a performance input that can still reach a timeout.
//
// The file is brokkr-owned state under `state_root/.brokkr/`. Each record
// carries when it was last observed; a name unobserved for
// `RETENTION_SECS` is dropped on the next write, so renamed and deleted tests
// do not accumulate. That is retention only: an old sample is still a better
// prior than the plan mean, so lookups do not age records out.

// `include!` puts every check_cmd file in one namespace, so BTreeMap, Path,
// PathBuf and Duration are already imported by a sibling.

/// One test's last observed duration, and when it was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TestCost {
    /// Integer microseconds, so the file is independent of serde's `Duration`
    /// representation. Not milliseconds: a suite of sub-millisecond tests
    /// would record every one as zero and lose its binaries' relative costs.
    pub(crate) us: u64,
    /// Unix seconds of the run that observed it, for retention.
    pub(crate) seen: u64,
}

/// How long a test name may go unobserved before a write drops it. Wall
/// clock, not a run count: a binary's ignored tests go unobserved through
/// every plain check between two gates, however many there are.
const RETENTION_SECS: u64 = 30 * 24 * 60 * 60;

/// One binary's recorded tests: name -> cost.
pub(crate) type BinaryHistory = BTreeMap<String, TestCost>;

/// `[[check]] entry name -> binary measurement key -> test name -> cost`.
/// The entry name, not the lane-qualified sweep label: `default` and
/// `tier1/default` run the same binaries, and keying by label made every
/// profile re-pay the warm-up.
type Store = BTreeMap<String, BTreeMap<String, BinaryHistory>>;

fn store_path(state_root: &Path) -> std::path::PathBuf {
    crate::clean::layout::ProjectState::new(state_root).parallel_timings_file()
}

/// Read the recorded times, or an empty store.
///
/// Every failure path yields an empty store rather than an error, including
/// the per-binary format an older brokkr wrote, which no longer parses.
pub(crate) fn timings_load(state_root: &Path) -> Store {
    std::fs::read_to_string(store_path(state_root))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// One binary of a plan as the allocator sees it: its measurement key and the
/// names this run will execute.
pub(crate) struct AllocInput<'a> {
    pub(crate) key: String,
    pub(crate) names: Vec<&'a str>,
}

/// Allocate each binary a proportional share of the test budget, returned as
/// `(input index, slots)` ordered largest claim first.
///
/// A binary's weight is the sum of its executed names' recorded durations.
/// An unrecorded name is priced at the mean over every recorded name in this
/// plan's inventories (not the binary's own mean: one cheap sample would price
/// its unseen siblings cheap). With nothing recorded anywhere the weights are
/// the test counts. Claims are clamped to `[1, min(budget, count)]`. Pure, so a
/// whole allocation shape can be checked without launching a test process.
pub(crate) fn allocate(
    inputs: &[AllocInput<'_>],
    history: Option<&BTreeMap<String, BinaryHistory>>,
    budget: u32,
) -> Vec<(usize, u32)> {
    let budget = budget.max(1);
    let lookup = |input: &AllocInput<'_>, name: &str| -> Option<u128> {
        history?
            .get(&input.key)?
            .get(name)
            .map(|cost| u128::from(cost.us))
    };
    let (known_us, known_names) = inputs
        .iter()
        .flat_map(|input| input.names.iter().filter_map(move |name| lookup(input, name)))
        .fold((0_u128, 0_u128), |(us, n), cost| (us.saturating_add(cost), n + 1));

    let weights: Vec<u128> = inputs
        .iter()
        .map(|input| {
            let count = input.names.len() as u128;
            if known_names == 0 {
                // No history at all: weight by count.
                return count;
            }
            let (recorded, unseen) = input.names.iter().fold((0_u128, 0_u128), |(sum, unseen), name| {
                match lookup(input, name) {
                    Some(us) => (sum.saturating_add(us), unseen),
                    None => (sum, unseen + 1),
                }
            });
            // Unseen names at the plan mean, multiplied before dividing so
            // the mean is not rounded once per name. `max(1)` keeps a
            // zero-duration total off the zero-weight path.
            let estimated = unseen.saturating_mul(known_us) / known_names;
            recorded.saturating_add(estimated).max(1)
        })
        .collect();
    let total_weight = weights.iter().copied().fold(0_u128, u128::saturating_add);

    let mut result: Vec<(usize, u32)> = inputs
        .iter()
        .zip(&weights)
        .enumerate()
        .map(|(index, (input, weight))| {
            let share = u128::from(budget)
                .saturating_mul(*weight)
                .checked_div(total_weight)
                .unwrap_or(1);
            let count = u32::try_from(input.names.len()).unwrap_or(u32::MAX).max(1);
            let slots = u32::try_from(share).unwrap_or(budget).clamp(1, budget).min(count);
            (index, slots)
        })
        .collect();
    result.sort_by(|(a, slots_a), (b, slots_b)| {
        slots_b
            .cmp(slots_a)
            .then_with(|| inputs[*a].key.cmp(&inputs[*b].key))
            .then_with(|| a.cmp(b))
    });
    result
}

/// What one binary of a finished sweep observed: its measurement key and each
/// completed test's duration.
pub(crate) struct Observed {
    pub(crate) key: String,
    pub(crate) completed: Vec<(String, Duration)>,
}

/// Merge this sweep's observations into the store and write it back.
///
/// Per test name: a completed observation replaces that name's record (the
/// last sample of a name within the run wins), and every name the run did not
/// complete - unselected, ignored, unstarted, hung - keeps its record. Names
/// unobserved for [`RETENTION_SECS`] are then dropped store-wide.
pub(crate) fn timings_record(state_root: &Path, entry: &str, observed: &[Observed], now: u64) {
    let mut store = timings_load(state_root);
    let entries = store.entry(entry.to_owned()).or_default();
    for binary in observed {
        if binary.completed.is_empty() {
            continue;
        }
        let tests = entries.entry(binary.key.clone()).or_default();
        for (name, duration) in &binary.completed {
            let us = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
            tests.insert(name.clone(), TestCost { us, seen: now });
        }
    }
    expire(&mut store, now);

    let path = store_path(state_root);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(text) = toml::to_string(&store) {
        // Written beside and renamed over, so a kill or a full disk mid-write
        // leaves the previous store whole rather than a truncated file that
        // reads as empty and loses every entry this run did not touch. Best
        // effort: a failed write costs later allocations, never this run.
        let partial = path.with_extension("toml.partial");
        if std::fs::write(&partial, text).is_ok() && std::fs::rename(&partial, &path).is_err() {
            drop(std::fs::remove_file(&partial));
        }
    }
}

/// Drop names unobserved for [`RETENTION_SECS`], then any binary or entry
/// left empty.
fn expire(store: &mut Store, now: u64) {
    let cutoff = now.saturating_sub(RETENTION_SECS);
    for binaries in store.values_mut() {
        for tests in binaries.values_mut() {
            tests.retain(|_, cost| cost.seen >= cutoff);
        }
        binaries.retain(|_, tests| !tests.is_empty());
    }
    store.retain(|_, binaries| !binaries.is_empty());
}

/// Unix seconds now, for [`timings_record`]; zero on a clock before the epoch,
/// which only delays expiry.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod binary_timings_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn input<'a>(key: &str, names: &[&'a str]) -> AllocInput<'a> {
        AllocInput { key: key.into(), names: names.to_vec() }
    }

    fn history(rows: &[(&str, &[(&str, u64)])]) -> BTreeMap<String, BinaryHistory> {
        rows.iter()
            .map(|(key, tests)| {
                let tests = tests
                    .iter()
                    .map(|(name, ms)| ((*name).to_owned(), TestCost { us: *ms * 1_000, seen: 0 }))
                    .collect();
                ((*key).to_owned(), tests)
            })
            .collect()
    }

    fn names(prefix: &str, n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i}")).collect()
    }

    // With nothing recorded the weights ARE the test counts.
    #[test]
    fn no_history_falls_back_to_counts() {
        let n = names("t", 10);
        let refs: Vec<&str> = n.iter().map(String::as_str).collect();
        assert_eq!(allocate(&[input("one", &refs)], None, 8), vec![(0, 8)]);
    }

    // THE TICKET. A plain run records a binary's cheap tests only; the gate,
    // which also executes its slow ignored tests, must price those from their
    // own records, which the plain run never touched.
    #[test]
    fn a_narrower_run_cannot_cheapen_a_wider_selection() {
        let slow: Vec<String> = names("socket", 30);
        let mut all: Vec<&str> = slow.iter().map(String::as_str).collect();
        all.push("unit");
        let mut cli_tests: Vec<(&str, u64)> = slow.iter().map(|n| (n.as_str(), 1_600)).collect();
        cli_tests.push(("unit", 1));
        let rest = names("r", 200);
        let rest_refs: Vec<&str> = rest.iter().map(String::as_str).collect();
        let rest_tests: Vec<(&str, u64)> = rest.iter().map(|n| (n.as_str(), 50)).collect();
        let h = history(&[("cli", &cli_tests), ("rest", &rest_tests)]);

        // Gate: the ignored socket tests are executed, so `cli` is the pole.
        let gate = allocate(&[input("cli", &all), input("rest", &rest_refs)], Some(&h), 24);
        assert_eq!(gate[0], (0, 19), "{gate:?}");
        // Plain: only `unit` executes, and `cli` is cheap for THIS run only.
        let plain = allocate(&[input("cli", &["unit"]), input("rest", &rest_refs)], Some(&h), 24);
        assert_eq!(plain, vec![(1, 23), (0, 1)]);
    }

    // The old cap's failure: four tests of 10, 6, 6, 6 seconds need four
    // slots to reach their 10s floor, and a lone binary must get them all.
    #[test]
    fn a_lone_binary_is_not_capped_below_its_test_count() {
        let h = history(&[("b", &[("a", 10_000), ("b", 6_000), ("c", 6_000), ("d", 6_000)])]);
        assert_eq!(allocate(&[input("b", &["a", "b", "c", "d"])], Some(&h), 8), vec![(0, 4)]);
    }

    // An unrecorded name is priced at the plan-wide mean, not its binary's:
    // `fresh`'s one cheap sample must not price its forty unseen tests cheap.
    #[test]
    fn unseen_names_are_priced_at_the_plan_mean() {
        let unseen = names("new", 40);
        let mut fresh: Vec<&str> = unseen.iter().map(String::as_str).collect();
        fresh.push("cheap");
        let h = history(&[("old", &[("x", 1_000), ("y", 1_000)]), ("fresh", &[("cheap", 1)])]);
        let got = allocate(&[input("old", &["x", "y"]), input("fresh", &fresh)], Some(&h), 24);
        // fresh: 1 + 40 * 667 against old's 2000, so fresh takes the pool.
        assert_eq!(got, vec![(1, 22), (0, 1)]);
    }

    // Every claim remains satisfiable, even if the budget is smaller than the
    // binary count or a caller passes zero.
    #[test]
    fn allocation_claims_are_clamped_to_a_usable_budget() {
        let pool = names("t", 100);
        let inputs: Vec<AllocInput<'_>> = [1_usize, 3, 4, 9, 100]
            .into_iter()
            .enumerate()
            .map(|(i, n)| AllocInput {
                key: i.to_string(),
                names: pool[..n].iter().map(String::as_str).collect(),
            })
            .collect();
        for (_, slots) in allocate(&inputs, None, 4) {
            assert!((1..=4).contains(&slots));
        }
        assert_eq!(allocate(&[input("empty", &[])], None, 0), vec![(0, 1)]);
    }

    // Fat binaries keep room for each other.
    #[test]
    fn allocation_leaves_room_for_multiple_large_binaries() {
        let pool = names("t", 300);
        let refs: Vec<&str> = pool.iter().map(String::as_str).collect();
        let inputs: Vec<AllocInput<'_>> = (0..8).map(|i| input(&i.to_string(), &refs)).collect();
        let allocated = allocate(&inputs, None, 24);
        assert!(allocated.iter().all(|(_, slots)| *slots == 3));
    }

    // Two prepared runs sharing a key are still two runs: the result is by
    // index, so neither disappears from the plan.
    #[test]
    fn same_keyed_inputs_both_get_a_claim() {
        let got = allocate(&[input("k", &["a"]), input("k", &["a"])], None, 4);
        assert_eq!(got.len(), 2);
    }

    fn state(scratch: &str) -> std::path::PathBuf {
        crate::test_scratch::scratch("binary_timings", scratch)
    }

    // A run merges per name: what it did not complete keeps its record, what
    // it did is replaced, and the last sample of a repeated name wins.
    #[test]
    fn recording_merges_per_name() {
        let root = state("timings-merge");
        let ms = Duration::from_millis;
        let gate = Observed {
            key: "k".into(),
            completed: vec![("slow".into(), ms(9_000)), ("unit".into(), ms(5))],
        };
        timings_record(&root, "e", &[gate], 100);
        let plain = Observed {
            key: "k".into(),
            completed: vec![("unit".into(), ms(7)), ("unit".into(), ms(8))],
        };
        timings_record(&root, "e", &[plain], 200);
        let got = &timings_load(&root)["e"]["k"];
        assert_eq!(got["slow"], TestCost { us: 9_000_000, seen: 100 });
        assert_eq!(got["unit"], TestCost { us: 8_000, seen: 200 });
    }

    // Retention: a name unobserved past the window is dropped on the next
    // write, and a binary left empty goes with it.
    #[test]
    fn unobserved_names_expire_on_write() {
        let root = state("timings-expire");
        let ms = Duration::from_millis;
        timings_record(&root, "e", &[Observed { key: "gone".into(), completed: vec![("t".into(), ms(1))] }], 0);
        let later = RETENTION_SECS + 1;
        timings_record(&root, "e", &[Observed { key: "live".into(), completed: vec![("t".into(), ms(1))] }], later);
        let store = timings_load(&root);
        assert!(!store["e"].contains_key("gone"));
        assert!(store["e"].contains_key("live"));
    }

    #[test]
    fn a_store_round_trips_through_toml() {
        let mut store: Store = Store::new();
        store
            .entry("default".into())
            .or_default()
            .entry("pkg test:t".into())
            .or_default()
            .insert("mod::case".into(), TestCost { us: 22_300, seen: 7 });
        let text = toml::to_string(&store).unwrap();
        let back: Store = toml::from_str(&text).unwrap();
        assert_eq!(back["default"]["pkg test:t"]["mod::case"], TestCost { us: 22_300, seen: 7 });
    }

    // Sub-millisecond tests keep their relative costs: a thousand 0.9ms tests
    // outweigh one.
    #[test]
    fn sub_millisecond_costs_keep_their_weight() {
        let many = names("m", 1_000);
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let mut h = BTreeMap::new();
        h.insert("many".to_owned(), many.iter().map(|n| (n.clone(), TestCost { us: 900, seen: 0 })).collect());
        h.insert("one".to_owned(), BTreeMap::from([("x".to_owned(), TestCost { us: 900, seen: 0 })]));
        let got = allocate(&[input("many", &refs), input("one", &["x"])], Some(&h), 8);
        assert_eq!(got, vec![(0, 7), (1, 1)]);
    }

    // The per-binary format an older brokkr wrote no longer parses, which
    // must read as no history rather than fail the check.
    #[test]
    fn the_per_binary_format_no_longer_parses() {
        let old = "[default.\"pkg/test:t\"]\nserial = 22300\nslowest = 10100\n";
        assert!(toml::from_str::<Store>(old).is_err());
    }
}
