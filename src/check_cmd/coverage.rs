// Coverage accounting (TIERED-CHECK.md feature 4) - the phase that makes
// `certifies = "complete"` mean something. Runs only under a complete
// profile, after the test phase (binaries are built and green).
//
// The unit of coverage is the (build shape, test) pair, not the test
// name: a pass under one feature graph is not evidence about another (the
// B41 argument), so the universe is enumerated per distinct build shape
// and subtraction keeps the pair. Enumeration is ground truth, not
// reimplementation: the universe is `--list --include-ignored` with no
// filters, each lane's ran-set is `--list` under the lane's real filter
// argv, and libtest itself decides what each argv admits.
//
// Every non-run pair must be one of:
//  - ignored:     `#[ignore]` at the source, lane runs without
//                 include_ignored - counted and reported, not fatal
//                 (lane policy, visible in the diff that adds the
//                 attribute);
//  - quarantined: matches a `[[quarantine]]` pattern, counted per entry;
//  - orphaned:    anything else - the check fails.
//
// Staleness is mechanical in both directions: a pattern entry justifying
// zero pairs fails the check (delete it when the bug closes), and the
// per-entry pair counts are printed so an entry silently growing (a new
// test riding an old substring) is visible in the trailer.

use std::collections::BTreeSet;

/// Aggregate result of the coverage phase, carried into the `--json`
/// summary (additive under `schema: 1`).
#[derive(serde::Serialize, Clone, Copy)]
struct CoverageStats {
    /// (build shape, test) pairs in the universe.
    pairs: usize,
    /// Pairs some lane runs.
    run: usize,
    /// Non-run pairs justified by a `[[quarantine]]` pattern.
    quarantined: usize,
    /// Non-run pairs whose test is `#[ignore]`d at the source.
    ignored: usize,
    /// Non-run, unjustified pairs. Any value above zero failed the check.
    orphaned: usize,
}

/// One build shape's enumeration, package-qualified: the full universe,
/// the `#[ignore]`d subset, and the union of every lane's ran-set. Each
/// element is a `(package, test)` pair (TIERED-CHECK feature 11 upgraded
/// the coverage pair to (build shape, package, test)).
struct ShapeCoverage {
    label: String,
    universe: BTreeSet<(String, String)>,
    ignored: BTreeSet<(String, String)>,
    ran: BTreeSet<(String, String)>,
}

/// What the audit produced. `stats` is present whenever enumeration and
/// classification completed - including on the two failing paths, so a
/// consumer of a failed audit still gets the counts instead of a null
/// `coverage` object. Only a failure that predates classification
/// (enumeration itself) leaves it `None`.
struct CoverageOutcome {
    stats: Option<CoverageStats>,
    result: Result<(), DevError>,
}

impl CoverageOutcome {
    /// Enumeration died before any counts existed.
    fn aborted(e: DevError) -> Self {
        CoverageOutcome { stats: None, result: Err(e) }
    }
}

/// One line for the whole ledger: entry count, total pairs, and the
/// per-issue breakdown in descending pair order. The breakdown is what
/// carries the countdown and the growth signal that the per-entry listing
/// used to - an issue whose pair count climbs is visible here too, without
/// a line per entry. Issues are first-seen ordered within a tie so the
/// line is stable run to run.
fn quarantine_rollup(quarantine: &[QuarantineEntry], per_entry: &[usize]) -> String {
    let mut issues: Vec<(&str, usize)> = Vec::new();
    for (entry, count) in quarantine.iter().zip(per_entry) {
        match issues.iter_mut().find(|(i, _)| *i == entry.issue) {
            Some((_, total)) => *total += count,
            None => issues.push((entry.issue.as_str(), *count)),
        }
    }
    issues.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let pairs: usize = per_entry.iter().sum();
    let breakdown: Vec<String> = issues
        .iter()
        .map(|(issue, n)| format!("{issue} {n}"))
        .collect();

    format!(
        "quarantine: {} entries, {pairs} pairs - {} (--all to list)",
        quarantine.len(),
        breakdown.join(", ")
    )
}

fn run_coverage_phase(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    executed: &[bool],
    quarantine: &[QuarantineEntry],
    limit: usize,
    all: bool,
    commands: bool,
) -> CoverageOutcome {
    let shapes = match enumerate_shapes(project_root, sweeps, executed, commands) {
        Ok(s) => s,
        Err(e) => return CoverageOutcome::aborted(e),
    };
    let report = classify(&shapes, quarantine);
    let stats = Some(report.stats);

    // The per-entry pair counts are the countdown the ledger exists for,
    // and the growth signal when a substring starts matching more than it
    // used to - but one line per entry is a page of them on a real ledger.
    // Rolled up per issue by default, which keeps both signals; `--all`
    // restores the entry-by-entry listing.
    if all {
        for (entry, count) in quarantine.iter().zip(&report.per_entry) {
            match (&entry.pattern, &entry.category) {
                (Some(p), _) => {
                    let scope = entry
                        .package
                        .as_deref()
                        .map(|pkg| format!("{pkg}: "))
                        .unwrap_or_default();
                    output::run_msg(&format!(
                        "quarantine {} ({scope}{p}): {count} pairs",
                        entry.issue
                    ));
                }
                (None, Some(cat)) => {
                    output::run_msg(&format!("quarantine {} (category {cat})", entry.issue));
                }
                (None, None) => {}
            }
        }
    } else if !quarantine.is_empty() {
        output::run_msg(&quarantine_rollup(quarantine, &report.per_entry));
    }
    // Package-level exclusion is outside the pair audit (the binaries
    // cannot even build); say so rather than hide it.
    let excluding: Vec<String> = sweeps
        .iter()
        .filter(|s| !s.test_exclude_packages.is_empty())
        .map(|s| format!("{} ({})", s.label, s.test_exclude_packages.len()))
        .collect();

    if !excluding.is_empty() {
        output::run_msg(&format!(
            "coverage: sweeps excluding packages from tests - outside the pair \
             audit: {}",
            excluding.join(", ")
        ));
    }

    let stale: Vec<&str> = quarantine
        .iter()
        .zip(&report.per_entry)
        .filter(|(q, n)| q.pattern.is_some() && **n == 0)
        .map(|(q, _)| q.issue.as_str())
        .collect();

    // Both findings are printed before the phase fails: an unhealthy run
    // with stale entries AND orphans needs the orphan worksheet (the very
    // reason this phase runs on failing test phases) just as much as the
    // stale report, and returning on the first hid the other.
    if !report.orphans.is_empty() {
        let cap = if all { usize::MAX } else { limit };
        for orphan in report.orphans.iter().take(cap) {
            output::error(&format!("orphaned: {orphan} (run nowhere, quarantined nowhere)"));
        }

        if report.orphans.len() > cap {
            output::error(&format!(
                "... and {} more (rerun with --all)",
                report.orphans.len() - cap
            ));
        }
        output::error(&format!(
            "{} orphaned pair(s): every skipped test needs a [[quarantine]] \
             entry with an issue, or a lane that runs it under this build shape",
            report.orphans.len()
        ));
    }

    if !stale.is_empty() {
        output::error(&format!(
            "stale [[quarantine]] entries ({}): every matching pair runs (or no \
             pair matches). The ledger must shrink when a suppression is \
             removed - delete the entries.",
            stale.join(", ")
        ));
    }

    if !report.orphans.is_empty() || !stale.is_empty() {
        return CoverageOutcome { stats, result: Err(DevError::Build("coverage failed".into())) };
    }

    output::run_msg(&format!(
        "coverage: {} shapes, {} pairs - {} run, {} quarantined, {} ignored, 0 orphaned",
        shapes.len(),
        report.stats.pairs,
        report.stats.run,
        report.stats.quarantined,
        report.stats.ignored
    ));

    CoverageOutcome { stats, result: Ok(()) }
}

/// Group active sweeps by build shape and enumerate each shape once:
/// universe (`--include-ignored`, no filters), plain listing (to derive
/// the ignored set), and every lane's filtered ran-set.
fn enumerate_shapes(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    executed: &[bool],
    commands: bool,
) -> Result<Vec<ShapeCoverage>, DevError> {
    let mut order: Vec<profile::BuildShapeKey> = Vec::new();
    let mut groups: HashMap<profile::BuildShapeKey, Vec<usize>> = HashMap::new();
    for (idx, sweep) in sweeps.iter().enumerate() {
        let key = sweep.build_shape_key();
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(idx);
    }

    let mut out = Vec::with_capacity(order.len());
    for key in &order {
        let members = &groups[key];
        let first = &sweeps[members[0]];
        // Same shape => same env by construction (env is in the key), and
        // rustflags shapes keep their isolated target dir so enumeration
        // never causes a cross-shape rebuild.
        let mut env_owned: Vec<(String, String)> = first
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env_owned.extend(sweep_cargo_env(first, project_root));
        let env_refs: Vec<(&str, &str)> = env_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let bare = shape_selection_args(first);
        // Per-binary enumeration (feature 11): the artifact stream gives
        // package attribution, direct-binary `--list` gives the names.
        // libtest's `--list` includes `#[ignore]`d tests regardless of
        // `--include-ignored` (verified empirically), so the ignored set
        // comes from `--list --ignored`, which lists ONLY ignored tests.
        let Some(binaries) = test_binaries(project_root, &bare, &env_refs, commands)? else {
            return Err(DevError::Build("coverage enumeration failed".into()));
        };
        let libdir = toolchain_libdir(project_root, &env_refs)?;
        let mut universe: BTreeSet<(String, String)> = BTreeSet::new();
        let mut ignored: BTreeSet<(String, String)> = BTreeSet::new();
        for b in &binaries {
            let Some(all) =
                binary_list(b, project_root, &["--include-ignored"], &env_refs, &libdir)?
            else {
                return Err(DevError::Build("coverage enumeration failed".into()));
            };
            universe.extend(all.into_iter().map(|t| (b.package.clone(), t)));
            let Some(ig) = binary_list(b, project_root, &["--ignored"], &env_refs, &libdir)? else {
                return Err(DevError::Build("coverage enumeration failed".into()));
            };
            ignored.extend(ig.into_iter().map(|t| (b.package.clone(), t)));
        }

        let mut ran: BTreeSet<(String, String)> = BTreeSet::new();
        for &idx in members {
            // A lane the test phase never reached (an earlier sweep failed
            // fast) ran nothing, so it may not credit its filtered set - the
            // universe still carries the shape (enumerated above), so its
            // pairs surface as non-run rather than silently counted as run.
            if !executed[idx] {
                continue;
            }
            let sweep = &sweeps[idx];
            let lane_binaries = filter_binaries(&binaries, &sweep.cargo_test_filters);
            let mut libtest: Vec<&str> = sweep.name_filters.iter().map(String::as_str).collect();
            libtest.extend(sweep.libtest_args.iter().map(String::as_str));
            let inc = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
            for b in lane_binaries {
                let Some(listed) = binary_list(b, project_root, &libtest, &env_refs, &libdir)?
                else {
                    return Err(DevError::Build("coverage enumeration failed".into()));
                };
                for t in listed {
                    // Package-qualified skips narrow the lane's claim; a
                    // lane without `--include-ignored` lists ignored names
                    // it will never execute - subtract both, or the lane
                    // claims coverage it does not provide.
                    if sweep.qualified_skips.iter().any(|q| q.matches(&b.package, &t)) {
                        continue;
                    }
                    let pair = (b.package.clone(), t);

                    if !inc && ignored.contains(&pair) {
                        continue;
                    }
                    ran.insert(pair);
                }
            }
        }

        out.push(ShapeCoverage {
            label: first.label.clone(),
            universe,
            ignored,
            ran,
        });
    }
    Ok(out)
}

/// The shape's bare cargo selection: packages/excludes + features, no
/// target filters (those are lane narrowing, audited via the ran-sets).
fn shape_selection_args(sweep: &ResolvedSweep) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    for pkg in &sweep.packages {
        args.push("-p".into());
        args.push(pkg.clone());
    }

    if !sweep.test_exclude_packages.is_empty() {
        args.push("--workspace".into());
        for pkg in &sweep.test_exclude_packages {
            args.push("--exclude".into());
            args.push(pkg.clone());
        }
    }
    args.extend(sweep.cargo_feature_args.iter().cloned());
    args
}

struct CoverageReport {
    stats: CoverageStats,
    /// Pair count justified per `[[quarantine]]` entry, index-aligned.
    per_entry: Vec<usize>,
    /// `shape-label/package/test-name` for every unjustified non-run pair.
    orphans: Vec<String>,
}

/// Pure pair classification: universe minus ran, partitioned into
/// ignored / quarantined / orphaned per shape. A quarantine entry with a
/// `package` field justifies only that package's pairs - a name-only
/// pattern written for one package must not absorb same-named pairs in
/// every other (the mirror of the ignored-listing bug).
fn classify(shapes: &[ShapeCoverage], quarantine: &[QuarantineEntry]) -> CoverageReport {
    let mut stats = CoverageStats {
        pairs: 0,
        run: 0,
        quarantined: 0,
        ignored: 0,
        orphaned: 0,
    };
    let mut per_entry = vec![0usize; quarantine.len()];
    let mut orphans: Vec<String> = Vec::new();
    for shape in shapes {
        for pair in &shape.universe {
            let (package, test) = pair;
            stats.pairs += 1;

            if shape.ran.contains(pair) {
                stats.run += 1;
                continue;
            }

            if shape.ignored.contains(pair) {
                stats.ignored += 1;
                continue;
            }
            // Most-specific match wins: the longest matching pattern, ties
            // broken by declaration order. First-match-wins misattributed a
            // pair to a broad entry (`test_bar`) that a narrower one
            // (`test_bar_roundtrip`) was written for, leaving the narrower
            // entry crediting zero pairs and failing the stale check - a
            // narrower suppression reported dead while it was doing its job.
            let hit = quarantine
                .iter()
                .enumerate()
                .filter(|(_, q)| {
                    q.pattern.as_deref().is_some_and(|p| test.contains(p))
                        && q.package.as_deref().is_none_or(|pkg| pkg == package)
                })
                .max_by_key(|(i, q)| {
                    (
                        q.pattern.as_deref().map_or(0, str::len),
                        std::cmp::Reverse(*i),
                    )
                })
                .map(|(i, _)| i);
            match hit {
                Some(i) => {
                    per_entry[i] += 1;
                    stats.quarantined += 1;
                }
                None => {
                    stats.orphaned += 1;
                    orphans.push(format!("{}/{package}/{test}", shape.label));
                }
            }
        }
    }
    CoverageReport {
        stats,
        per_entry,
        orphans,
    }
}

#[cfg(test)]
mod coverage_tests {
    #![allow(clippy::unwrap_used)]

    use super::{classify, CoverageStats, QuarantineEntry, ShapeCoverage};
    use std::collections::BTreeSet;

    fn set(pairs: &[(&str, &str)]) -> BTreeSet<(String, String)> {
        pairs
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect()
    }

    fn entry(pattern: &str, issue: &str) -> QuarantineEntry {
        QuarantineEntry {
            pattern: Some(pattern.into()),
            package: None,
            category: None,
            issue: issue.into(),
            reason: "test".into(),
        }
    }

    fn stats_of(shapes: &[ShapeCoverage], q: &[QuarantineEntry]) -> CoverageStats {
        classify(shapes, q).stats
    }

    #[test]
    fn pairs_are_per_shape_not_per_name() {
        // The serial_tests:: hole: run in the default shape's serial lane,
        // skipped in the ffi shape - name-level accounting would call it
        // covered, pair-level accounting reports the ffi pair.
        let shapes = vec![
            ShapeCoverage {
                label: "tier1/default".into(),
                universe: set(&[("core", "serial_tests::a"), ("core", "plain")]),
                ignored: set(&[]),
                ran: set(&[("core", "serial_tests::a"), ("core", "plain")]),
            },
            ShapeCoverage {
                label: "tier1/ffi".into(),
                universe: set(&[("core", "serial_tests::a"), ("core", "plain")]),
                ignored: set(&[]),
                ran: set(&[("core", "plain")]),
            },
        ];
        let report = classify(&shapes, &[]);
        assert_eq!(report.stats.orphaned, 1);
        assert_eq!(report.orphans, vec!["tier1/ffi/core/serial_tests::a"]);

        // A quarantine entry justifies exactly that pair.
        let q = vec![entry("serial_tests::", "B14")];
        let report = classify(&shapes, &q);
        assert_eq!(report.stats.orphaned, 0);
        assert_eq!(report.per_entry, vec![1]);
    }

    #[test]
    fn ignored_pairs_count_separately() {
        let shapes = vec![ShapeCoverage {
            label: "default".into(),
            universe: set(&[("core", "a"), ("core", "slow_manual")]),
            ignored: set(&[("core", "slow_manual")]),
            ran: set(&[("core", "a")]),
        }];
        let stats = stats_of(&shapes, &[]);
        assert_eq!(stats.ignored, 1);
        assert_eq!(stats.orphaned, 0);
        assert_eq!(stats.run, 1);
    }

    #[test]
    fn most_specific_entry_gets_the_credit() {
        // Two entries both match; the longer pattern wins regardless of
        // declaration order (was first-match-wins, which credited the
        // broader entry and starved the narrower one).
        let shapes = vec![ShapeCoverage {
            label: "default".into(),
            universe: set(&[("core", "test_bar_roundtrip")]),
            ignored: set(&[]),
            ran: set(&[]),
        }];
        // "roundtrip" (9) is longer than "test_bar" (8): credit index 1.
        let q = vec![entry("test_bar", "B50"), entry("roundtrip", "B99")];
        let report = classify(&shapes, &q);
        assert_eq!(report.per_entry, vec![0, 1]);
    }

    #[test]
    fn narrower_nested_pattern_is_not_starved() {
        // The S3-16 bug: a broad `test_bar` entry declared before a narrower
        // `test_bar_roundtrip` used to absorb the roundtrip pair, so the
        // narrower entry credited zero pairs and was flagged stale, failing
        // the gate. Most-specific-wins gives each entry its own pairs.
        let shapes = vec![ShapeCoverage {
            label: "default".into(),
            universe: set(&[
                ("core", "test_bar_basic"),
                ("core", "test_bar_roundtrip"),
            ]),
            ignored: set(&[]),
            ran: set(&[]),
        }];
        let q = vec![
            entry("test_bar", "B50"),
            entry("test_bar_roundtrip", "B51"),
        ];
        let report = classify(&shapes, &q);
        // test_bar_basic -> broad entry; test_bar_roundtrip -> narrow entry.
        assert_eq!(report.per_entry, vec![1, 1]);
        assert_eq!(report.stats.orphaned, 0);
    }

    #[test]
    fn package_scoped_entry_does_not_absorb_other_packages() {
        // The over-absorption hazard: a pattern written for infrastructure
        // must not justify a same-named pair in backtest, or a test that
        // later stops running lands as accounted instead of orphaned.
        let shapes = vec![ShapeCoverage {
            label: "serial/default".into(),
            universe: set(&[
                ("nautilus-infrastructure", "serial_tests::t"),
                ("nautilus-backtest", "serial_tests::t"),
            ]),
            ignored: set(&[]),
            ran: set(&[]),
        }];
        let mut scoped = entry("serial_tests::", "B51");
        scoped.package = Some("nautilus-infrastructure".into());
        let report = classify(&shapes, &[scoped]);
        assert_eq!(report.per_entry, vec![1]);
        assert_eq!(report.stats.orphaned, 1);
        assert_eq!(
            report.orphans,
            vec!["serial/default/nautilus-backtest/serial_tests::t"]
        );
    }
}
