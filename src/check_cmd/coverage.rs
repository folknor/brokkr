// Coverage accounting - the phase that makes
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
    /// Non-run pairs of shapes whose every sweep is a `curated = true`
    /// entry - exempt by declaration, reported, never orphaned.
    curated: usize,
    /// Non-run, unjustified pairs. Any value above zero failed the check.
    orphaned: usize,
    /// Declared `skip` / `only` filters that matched nothing. Any value above
    /// zero failed the check. Carried in the summary because a dead filter is
    /// otherwise invisible in the counts - it moves no pair between buckets,
    /// which is exactly why the orphan audit cannot see it.
    dead_filters: usize,
}

/// One build shape's enumeration: the full universe, the `#[ignore]`d
/// subset, and the union of every lane's ran-set. Each element is a
/// `(unit, test)` pair, where the unit is the package name for a
/// libtest-only shape and the nextest binary id (`pkg`, `pkg::target`,
/// `pkg::bin/name`) for a shape any of whose lanes runs under the nextest
/// engine. The finer key wins shape-wide because the projection between
/// the two keyings is non-injective - two binaries in one package can
/// define the same test path (measured: seven such collisions inside
/// nautilus-infrastructure alone), and a mixed shape keyed coarse would
/// merge pairs one lane distinguishes. With the build shape this map is
/// keyed on, the full coverage pair is (build shape, unit, test).
struct ShapeCoverage {
    label: String,
    /// Every sweep producing this shape is `curated = true`, so its
    /// non-run pairs are exempt from the universe by declaration. Keyed
    /// on the sweeps, not the shape: one non-curated sweep sharing the
    /// shape claims the full universe and keeps the shape audited.
    curated: bool,
    universe: BTreeSet<(String, String)>,
    ignored: BTreeSet<(String, String)>,
    ran: BTreeSet<(String, String)>,
}

/// The package half of a pair unit. A bare package name maps to itself; a
/// nextest binary id (`pkg::target`, `pkg::bin/name`) yields its package
/// prefix - which is what keeps package-scoped `[[quarantine]]` entries and
/// qualified skips meaning "every binary id in X" across both keyings.
fn package_of(unit: &str) -> &str {
    unit.split_once("::").map_or(unit, |(p, _)| p)
}

/// The pair unit for one test binary under the shape's keying. For a
/// nextest-keyed shape this is nextest's own `RustBinaryId` construction,
/// used deliberately so the libtest lanes' claims and the engine listing's
/// ids can never drift apart - both sides derive the id through the same
/// nextest code.
///
/// The kind must be NORMALIZED the way nextest's `BinaryList` normalizes it
/// before the id is built: every lib-like crate-type (`lib`, `rlib`,
/// `dylib`, `cdylib`, `staticlib`) collapses to the lib kind, whose id is
/// the bare package name. A crate declaring
/// `crate-type = ["rlib", "staticlib", "cdylib"]` reports its lib target's
/// kind as `rlib` in the artifact stream, and passing that raw yielded
/// `pkg::rlib/target` against the engine's plain `pkg` - which orphaned
/// every lib-binary pair whose only runner was the engine lane (39 of them
/// on the consuming workspace's first migrated gate run). Matching on
/// brokkr's single stored kind is equivalent to nextest's any-in-list rule,
/// because only lib targets carry multiple kinds and all of theirs are
/// lib-like.
fn binary_unit(b: &TestBinary, nextest_keyed: bool) -> String {
    if !nextest_keyed {
        return b.package.clone();
    }
    let kind = match b.kind.as_str() {
        "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" => "lib",
        other => other,
    };
    nextest_metadata::RustBinaryId::from_parts(
        &b.package,
        &nextest_metadata::RustTestBinaryKind::new(kind.to_owned()),
        &b.target,
    )
    .to_string()
}

/// A declared `skip` / `only` filter that matched nothing in the lane it was
/// declared on - the filter-side twin of a stale `[[quarantine]]` entry.
///
/// A dead `skip` is a name that drifted: whatever it excluded runs again under
/// a name nobody wrote down, or it will silently start catching an unrelated
/// test that grows into the substring later. A dead `only` is worse, because
/// the lane then evaluates nothing at all - a sweep declared to carry a
/// contract, whose filter no longer matches, is a gate that has stopped
/// existing while still appearing in the config as evidence the contract is
/// checked. Neither subtracts anything from the lane's claim, so the orphan
/// audit cannot see either: no pair goes non-run, so nothing is orphaned.
struct DeadFilter {
    /// Every sweep it was judged against, rendered - a profile-level filter is
    /// judged against the union of them (see [`FilterLedger`]), so naming one
    /// would misreport where the check actually looked.
    sweeps: String,
    origin: String,
    label: String,
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
        "quarantine: {} entries, {pairs} pairs - {} (--triage to list)",
        quarantine.len(),
        breakdown.join(", ")
    )
}

/// The two declared narrowings of the universe, said out loud so a
/// narrowed claim never reads as a full one: package-level exclusion is
/// outside the pair audit entirely (the binaries cannot even build), and
/// curated sweeps have their shapes' non-run pairs exempted by declaration
/// (`curated_pairs` is the audit's count of those).
fn report_declared_narrowing(sweeps: &[ResolvedSweep], curated_pairs: usize) {
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
    let curated: Vec<&str> = sweeps
        .iter()
        .filter(|s| s.curated)
        .map(|s| s.label.as_str())
        .collect();

    if !curated.is_empty() {
        output::run_msg(&format!(
            "coverage: curated sweeps - non-run pairs exempt from the \
             universe ({curated_pairs} pairs): {}",
            curated.join(", ")
        ));
    }
}

/// One audited unit per cargo RESOLUTION, not per configured sweep.
///
/// Under package mode a two-package lane is one lane but two independent cargo
/// graphs. A single `ShapeCoverage` spanning both would claim a graph boundary
/// that does not exist - one enumeration, one ignored set and one curated
/// verdict standing in for two compiles - and would pull enumeration back
/// toward the batched multi-`-p` command the mode exists to avoid.
///
/// Returns first-seen order alongside the map so the report follows
/// declaration order rather than hash order.
type ResolutionKey = (profile::BuildShapeKey, Option<String>);

/// Doc-only sweeps sit outside the pair audit, and the narrowing is
/// reported like `test_exclude_packages`: visible on every run rather than
/// silently absent.
fn report_doc_only_exclusion(sweeps: &[ResolvedSweep]) {
    let doc_only = sweeps.iter().filter(|s| s.doc_only).count();
    if doc_only > 0 {
        output::run_msg(&format!(
            "coverage: {doc_only} doc-only sweep{} outside the pair audit (doctests are not \
             enumerable)",
            if doc_only == 1 { "" } else { "s" }
        ));
    }
}

fn group_by_resolution(
    sweeps: &[ResolvedSweep],
) -> (Vec<ResolutionKey>, HashMap<ResolutionKey, Vec<usize>>) {
    let mut order: Vec<ResolutionKey> = Vec::new();
    let mut groups: HashMap<ResolutionKey, Vec<usize>> = HashMap::new();
    for (idx, sweep) in sweeps.iter().enumerate() {
        // Doc-only sweeps are outside the pair audit: doctests cannot be
        // enumerated, and enumerating the shape's TEST BINARIES for a lane
        // that deliberately runs none would orphan every pair. Skipped in
        // place so `executed[idx]` keeps its original indexing.
        if sweep.doc_only {
            continue;
        }
        for resolution in sweep.resolutions(&sweep.packages) {
            let key = (sweep.build_shape_key(), resolution);
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(idx);
        }
    }
    (order, groups)
}

#[allow(clippy::too_many_arguments)]
fn run_coverage_phase(
    project_root: &Path,
    state_root: &Path,
    sweeps: &[ResolvedSweep],
    executed: &[bool],
    quarantine: &[QuarantineEntry],
    allow_flags: &[String],
    limit: usize,
    triage: bool,
    commands: bool,
) -> CoverageOutcome {
    let (shapes, dead) =
        match enumerate_shapes(project_root, state_root, sweeps, executed, allow_flags, commands) {
            Ok(s) => s,
            Err(e) => return CoverageOutcome::aborted(e),
        };
    let mut report = classify(&shapes, quarantine);
    report.stats.dead_filters = dead.len();
    let stats = Some(report.stats);

    report_doc_only_exclusion(sweeps);

    // The per-entry pair counts are the countdown the ledger exists for,
    // and the growth signal when a substring starts matching more than it
    // used to - but one line per entry is a page of them on a real ledger.
    // Rolled up per issue by default, which keeps both signals; `--triage`
    // restores the entry-by-entry listing.
    if triage {
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
    report_declared_narrowing(sweeps, report.stats.curated);

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
        let cap = if triage { usize::MAX } else { limit };
        for orphan in report.orphans.iter().take(cap) {
            output::error(&format!("orphaned: {orphan} (run nowhere, quarantined nowhere)"));
        }

        if report.orphans.len() > cap {
            output::error(&format!(
                "... and {} more (rerun with --triage)",
                report.orphans.len() - cap
            ));
        }
        output::error(&format!(
            "{}: every skipped test needs a [[quarantine]] \
             entry with an issue, or a lane that runs it under this build shape",
            output::count(report.orphans.len(), "orphaned pair")
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

    // Reported alongside the other two findings rather than instead of them:
    // a run can have orphans, stale entries and dead filters at once, and each
    // is a different edit in a different file.
    for d in &dead {
        output::error(&format!(
            "dead filter: {} in {} - matches no test in any sweep it applies \
             to ({})",
            d.label, d.origin, d.sweeps
        ));
    }

    if !dead.is_empty() {
        output::error(&format!(
            "{} dead `skip`/`only` filter{}: a filter that selects nothing is a \
             name that drifted, not a no-op - a dead `skip` no longer excludes \
             what it names, and a dead `only` leaves its lane evaluating \
             nothing while still reading as a gate. Fix the substring or delete \
             the filter.",
            dead.len(),
            if dead.len() == 1 { "" } else { "s" }
        ));
    }

    if !report.orphans.is_empty() || !stale.is_empty() || !dead.is_empty() {
        return CoverageOutcome { stats, result: Err(DevError::Build("coverage failed".into())) };
    }

    let curated_frag = if report.stats.curated > 0 {
        format!("{} curated, ", report.stats.curated)
    } else {
        String::new()
    };
    output::run_msg(&format!(
        "coverage: {} shapes, {} pairs - {} run, {} quarantined, {} ignored, {curated_frag}0 orphaned",
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
/// `allow_flags` is the test phase's `[lints] allow` set, and it is not
/// optional here: enumeration COMPILES (`cargo test --no-run`), so it is in the
/// test phase's class rather than clippy's - a lint the project's `-Dwarnings`
/// turns into an error kills the build before any diagnostic exists to filter.
/// Omitting it cost a whole gate: every lane green, then the audit failing last
/// on two `deprecated` errors the injection exists to suppress.
fn enumerate_shapes(
    project_root: &Path,
    state_root: &Path,
    sweeps: &[ResolvedSweep],
    executed: &[bool],
    allow_flags: &[String],
    commands: bool,
) -> Result<(Vec<ShapeCoverage>, Vec<DeadFilter>), DevError> {
    let (order, groups) = group_by_resolution(sweeps);

    // cargo's resolved target dir: a rustflags shape's isolated dir hangs off
    // it (S3-20), and enumeration must reuse the *same* dir the test phase
    // built into or it would rebuild the whole shape from scratch.
    let meta_target_dir = build::project_info(Some(project_root))?.target_dir;

    let mut ledger = FilterLedger::default();
    let mut out = Vec::with_capacity(order.len());
    for key in &order {
        let (_, resolution) = key;
        let members = &groups[key];
        let first = &sweeps[members[0]];
        // Same shape => same env by construction (env is in the key), and
        // rustflags shapes keep their isolated target dir so enumeration
        // never causes a cross-shape rebuild.
        //
        // The lint allows resolve per shape, exactly as the test phase resolves
        // them per sweep, and for the same reason: a shape carrying `rustflags`
        // exports an env var, so the env is the live layer for it whatever the
        // config chain says. Resolving them here rather than taking the test
        // phase's answer also keeps the rebuild rule intact - a mismatch in
        // either direction re-fingerprints the shape and rebuilds it.
        let (env_allows, allow_args) =
            rustflags::plumbing(project_root, !first.rustflags.is_empty(), allow_flags);
        let mut env_owned: Vec<(String, String)> = first
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env_owned.extend(sweep_cargo_env(first, &meta_target_dir, env_allows));
        let env_refs: Vec<(&str, &str)> = env_owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Under package mode the enumeration is scoped to this resolution's
        // one package, matching the one cargo command the lane actually ran
        // for it. Enumerating the lane's whole package list here would
        // catalogue binaries from a batched graph nothing built.
        let bare = resolution_enumeration_args(first, resolution.as_deref(), allow_args.clone());
        // Per-binary enumeration (feature 11): the artifact stream gives
        // package attribution, direct-binary `--list` gives the names.
        // libtest's `--list` includes `#[ignore]`d tests regardless of
        // `--include-ignored` (verified empirically), so the ignored set
        // comes from `--list --ignored`, which lists ONLY ignored tests.
        let Some(binaries) = test_binaries(project_root, &bare, &env_refs, commands)? else {
            return Err(DevError::Build("coverage enumeration failed".into()));
        };
        // The finer key wins shape-wide: any nextest lane on the shape keys
        // the whole shape's pairs by binary id, and libtest lanes' claims are
        // attributed to the binaries they were enumerated from - which they
        // already are, since enumeration is per binary and only aggregated
        // afterward. No information is missing on either side.
        let nextest_keyed = members
            .iter()
            .any(|&idx| sweeps[idx].harness == crate::config::Harness::Nextest);
        let libdir = toolchain_libdir(project_root, &env_refs)?;
        let (universe, ignored, per_binary) =
            enumerate_shape_universe(project_root, &binaries, nextest_keyed, &env_refs, &libdir)?;
        // The liveness machinery speaks (package, test) - a package-scoped
        // filter must not be compared against a binary id - so a nextest-keyed
        // shape derives package-keyed views of the same sets.
        let ignored_pkg: BTreeSet<(String, String)> = ignored
            .iter()
            .map(|(u, t)| (package_of(u).to_owned(), t.clone()))
            .collect();

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
            // Everything the lane's own binaries contain, before any of its
            // filters apply: the reference set the alive-check runs against.
            let candidates: Vec<(String, String)> = lane_binaries
                .iter()
                .filter_map(|b| per_binary.get(&b.executable))
                .flat_map(|pairs| pairs.iter())
                .map(|(u, t)| (package_of(u).to_owned(), t.clone()))
                .collect();
            // Recorded, not decided: a profile-level filter spans the
            // profile's sweeps, and those sit in DIFFERENT build shapes (an
            // unscoped sweep and a package-scoped one are two shapes by
            // construction), so the verdict cannot be taken inside this loop.
            for (filter, live) in filter_liveness(sweep, &candidates, &ignored_pkg) {
                ledger.record(filter, live, &sweep.label);
            }
            if sweep.harness == crate::config::Harness::Nextest {
                nextest_lane_claims(
                    project_root,
                    state_root,
                    sweep,
                    &allow_args,
                    &env_refs,
                    commands,
                    &mut ran,
                )?;
                continue;
            }
            let mut libtest: Vec<&str> = sweep.name_filters.iter().map(String::as_str).collect();
            libtest.extend(sweep.libtest_args.iter().map(String::as_str));
            let inc = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
            for b in lane_binaries {
                let unit = binary_unit(b, nextest_keyed);
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
                    let pair = (unit.clone(), t);

                    if !inc && ignored.contains(&pair) {
                        continue;
                    }
                    ran.insert(pair);
                }
            }
        }

        out.push(ShapeCoverage {
            // The resolution is part of the name under package mode: an
            // orphan report that said only "daemon-package" would not say
            // which of the lane's resolutions is missing the pair.
            label: match resolution {
                Some(pkg) => format!("{}/{pkg}", first.label),
                None => first.label.clone(),
            },
            curated: members.iter().all(|&idx| sweeps[idx].curated),
            universe,
            ignored,
            ran,
        });
    }
    Ok((out, ledger.dead()))
}

/// A shape's universe and `#[ignore]`d set from per-binary listings, plus
/// the per-binary pair lists themselves. Kept per binary, not just folded
/// into the universe: a lane narrows the binary set with `--test <target>`,
/// and a filter must be judged against the names the LANE can see - judging
/// against the shape's universe would report a skip alive on the strength of
/// a match inside a binary the lane narrowed away. The listings are already
/// being fetched here, so retaining them costs nothing. libtest's `--list`
/// includes `#[ignore]`d tests regardless of `--include-ignored`, so the
/// ignored set comes from `--list --ignored`, which lists only them.
type ShapeUniverse = (
    BTreeSet<(String, String)>,
    BTreeSet<(String, String)>,
    HashMap<String, Vec<(String, String)>>,
);

fn enumerate_shape_universe(
    project_root: &Path,
    binaries: &[TestBinary],
    nextest_keyed: bool,
    env_refs: &[(&str, &str)],
    libdir: &str,
) -> Result<ShapeUniverse, DevError> {
    let mut universe: BTreeSet<(String, String)> = BTreeSet::new();
    let mut ignored: BTreeSet<(String, String)> = BTreeSet::new();
    let mut per_binary: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for b in binaries {
        let unit = binary_unit(b, nextest_keyed);
        let Some(all) = binary_list(b, project_root, &["--include-ignored"], env_refs, libdir)?
        else {
            return Err(DevError::Build("coverage enumeration failed".into()));
        };
        let pairs: Vec<(String, String)> = all.into_iter().map(|t| (unit.clone(), t)).collect();
        universe.extend(pairs.iter().cloned());
        per_binary.insert(b.executable.clone(), pairs);
        let Some(ig) = binary_list(b, project_root, &["--ignored"], env_refs, libdir)? else {
            return Err(DevError::Build("coverage enumeration failed".into()));
        };
        ignored.extend(ig.into_iter().map(|t| (unit.clone(), t)));
    }
    Ok((universe, ignored, per_binary))
}

/// One nextest lane's contribution to its shape's ran-set: the engine's own
/// listing under the sweep's real filters - the same code path that selects
/// its run, so audit and execution can never disagree. `Selected` is the
/// only crediting verdict; the engine marking a test `Ignored` already
/// encodes the include-ignored policy, and qualified skips ride the
/// filtersets. A verdict the ledger has no policy for refuses the audit -
/// an unaccounted reason is not an audited pair.
#[allow(clippy::too_many_arguments)]
fn nextest_lane_claims(
    project_root: &Path,
    state_root: &Path,
    sweep: &ResolvedSweep,
    allow_args: &[String],
    env_refs: &[(&str, &str)],
    commands: bool,
    ran: &mut BTreeSet<(String, String)>,
) -> Result<(), DevError> {
    let mut selection = allow_args.to_vec();
    selection.extend(shape_selection_args(sweep));
    selection.extend(sweep.cargo_test_filters.iter().cloned());
    let cases =
        nextest_shape_cases(project_root, state_root, sweep, &selection, env_refs, commands)?;
    for case in cases {
        match case.disposition {
            Disposition::Selected => {
                let _ = ran.insert((case.binary_id, case.test));
            }
            Disposition::Ignored | Disposition::Unmatched => {}
            Disposition::Unclassified => {
                return Err(DevError::Build(format!(
                    "coverage: the engine reported a filter verdict the ledger has no policy \
                     for on {}::{} - an unaudited reason cannot be accounted, so the audit \
                     refuses.",
                    case.binary_id, case.test
                )));
            }
        }
    }
    Ok(())
}

/// Every filter on `sweep` that matched nothing it could have matched.
///
/// The two kinds are checked against different sets, because they are dead for
/// different reasons:
///
/// - a `skip` is dead when nothing it could remove EXISTS, so it is judged
///   against the lane's candidates before any filtering;
/// - an `only` is dead when the lane EVALUATES nothing under it, so the
///   qualified skips and (on a lane without `--include-ignored`) the ignored
///   names come out first. An `only` whose every match is skipped or ignored
///   satisfies "matched something" while selecting no work, which is precisely
///   the silently-vanished gate.
///
/// Each filter is asserted INDIVIDUALLY. libtest ORs positional filters, so a
/// lane with a live `only` and a dead one still runs tests - and folding the
/// assertion the way libtest folds the filters would let the live sibling
/// cover for the dead one. Both halves green, nothing evaluated: the failure
/// family this check exists for.
///
/// The post-skip set is computed here rather than read from a second listing.
/// libtest's `--skip` and positional filters are plain substring matches (no
/// `--exact` is ever in a lane's argv - `libtest_args` is built from
/// include-ignored, skips and thread count alone), so the local computation is
/// exact, and a listing per binary per filter is not.
fn filter_liveness<'a>(
    sweep: &'a ResolvedSweep,
    candidates: &[(String, String)],
    ignored: &BTreeSet<(String, String)>,
) -> Vec<(&'a DeclaredFilter, bool)> {
    let include_ignored = sweep.libtest_args.iter().any(|a| a == "--include-ignored");
    let name_skips: Vec<&DeclaredFilter> = sweep
        .declared_filters
        .iter()
        .filter(|f| f.kind == FilterKind::Skip)
        .collect();

    let evaluated: Vec<&(String, String)> = candidates
        .iter()
        .filter(|(pkg, test)| {
            if !include_ignored && ignored.contains(&((*pkg).clone(), (*test).clone())) {
                return false;
            }
            if sweep.qualified_skips.iter().any(|q| q.matches(pkg, test)) {
                return false;
            }
            !name_skips.iter().any(|f| f.matches(pkg, test))
        })
        .collect();

    sweep
        .declared_filters
        .iter()
        .map(|f| {
            let live = match f.kind {
                FilterKind::Skip => candidates.iter().any(|(pkg, test)| f.matches(pkg, test)),
                FilterKind::Only => evaluated.iter().any(|(pkg, test)| f.matches(pkg, test)),
            };
            (f, live)
        })
        .collect()
}

/// Liveness accumulated across every sweep a filter applies to.
///
/// THE REFERENCE SET IS THE FILTER'S SCOPE, NOT THE LANE'S. A `[[check]]`
/// filter belongs to one entry and a profile-level filter is declared once
/// against the profile's whole sweep list, so the author's claim differs:
/// "this test should not run in this sweep" versus "…in this profile". The
/// latter is satisfied by matching anywhere the profile runs.
///
/// Judging a profile filter per sweep was a real defect, not a strictness
/// setting. Any profile combining an unscoped sweep with a package-scoped one
/// reports a false death for essentially every entry - each skip names a test
/// outside the scoped sweep's packages, so it is necessarily dead there while
/// doing its job in the unscoped sweep. The only ways to silence it would be
/// to stop scoping sweeps or to stop skipping tests.
///
/// The scope key is the PROVENANCE, which makes the two cases one rule rather
/// than a branch: `origin` already names either the profile block or the
/// specific `[[check]]` entry, so unioning over every sweep that carries the
/// same `(origin, kind, pattern, package)` gives a profile filter the union
/// across the profile's sweeps and an entry filter the union across the lanes
/// running that entry - exactly the two intended reference sets.
///
/// Nothing is lost in the direction the feature is for: a filter dead in every
/// sweep it applies to still has no live sighting, and still reports.
#[derive(Default)]
struct FilterLedger {
    /// First-seen order, so the report follows declaration order rather than
    /// hash order. Linear lookup: a config carries filters in the tens.
    tallies: Vec<FilterTally>,
}

struct FilterTally {
    origin: String,
    label: String,
    /// Matched something in at least one sweep it applies to.
    live: bool,
    /// Every sweep it was judged against, for the report - a dead filter's
    /// remedy depends on which lanes were even looking.
    sweeps: Vec<String>,
}

impl FilterLedger {
    fn record(&mut self, filter: &DeclaredFilter, live: bool, sweep: &str) {
        let label = filter.label();
        match self
            .tallies
            .iter_mut()
            .find(|t| t.origin == filter.origin && t.label == label)
        {
            Some(t) => {
                t.live |= live;
                if !t.sweeps.iter().any(|s| s == sweep) {
                    t.sweeps.push(sweep.to_owned());
                }
            }
            None => self.tallies.push(FilterTally {
                origin: filter.origin.clone(),
                label,
                live,
                sweeps: vec![sweep.to_owned()],
            }),
        }
    }

    fn dead(self) -> Vec<DeadFilter> {
        self.tallies
            .into_iter()
            .filter(|t| !t.live)
            .map(|t| DeadFilter {
                sweeps: t.sweeps.join(", "),
                origin: t.origin,
                label: t.label,
            })
            .collect()
    }
}

/// The shape's bare cargo selection: packages/excludes + features, no
/// target filters (those are lane narrowing, audited via the ran-sets).
fn shape_selection_args(sweep: &ResolvedSweep) -> Vec<String> {
    // The shape's profile comes first: enumeration compiles
    // (`cargo test --no-run`), and enumerating a release shape in dev would
    // both rebuild the world and list the dev build's tests - a universe for
    // a build the phase never ran.
    let mut args: Vec<String> = sweep_profile_args(sweep);
    // Exactly the same argument as the profile, and the reason this line is
    // not optional: enumerating a package-mode lane under ambient resolution
    // would list the tests of a build nothing ran, and rebuild the shape in
    // both directions on every audit.
    args.extend(sweep.unification_args());
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

/// The enumeration build's selection: the lint allows first, then the shape.
///
/// Prepended rather than appended, matching the process-isolated lane, and a
/// named function rather than two lines at the call site so the property is
/// testable without spawning cargo: the enumeration is assembled from exactly
/// one selection, so an allow that lives in it cannot be dropped on the way.
fn shape_enumeration_args(sweep: &ResolvedSweep, allow_args: Vec<String>) -> Vec<String> {
    let mut args = allow_args;
    args.extend(shape_selection_args(sweep));
    args
}

/// The enumeration selection for ONE cargo resolution of a shape.
///
/// `resolution` is `Some(pkg)` only under package mode, where the lane ran one
/// cargo command per package and the audit must enumerate the same way: a
/// batched `-p a -p b` listing would catalogue binaries from a graph no run
/// produced, which is the batched-versus-independent difference the mode is
/// built on. The package REPLACES the shape's own `-p` list rather than adding
/// to it, since cargo unions selection flags.
fn resolution_enumeration_args(
    sweep: &ResolvedSweep,
    resolution: Option<&str>,
    allow_args: Vec<String>,
) -> Vec<String> {
    let Some(pkg) = resolution else {
        return shape_enumeration_args(sweep, allow_args);
    };
    let mut args = allow_args;
    args.extend(sweep_profile_args(sweep));
    args.extend(sweep.unification_args());
    args.push("-p".to_owned());
    args.push(pkg.to_owned());
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
        curated: 0,
        orphaned: 0,
        // Filled in by the phase: `classify` sees pairs, not filters.
        dead_filters: 0,
    };
    let mut per_entry = vec![0usize; quarantine.len()];
    let mut orphans: Vec<String> = Vec::new();
    for shape in shapes {
        for pair in &shape.universe {
            let (unit, test) = pair;
            // Quarantine `package` scoping means "every binary id in X", so
            // a binary-id unit is matched through its package prefix.
            let package = package_of(unit);
            stats.pairs += 1;

            if shape.ran.contains(pair) {
                stats.run += 1;
                continue;
            }
            // A curated shape's non-run pairs are exempt by declaration -
            // counted before ignored/quarantine so a curated shape never
            // credits (or stales) a `[[quarantine]]` entry.
            if shape.curated {
                stats.curated += 1;
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
                    // The unit, not the extracted package: for a
                    // nextest-keyed shape the binary id is the finer,
                    // actionable address (and it carries the package).
                    orphans.push(format!("{}/{unit}/{test}", shape.label));
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

    use super::{
        classify, filter_liveness, shape_enumeration_args, CoverageStats, DeclaredFilter,
        FilterKind, FilterLedger, QuarantineEntry, ResolvedSweep, ShapeCoverage,
    };
    use crate::config::QualifiedSkip;
    use std::collections::BTreeSet;

    #[test]
    fn enumeration_selection_carries_the_lint_allows() {
        // The audit's `cargo test --no-run` compiles, so it is in the test
        // phase's class: a lint the project's `-Dwarnings` turns into an error
        // kills the build before any diagnostic exists to filter. A green gate
        // failing on its last phase is what a missing injection here costs.
        let sweep = ResolvedSweep {
            packages: vec!["pkg".into()],
            ..ResolvedSweep::default()
        };
        let allows = vec![
            "--config".to_owned(),
            "target.\"cfg(all())\".rustflags=[\"-A\",\"deprecated\"]".to_owned(),
        ];
        let args = shape_enumeration_args(&sweep, allows.clone());
        // Prepended, and the shape survives intact behind it.
        assert_eq!(args[..2], allows[..]);
        assert_eq!(&args[2..], &["-p".to_owned(), "pkg".to_owned()][..]);
        // An env-sink project passes none, and the selection is then the shape
        // alone - no empty-arg residue for cargo to choke on.
        assert_eq!(
            shape_enumeration_args(&sweep, Vec::new()),
            vec!["-p".to_owned(), "pkg".to_owned()]
        );
    }

    fn filter(kind: FilterKind, pattern: &str) -> DeclaredFilter {
        DeclaredFilter {
            kind,
            pattern: pattern.into(),
            package: None,
            origin: "[test.profiles.tier1]".into(),
        }
    }

    fn candidates(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect()
    }

    /// The verdict over a whole profile: every sweep's liveness recorded, then
    /// the ledger asked. Mirrors `enumerate_shapes`, which cannot decide
    /// inside its per-shape loop.
    fn dead_over(lanes: &[(&ResolvedSweep, &[(String, String)])]) -> Vec<String> {
        let ignored = BTreeSet::new();
        let mut ledger = FilterLedger::default();
        for (sweep, cands) in lanes {
            for (f, live) in filter_liveness(sweep, cands, &ignored) {
                ledger.record(f, live, &sweep.label);
            }
        }
        ledger.dead().into_iter().map(|d| d.label).collect()
    }

    fn dead_labels(
        sweep: &ResolvedSweep,
        cands: &[(String, String)],
        ignored: &BTreeSet<(String, String)>,
    ) -> Vec<String> {
        let mut ledger = FilterLedger::default();
        for (f, live) in filter_liveness(sweep, cands, ignored) {
            ledger.record(f, live, &sweep.label);
        }
        ledger.dead().into_iter().map(|d| d.label).collect()
    }

    #[test]
    fn a_profile_filter_lives_if_it_matches_in_any_sweep_it_applies_to() {
        // The false-death shape: a profile declares one skip list and runs an
        // unscoped sweep plus a package-scoped one. `server_only` names a test
        // outside the scoped sweep's packages, so it is NECESSARILY dead there
        // while doing its job in the unscoped sweep. Judged per sweep, every
        // such entry reports dead and the only remedies are to stop scoping
        // sweeps or to stop skipping tests.
        let profile_skip = DeclaredFilter {
            kind: FilterKind::Skip,
            pattern: "server_only".into(),
            package: None,
            origin: "[test.profiles.gate]".into(),
        };
        let workspace = ResolvedSweep {
            label: "workspace".into(),
            declared_filters: vec![profile_skip.clone()],
            ..ResolvedSweep::default()
        };
        let instrumented = ResolvedSweep {
            label: "instrumented".into(),
            declared_filters: vec![profile_skip],
            ..ResolvedSweep::default()
        };
        let wide = candidates(&[("mogwai-server", "server_only_probe")]);
        let scoped = candidates(&[("mogwai-data", "unrelated")]);

        assert!(dead_over(&[(&workspace, &wide), (&instrumented, &scoped)]).is_empty());
        // Order of sweeps cannot matter: a live sighting anywhere settles it.
        assert!(dead_over(&[(&instrumented, &scoped), (&workspace, &wide)]).is_empty());

        // Nothing is lost in the direction the check exists for: dead in every
        // sweep is still dead.
        let elsewhere = candidates(&[("mogwai-server", "renamed")]);
        assert_eq!(
            dead_over(&[(&workspace, &elsewhere), (&instrumented, &scoped)]),
            vec!["skip \"server_only\""]
        );
    }

    #[test]
    fn an_entry_filter_unions_only_over_its_own_entrys_lanes() {
        // Provenance is the scope key, so the same union rule gives an entry
        // filter a narrower reference set: two `[[check]]` entries carrying the
        // same pattern are two filters, and one living cannot cover the other.
        let live_entry = ResolvedSweep {
            label: "workspace".into(),
            declared_filters: vec![DeclaredFilter {
                kind: FilterKind::Skip,
                pattern: "shared_name".into(),
                package: None,
                origin: "[[check]] 'workspace'".into(),
            }],
            ..ResolvedSweep::default()
        };
        let dead_entry = ResolvedSweep {
            label: "instrumented".into(),
            declared_filters: vec![DeclaredFilter {
                kind: FilterKind::Skip,
                pattern: "shared_name".into(),
                package: None,
                origin: "[[check]] 'instrumented'".into(),
            }],
            ..ResolvedSweep::default()
        };
        let wide = candidates(&[("core", "shared_name_test")]);
        let scoped = candidates(&[("data", "unrelated")]);
        assert_eq!(
            dead_over(&[(&live_entry, &wide), (&dead_entry, &scoped)]),
            vec!["skip \"shared_name\""]
        );
    }

    #[test]
    fn a_skip_matching_nothing_is_dead_and_a_matching_one_is_not() {
        let sweep = ResolvedSweep {
            declared_filters: vec![
                filter(FilterKind::Skip, "serial_tests::"),
                filter(FilterKind::Skip, "renamed_away"),
            ],
            ..ResolvedSweep::default()
        };
        let cands = candidates(&[("core", "serial_tests::a"), ("core", "plain")]);
        assert_eq!(
            dead_labels(&sweep, &cands, &BTreeSet::new()),
            vec!["skip \"renamed_away\""]
        );
    }

    #[test]
    fn a_filter_is_judged_against_the_lanes_binaries_not_the_shapes_universe() {
        // The unsoundness the caller's candidate set exists to close: the
        // shape's universe carries `tape::budget`, but this lane narrows to
        // one target with `--test`, so the skip removes nothing HERE. Judged
        // against the wider universe it would read as alive - the same defect
        // this phase exists to catch, one level up.
        let sweep = ResolvedSweep {
            declared_filters: vec![filter(FilterKind::Skip, "tape::budget")],
            ..ResolvedSweep::default()
        };
        let lane_only = candidates(&[("core", "unit::a")]);
        assert_eq!(
            dead_labels(&sweep, &lane_only, &BTreeSet::new()),
            vec!["skip \"tape::budget\""]
        );

        let whole_shape = candidates(&[("core", "unit::a"), ("core", "tape::budget")]);
        assert!(dead_labels(&sweep, &whole_shape, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn each_only_is_asserted_individually() {
        // libtest ORs positional filters, so this lane runs tests and looks
        // healthy. Folding the assertion the way libtest folds the filters
        // would let the live sibling cover for the dead one: both halves
        // green, half of what was declared evaluating nothing.
        let sweep = ResolvedSweep {
            declared_filters: vec![
                filter(FilterKind::Only, "read_market_latency"),
                filter(FilterKind::Only, "write_market_latency"),
            ],
            ..ResolvedSweep::default()
        };
        let cands = candidates(&[("core", "read_market_latency_p99")]);
        assert_eq!(
            dead_labels(&sweep, &cands, &BTreeSet::new()),
            vec!["only \"write_market_latency\""]
        );
    }

    #[test]
    fn an_only_whose_every_match_is_skipped_or_ignored_is_dead() {
        // "Matched something" is satisfied here and the lane still evaluates
        // nothing - the vanished gate. `skip` and `only` therefore run against
        // different sets: the skip below is alive (it removes a real test)
        // while the only it empties is dead.
        let sweep = ResolvedSweep {
            declared_filters: vec![
                filter(FilterKind::Skip, "_slow"),
                filter(FilterKind::Only, "latency"),
            ],
            ..ResolvedSweep::default()
        };
        let cands = candidates(&[("core", "latency_slow"), ("core", "other")]);
        assert_eq!(
            dead_labels(&sweep, &cands, &BTreeSet::new()),
            vec!["only \"latency\""]
        );

        // Same shape via `#[ignore]` rather than a skip: a lane without
        // --include-ignored lists names it will never execute.
        let sweep = ResolvedSweep {
            declared_filters: vec![filter(FilterKind::Only, "latency")],
            ..ResolvedSweep::default()
        };
        let ignored: BTreeSet<(String, String)> =
            candidates(&[("core", "latency_manual")]).into_iter().collect();
        let cands = candidates(&[("core", "latency_manual")]);
        assert_eq!(
            dead_labels(&sweep, &cands, &ignored),
            vec!["only \"latency\""]
        );

        // ...and alive again once the lane lifts the ignore.
        let lifted = ResolvedSweep {
            libtest_args: vec!["--include-ignored".into()],
            ..sweep
        };
        assert!(dead_labels(&lifted, &cands, &ignored).is_empty());
    }

    #[test]
    fn a_qualified_skip_is_dead_when_its_package_has_no_match() {
        // Package scoping is the point: the pattern matches a test in another
        // package, and the entry is still dead where it was written.
        let mut scoped = filter(FilterKind::Skip, "serial_tests::");
        scoped.package = Some("nautilus-infrastructure".into());
        let sweep = ResolvedSweep {
            declared_filters: vec![scoped],
            qualified_skips: vec![QualifiedSkip {
                package: "nautilus-infrastructure".into(),
                pattern: "serial_tests::".into(),
            }],
            ..ResolvedSweep::default()
        };
        let cands = candidates(&[("nautilus-backtest", "serial_tests::t")]);
        assert_eq!(
            dead_labels(&sweep, &cands, &BTreeSet::new()),
            vec!["skip \"serial_tests::\" (package nautilus-infrastructure)"]
        );

        let cands = candidates(&[("nautilus-infrastructure", "serial_tests::t")]);
        assert!(dead_labels(&sweep, &cands, &BTreeSet::new()).is_empty());
    }

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
                curated: false,
                label: "tier1/default".into(),
                universe: set(&[("core", "serial_tests::a"), ("core", "plain")]),
                ignored: set(&[]),
                ran: set(&[("core", "serial_tests::a"), ("core", "plain")]),
            },
            ShapeCoverage {
                curated: false,
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
            curated: false,
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
            curated: false,
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
            curated: false,
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
    fn curated_shape_exempts_non_run_pairs_without_touching_quarantine() {
        // A curated shape's non-run pairs are exempt by declaration: not
        // orphaned, and never credited to a quarantine entry (which would
        // both hide the exemption and un-stale a dead entry).
        let shapes = vec![ShapeCoverage {
            curated: true,
            label: "sim/sim-live".into(),
            universe: set(&[("live", "targeted"), ("live", "rest_a"), ("live", "rest_b")]),
            ignored: set(&[]),
            ran: set(&[("live", "targeted")]),
        }];
        let q = vec![entry("rest_", "B60")];
        let report = classify(&shapes, &q);
        assert_eq!(report.stats.run, 1);
        assert_eq!(report.stats.curated, 2);
        assert_eq!(report.stats.orphaned, 0);
        assert_eq!(report.per_entry, vec![0]);
    }

    #[test]
    fn non_curated_shape_gets_no_exemption() {
        // The entry-keyed guard: the exemption is decided per shape from
        // its sweeps (`enumerate_shapes` sets `curated` only when every
        // member sweep is curated), so a full entry sharing a build shape
        // with a curated one keeps the shape fully audited. Here the shape
        // arrives non-curated and its non-run pair must orphan.
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "sim/sim-common".into(),
            universe: set(&[("common", "a"), ("common", "b")]),
            ignored: set(&[]),
            ran: set(&[("common", "a")]),
        }];
        let report = classify(&shapes, &[]);
        assert_eq!(report.stats.curated, 0);
        assert_eq!(report.stats.orphaned, 1);
        assert_eq!(report.orphans, vec!["sim/sim-common/common/b"]);
    }

    // The unit->package projection behind quarantine scoping and qualified
    // skips across both keyings: a bare package maps to itself, a binary id
    // yields its package prefix, bin-target ids included.
    #[test]
    fn package_of_handles_both_keyings() {
        assert_eq!(super::package_of("nautilus-core"), "nautilus-core");
        assert_eq!(
            super::package_of("nautilus-infrastructure::test_cache_redis"),
            "nautilus-infrastructure"
        );
        assert_eq!(super::package_of("nautilus-cli::bin/nautilus"), "nautilus-cli");
    }

    // The unit is nextest's own id construction so the libtest lanes'
    // claims and the engine listing can never drift: lib harness -> bare
    // package, integration target -> pkg::target, bin harness -> pkg::bin/x.
    #[test]
    fn binary_units_match_nextests_id_format() {
        let bin = |kind: &str, target: &str| super::TestBinary {
            package: "pkg".into(),
            package_id: "path+file:///x/pkg#pkg@0.1.0".into(),
            target: target.into(),
            kind: kind.into(),
            executable: "/t/x".into(),
            manifest_dir: std::path::PathBuf::from("/x/pkg"),
        };
        assert_eq!(super::binary_unit(&bin("lib", "pkg"), true), "pkg");
        assert_eq!(super::binary_unit(&bin("test", "cache_redis"), true), "pkg::cache_redis");
        assert_eq!(super::binary_unit(&bin("bin", "cli"), true), "pkg::bin/cli");
        // Every lib-like crate-type collapses to the bare package name,
        // matching nextest's normalization: a `crate-type = ["rlib", ...]`
        // lib reports kind `rlib`, and passing it raw produced
        // `pkg::rlib/target` against the engine's plain `pkg` - orphaning
        // every lib pair the engine lane alone covered (the 39-orphan
        // migration failure).
        for kind in ["rlib", "dylib", "cdylib", "staticlib"] {
            assert_eq!(super::binary_unit(&bin(kind, "pkg"), true), "pkg", "{kind}");
        }
        // Libtest-only shapes keep the coarse key untouched.
        assert_eq!(super::binary_unit(&bin("test", "cache_redis"), false), "pkg");
    }

    // The B51 collision shape, measured on nautilus-infrastructure: two
    // binaries in one package define the same test path. Under (package,
    // test) that was ONE pair; under (binary-id, test) it is two, and one
    // package-scoped quarantine entry spans both - the exact +N delta the
    // first migrated gate run is checked against.
    #[test]
    fn a_test_path_shared_by_two_binaries_is_two_pairs_one_entry() {
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "serial/default".into(),
            universe: set(&[
                ("nautilus-infrastructure::test_cache_redis", "serial_tests::t"),
                ("nautilus-infrastructure::test_cache_postgres", "serial_tests::t"),
            ]),
            ignored: set(&[]),
            ran: set(&[]),
        }];
        let mut scoped = entry("serial_tests::", "B51");
        scoped.package = Some("nautilus-infrastructure".into());
        let report = classify(&shapes, &[scoped]);
        assert_eq!(report.stats.pairs, 2);
        assert_eq!(report.per_entry, vec![2]);
        assert_eq!(report.stats.orphaned, 0);
    }

    // Package scoping still excludes foreign packages when the unit is a
    // binary id - the scope compares against the id's package prefix, never
    // the whole id.
    #[test]
    fn package_scope_matches_the_id_prefix_not_the_whole_id() {
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "serial/default".into(),
            universe: set(&[
                ("nautilus-infrastructure::test_cache_redis", "serial_tests::t"),
                ("nautilus-backtest::regress", "serial_tests::t"),
            ]),
            ignored: set(&[]),
            ran: set(&[]),
        }];
        let mut scoped = entry("serial_tests::", "B51");
        scoped.package = Some("nautilus-infrastructure".into());
        let report = classify(&shapes, &[scoped]);
        assert_eq!(report.per_entry, vec![1]);
        assert_eq!(
            report.orphans,
            vec!["serial/default/nautilus-backtest::regress/serial_tests::t"]
        );
    }

    #[test]
    fn package_scoped_entry_does_not_absorb_other_packages() {
        // The over-absorption hazard: a pattern written for infrastructure
        // must not justify a same-named pair in backtest, or a test that
        // later stops running lands as accounted instead of orphaned.
        let shapes = vec![ShapeCoverage {
            curated: false,
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
