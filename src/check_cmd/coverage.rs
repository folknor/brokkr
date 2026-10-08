// Policy coverage - the half of the `coverage` phase that makes
// `certifies = "complete"` mean every test the profile could run is either
// selected by some lane or legitimately excluded. Runs only under a complete
// profile, after the test phase, and reads nothing but the plan the `prepare`
// phase wrote (see accounting.rs for the other half, and prepare.rs for where
// the plan comes from).
//
// The unit of coverage is the pair - (build shape, resolution, binary, test) -
// not the test name: a pass under one feature graph is not evidence about
// another (the B41 argument), and two binaries of one package defining the
// same test path are two tests. Enumeration is ground truth, not
// reimplementation: the universe is each binary's `--list --include-ignored`
// with no filters, each lane's selection is `--list` under the lane's real
// filter argv, and libtest itself decides what each argv admits.
//
// The selection is what the profile CONFIGURES, from the plan - never what the
// test phase happened to reach. A lane a fail-fast never got to still selects
// its pairs; its unrun executions are unobserved in the execution accounting,
// not orphaned pairs here.
//
// Every non-selected pair must be one of:
//  - ignored:     `#[ignore]` at the source, lane runs without
//                 include_ignored - counted and reported, not fatal
//                 (lane policy, visible in the diff that adds the
//                 attribute);
//  - quarantined: matches a `[[quarantine]]` pattern, counted per entry;
//  - curated:     its shape's every lane is a `curated = true` entry;
//  - orphaned:    anything else - the check fails.
//
// Staleness is mechanical in both directions: a pattern entry justifying
// zero pairs fails the check (delete it when the bug closes), and the
// per-entry pair counts are printed so an entry silently growing (a new
// test riding an old substring) is visible in the trailer.

use std::collections::BTreeSet;

/// Aggregate pair counts from classification.
#[derive(Clone, Copy, Default, Debug)]
struct CoverageStats {
    /// Pairs in the universe of every shape that could be classified.
    pairs: usize,
    /// Pairs some lane is expected to execute.
    selected: usize,
    /// Non-selected pairs justified by a `[[quarantine]]` pattern.
    quarantined: usize,
    /// Non-selected pairs whose test is `#[ignore]`d at the source.
    ignored: usize,
    /// Non-selected pairs of shapes whose every sweep is `curated = true`.
    curated: usize,
    /// Non-selected, unjustified pairs. Any value above zero fails the check.
    orphaned: usize,
}

/// One shape resolution's sets, keyed by (binary, test).
struct ShapeCoverage {
    label: String,
    /// Every sweep producing this shape is `curated = true`, so its
    /// non-selected pairs are exempt by declaration. Keyed on the sweeps, not
    /// the shape: one non-curated sweep sharing the shape keeps it audited.
    curated: bool,
    universe: BTreeSet<(BinaryUnit, String)>,
    ignored: BTreeSet<(BinaryUnit, String)>,
    selected: BTreeSet<(BinaryUnit, String)>,
}

/// A declared `skip` / `only` filter that matched nothing in the lanes it was
/// declared on - the filter-side twin of a stale `[[quarantine]]` entry.
///
/// A dead `skip` is a name that drifted: whatever it excluded runs again under
/// a name nobody wrote down, or it will silently start catching an unrelated
/// test that grows into the substring later. A dead `only` is worse, because
/// the lane then evaluates nothing at all - a sweep declared to carry a
/// contract, whose filter no longer matches, is a gate that has stopped
/// existing while still appearing in the config as evidence the contract is
/// checked. Neither subtracts anything from the lane's selection, so the
/// orphan audit cannot see either.
struct DeadFilter {
    /// Every sweep it was judged against, rendered - a profile-level filter is
    /// judged against the union of them (see [`FilterLedger`]), so naming one
    /// would misreport where the check actually looked.
    sweeps: String,
    origin: String,
    label: String,
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
        "quarantine: {} entries, {pairs} pairs - {}",
        quarantine.len(),
        breakdown.join(", ")
    )
}

/// The two declared narrowings of the universe, said out loud so a
/// narrowed claim never reads as a full one: package-level exclusion is
/// outside the pair audit entirely (the binaries cannot even build), and
/// curated sweeps have their shapes' non-selected pairs exempted by
/// declaration (`curated_pairs` is the audit's count of those).
///
/// Returned as clauses rather than printed: they ride on the coverage line
/// itself - the green summary or, on a failing audit, a line of their own
/// ahead of the findings - so `0 orphaned` can never be read apart from what
/// the universe left out.
fn declared_narrowing(sweeps: &[ResolvedSweep], curated_pairs: usize) -> Vec<String> {
    let mut out = Vec::new();
    let excluding: Vec<String> = sweeps
        .iter()
        .filter(|s| !s.test_exclude_packages.is_empty())
        .map(|s| format!("{} ({})", s.label, s.test_exclude_packages.len()))
        .collect();

    if !excluding.is_empty() {
        out.push(format!("packages excluded from tests in {}", excluding.join(", ")));
    }
    let curated: Vec<&str> = sweeps
        .iter()
        .filter(|s| s.curated)
        .map(|s| s.label.as_str())
        .collect();

    if !curated.is_empty() {
        out.push(format!(
            "{curated_pairs} non-selected pairs of curated sweeps {}",
            curated.join(", ")
        ));
    }
    out
}

/// Doc-only sweeps sit outside the pair audit, and the narrowing is
/// reported like `test_exclude_packages`: visible on every run rather than
/// silently absent.
fn doc_only_exclusion(sweeps: &[ResolvedSweep]) -> Option<String> {
    let doc_only = sweeps.iter().filter(|s| s.doc_only).count();
    (doc_only > 0).then(|| {
        format!(
            "{} (doctests are not enumerable)",
            output::count(doc_only, "doc-only sweep")
        )
    })
}

/// The shapes the plan can classify: each shape resolution whose universe was
/// enumerated and whose every lane was prepared, with the union of those
/// lanes' executions as its selection. A shape with an unprepared lane is left
/// out rather than classified on half its selection - its pairs would read as
/// orphaned only because the lane that selects them could not be prepared.
fn shapes_from_plan(plan: &AccountingPlan) -> Vec<ShapeCoverage> {
    plan.shapes
        .iter()
        .filter(|s| s.complete)
        .map(|s| {
            let selected: BTreeSet<(BinaryUnit, String)> = plan
                .lanes
                .iter()
                .filter(|l| l.shape == s.shape)
                .flat_map(|l| l.executions.iter())
                .filter(|p| p.resolution == s.resolution)
                .map(|p| (p.unit.clone(), p.test.clone()))
                .collect();
            ShapeCoverage {
                label: s.label.clone(),
                curated: s.curated,
                universe: s.universe.iter().cloned().collect(),
                ignored: s.ignored.iter().cloned().collect(),
                selected,
            }
        })
        .collect()
}

/// Classify the plan, print the worksheet, and return the policy block plus
/// whether policy failed. Findings print in full - orphans, stale entries and
/// dead filters at once, since each is a different edit in a different file.
///
/// An incomplete plan reports what it knows - the orphans of the shapes it
/// could classify, the dead filters whose every sweep was prepared - and is
/// never a pass. Stale quarantine entries are not judged there: an entry
/// matching nothing in the classified shapes may be matching in a shape that
/// could not be prepared.
#[allow(clippy::too_many_lines)]
pub(crate) fn report_policy(
    plan: &AccountingPlan,
    sweeps: &[ResolvedSweep],
    quarantine: &[QuarantineEntry],
) -> (PolicyCoverage, bool) {
    let shapes = shapes_from_plan(plan);
    let report = classify(&shapes, quarantine);
    let stats = report.stats;

    let mut outside: Vec<String> = doc_only_exclusion(sweeps).into_iter().collect();
    // The per-entry pair counts are the countdown the ledger exists for, and
    // the growth signal when a substring starts matching more than it used to -
    // but one line per entry is a page of them on a real ledger. Rolled up per
    // issue, which keeps both signals at the granularity a reader acts on.
    if !quarantine.is_empty() {
        output::run_msg(&quarantine_rollup(quarantine, &report.per_entry));
    }
    outside.extend(declared_narrowing(sweeps, stats.curated));
    let outside_clause = if outside.is_empty() {
        String::new()
    } else {
        format!(" (outside the audit: {})", outside.join("; "))
    };

    let stale: Vec<&str> = if plan.complete {
        quarantine
            .iter()
            .zip(&report.per_entry)
            .filter(|(q, n)| q.pattern.is_some() && **n == 0)
            .map(|(q, _)| q.issue.as_str())
            .collect()
    } else {
        Vec::new()
    };
    let dead = &plan.dead_filters;

    let failing = !report.orphans.is_empty() || !stale.is_empty() || !dead.is_empty();
    if (failing || !plan.complete) && !outside.is_empty() {
        output::run_msg(&format!("coverage:{outside_clause}"));
    }
    if !plan.complete {
        output::error(&format!(
            "coverage: the plan is incomplete - {}. Policy is reported for what was prepared, and \
             cannot pass.",
            plan.incomplete.join("; ")
        ));
    }

    if !report.orphans.is_empty() {
        for orphan in &report.orphans {
            output::error(&format!("orphaned: {orphan} (selected nowhere, quarantined nowhere)"));
        }
        output::error(&format!(
            "{}: every unselected test needs a [[quarantine]] \
             entry with an issue, or a lane that selects it under this build shape",
            output::count(report.orphans.len(), "orphaned pair")
        ));
    }

    if !stale.is_empty() {
        output::error(&format!(
            "stale [[quarantine]] entries ({}): every matching pair is selected (or no \
             pair matches). The ledger must shrink when a suppression is \
             removed - delete the entries.",
            stale.join(", ")
        ));
    }

    for d in dead {
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

    if !failing && plan.complete {
        let curated_frag = if stats.curated > 0 {
            format!("{} curated, ", stats.curated)
        } else {
            String::new()
        };
        output::run_msg(&format!(
            "coverage: {} shapes, {} pairs - {} selected, {} quarantined, {} ignored, \
             {curated_frag}0 orphaned{outside_clause}",
            shapes.len(),
            stats.pairs,
            stats.selected,
            stats.quarantined,
            stats.ignored,
        ));
    }

    let status = if !plan.complete {
        "incomplete"
    } else if failing {
        "failed"
    } else {
        "passed"
    };
    (
        PolicyCoverage {
            status,
            plan_complete: plan.complete,
            pairs: stats.pairs,
            selected: stats.selected,
            ignored: stats.ignored,
            quarantined: stats.quarantined,
            curated: stats.curated,
            orphaned: stats.orphaned,
            dead_filters: dead.len(),
        },
        failing || !plan.complete,
    )
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
/// sweep it applies to still has no live sighting, and still reports. A filter
/// one of whose sweeps could not be prepared is unknown rather than dead: the
/// sweep nobody listed may be exactly where it matches.
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
    /// Some sweep it applies to was never listed.
    unknown: bool,
    /// Every sweep it was judged against, for the report - a dead filter's
    /// remedy depends on which lanes were even looking.
    sweeps: Vec<String>,
}

impl FilterLedger {
    fn tally(&mut self, filter: &DeclaredFilter, sweep: &str) -> &mut FilterTally {
        let label = filter.label();
        let pos = match self
            .tallies
            .iter()
            .position(|t| t.origin == filter.origin && t.label == label)
        {
            Some(p) => p,
            None => {
                self.tallies.push(FilterTally {
                    origin: filter.origin.clone(),
                    label,
                    live: false,
                    unknown: false,
                    sweeps: Vec::new(),
                });
                self.tallies.len() - 1
            }
        };
        let t = &mut self.tallies[pos];
        if !t.sweeps.iter().any(|s| s == sweep) {
            t.sweeps.push(sweep.to_owned());
        }
        t
    }

    fn record(&mut self, filter: &DeclaredFilter, live: bool, sweep: &str) {
        let t = self.tally(filter, sweep);
        t.live |= live;
    }

    /// The filter applies to a sweep that could not be listed.
    fn record_unknown(&mut self, filter: &DeclaredFilter, sweep: &str) {
        self.tally(filter, sweep).unknown = true;
    }

    fn dead(self) -> Vec<DeadFilter> {
        self.tallies
            .into_iter()
            .filter(|t| !t.live && !t.unknown)
            .map(|t| DeadFilter {
                sweeps: t.sweeps.join(", "),
                origin: t.origin,
                label: t.label,
            })
            .collect()
    }
}

/// The shape's bare cargo selection: packages/excludes + features, no
/// target filters (those are lane narrowing, audited via the selections).
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
/// Not optional: enumeration COMPILES (`cargo test --no-run`), and a lint the
/// project's `-Dwarnings` turns into an error kills the build before any
/// diagnostic exists to filter.
fn shape_enumeration_args(sweep: &ResolvedSweep, allow_args: Vec<String>) -> Vec<String> {
    let mut args = allow_args;
    args.extend(shape_selection_args(sweep));
    args
}

/// The enumeration selection for ONE cargo resolution of a shape.
///
/// `resolution` is `Some(pkg)` only under package mode, where the lane runs one
/// cargo command per package and the universe must be enumerated the same way:
/// a batched `-p a -p b` listing would catalogue binaries from a graph no run
/// produced. The package REPLACES the shape's own `-p` list rather than adding
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
    /// `shape-label/binary-id/test-name` for every unjustified non-selected
    /// pair.
    orphans: Vec<String>,
}

/// Pure pair classification: universe minus selected, partitioned into
/// curated / ignored / quarantined / orphaned per shape. A quarantine entry
/// with a `package` field justifies only that package's pairs - every binary
/// of it - and a name-only pattern written for one package must not absorb
/// same-named pairs in every other.
fn classify(shapes: &[ShapeCoverage], quarantine: &[QuarantineEntry]) -> CoverageReport {
    let mut stats = CoverageStats::default();
    let mut per_entry = vec![0usize; quarantine.len()];
    let mut orphans: Vec<String> = Vec::new();
    for shape in shapes {
        for pair in &shape.universe {
            let (unit, test) = pair;
            stats.pairs += 1;

            if shape.selected.contains(pair) {
                stats.selected += 1;
                continue;
            }
            // A curated shape's non-selected pairs are exempt by declaration -
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
            // entry crediting zero pairs and failing the stale check.
            let hit = quarantine
                .iter()
                .enumerate()
                .filter(|(_, q)| {
                    q.pattern.as_deref().is_some_and(|p| test.contains(p))
                        && q.package.as_deref().is_none_or(|pkg| pkg == unit.package)
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
                    // The binary id, not the package: it is the finer,
                    // actionable address (and it carries the package).
                    orphans.push(format!("{}/{}/{test}", shape.label, unit.id()));
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
        classify, filter_liveness, shape_enumeration_args, BinaryUnit, CoverageStats, DeclaredFilter,
        FilterKind, FilterLedger, QuarantineEntry, ResolvedSweep, ShapeCoverage,
    };
    use crate::config::QualifiedSkip;
    use std::collections::BTreeSet;

    #[test]
    fn enumeration_selection_carries_the_lint_allows() {
        // The universe's `cargo test --no-run` compiles, so it is in the test
        // phase's class: a lint the project's `-Dwarnings` turns into an error
        // kills the build before any diagnostic exists to filter.
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
    /// the ledger asked. Mirrors `prepare_profile`, which cannot decide inside
    /// its per-lane loop.
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
        // while doing its job in the unscoped sweep.
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

    /// A filter one of whose sweeps could not be prepared is unknown, not
    /// dead: the unlisted sweep may be exactly where it matches.
    #[test]
    fn a_filter_with_an_unlisted_sweep_is_not_reported_dead() {
        let skip = filter(FilterKind::Skip, "renamed_away");
        let listed = ResolvedSweep {
            label: "listed".into(),
            declared_filters: vec![skip.clone()],
            ..ResolvedSweep::default()
        };
        let cands = candidates(&[("core", "plain")]);
        let mut ledger = FilterLedger::default();
        for (f, live) in filter_liveness(&listed, &cands, &BTreeSet::new()) {
            ledger.record(f, live, &listed.label);
        }
        ledger.record_unknown(&skip, "unprepared");
        assert!(ledger.dead().is_empty());
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
        // The shape's universe carries `tape::budget`, but this lane narrows to
        // one target with `--test`, so the skip removes nothing HERE. Judged
        // against the wider universe it would read as alive.
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
        // would let the live sibling cover for the dead one.
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
        // nothing - the vanished gate.
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

        // Same shape via `#[ignore]` rather than a skip.
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

    fn unit(package: &str, kind: &str, target: &str) -> BinaryUnit {
        BinaryUnit {
            package_id: format!("path+file:///x/{package}#{package}@0.1.0"),
            package: package.into(),
            kind: kind.into(),
            target: target.into(),
        }
    }

    fn lib(package: &str) -> BinaryUnit {
        unit(package, "lib", package)
    }

    fn set(pairs: &[(BinaryUnit, &str)]) -> BTreeSet<(BinaryUnit, String)> {
        pairs.iter().map(|(u, t)| (u.clone(), (*t).to_owned())).collect()
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
        // The serial_tests:: hole: selected in the default shape's serial lane,
        // skipped in the ffi shape - name-level accounting would call it
        // covered, pair-level accounting reports the ffi pair.
        let core = lib("core");
        let shapes = vec![
            ShapeCoverage {
                curated: false,
                label: "tier1/default".into(),
                universe: set(&[(core.clone(), "serial_tests::a"), (core.clone(), "plain")]),
                ignored: set(&[]),
                selected: set(&[(core.clone(), "serial_tests::a"), (core.clone(), "plain")]),
            },
            ShapeCoverage {
                curated: false,
                label: "tier1/ffi".into(),
                universe: set(&[(core.clone(), "serial_tests::a"), (core.clone(), "plain")]),
                ignored: set(&[]),
                selected: set(&[(core, "plain")]),
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
        let core = lib("core");
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "default".into(),
            universe: set(&[(core.clone(), "a"), (core.clone(), "slow_manual")]),
            ignored: set(&[(core.clone(), "slow_manual")]),
            selected: set(&[(core, "a")]),
        }];
        let stats = stats_of(&shapes, &[]);
        assert_eq!(stats.ignored, 1);
        assert_eq!(stats.orphaned, 0);
        assert_eq!(stats.selected, 1);
    }

    #[test]
    fn most_specific_entry_gets_the_credit() {
        let core = lib("core");
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "default".into(),
            universe: set(&[(core, "test_bar_roundtrip")]),
            ignored: set(&[]),
            selected: set(&[]),
        }];
        // "roundtrip" (9) is longer than "test_bar" (8): credit index 1.
        let q = vec![entry("test_bar", "B50"), entry("roundtrip", "B99")];
        let report = classify(&shapes, &q);
        assert_eq!(report.per_entry, vec![0, 1]);
    }

    #[test]
    fn narrower_nested_pattern_is_not_starved() {
        // The S3-16 bug: a broad `test_bar` entry declared before a narrower
        // `test_bar_roundtrip` used to absorb the roundtrip pair.
        let core = lib("core");
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "default".into(),
            universe: set(&[(core.clone(), "test_bar_basic"), (core, "test_bar_roundtrip")]),
            ignored: set(&[]),
            selected: set(&[]),
        }];
        let q = vec![
            entry("test_bar", "B50"),
            entry("test_bar_roundtrip", "B51"),
        ];
        let report = classify(&shapes, &q);
        assert_eq!(report.per_entry, vec![1, 1]);
        assert_eq!(report.stats.orphaned, 0);
    }

    #[test]
    fn curated_shape_exempts_non_selected_pairs_without_touching_quarantine() {
        let live = lib("live");
        let shapes = vec![ShapeCoverage {
            curated: true,
            label: "sim/sim-live".into(),
            universe: set(&[(live.clone(), "targeted"), (live.clone(), "rest_a"), (live.clone(), "rest_b")]),
            ignored: set(&[]),
            selected: set(&[(live, "targeted")]),
        }];
        let q = vec![entry("rest_", "B60")];
        let report = classify(&shapes, &q);
        assert_eq!(report.stats.selected, 1);
        assert_eq!(report.stats.curated, 2);
        assert_eq!(report.stats.orphaned, 0);
        assert_eq!(report.per_entry, vec![0]);
    }

    #[test]
    fn non_curated_shape_gets_no_exemption() {
        let common = lib("common");
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "sim/sim-common".into(),
            universe: set(&[(common.clone(), "a"), (common.clone(), "b")]),
            ignored: set(&[]),
            selected: set(&[(common, "a")]),
        }];
        let report = classify(&shapes, &[]);
        assert_eq!(report.stats.curated, 0);
        assert_eq!(report.stats.orphaned, 1);
        assert_eq!(report.orphans, vec!["sim/sim-common/common/b"]);
    }

    // The B51 collision shape, measured on nautilus-infrastructure: two
    // binaries in one package define the same test path. Two pairs, and one
    // package-scoped quarantine entry spans both.
    #[test]
    fn a_test_path_shared_by_two_binaries_is_two_pairs_one_entry() {
        let redis = unit("nautilus-infrastructure", "test", "test_cache_redis");
        let postgres = unit("nautilus-infrastructure", "test", "test_cache_postgres");
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "serial/default".into(),
            universe: set(&[(redis, "serial_tests::t"), (postgres, "serial_tests::t")]),
            ignored: set(&[]),
            selected: set(&[]),
        }];
        let mut scoped = entry("serial_tests::", "B51");
        scoped.package = Some("nautilus-infrastructure".into());
        let report = classify(&shapes, &[scoped]);
        assert_eq!(report.stats.pairs, 2);
        assert_eq!(report.per_entry, vec![2]);
        assert_eq!(report.stats.orphaned, 0);
    }

    #[test]
    fn package_scoped_entry_does_not_absorb_other_packages() {
        // A pattern written for infrastructure must not justify a same-named
        // pair in backtest, or a test that later stops running lands as
        // accounted instead of orphaned. The scope compares the package, never
        // the whole binary id.
        let shapes = vec![ShapeCoverage {
            curated: false,
            label: "serial/default".into(),
            universe: set(&[
                (unit("nautilus-infrastructure", "test", "test_cache_redis"), "serial_tests::t"),
                (unit("nautilus-backtest", "test", "regress"), "serial_tests::t"),
            ]),
            ignored: set(&[]),
            selected: set(&[]),
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
}
