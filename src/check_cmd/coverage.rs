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

/// One build shape's enumeration: the full universe, the `#[ignore]`d
/// subset, and the union of every lane's ran-set.
struct ShapeCoverage {
    label: String,
    universe: BTreeSet<String>,
    ignored: BTreeSet<String>,
    ran: BTreeSet<String>,
}

fn run_coverage_phase(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    quarantine: &[QuarantineEntry],
    limit: usize,
    all: bool,
    commands: bool,
) -> Result<CoverageStats, DevError> {
    let shapes = enumerate_shapes(project_root, sweeps, commands)?;
    let report = classify(&shapes, quarantine);

    // Per-entry pair counts, always printed: the countdown the ledger
    // exists for, and the growth signal when a substring starts matching
    // more than it used to.
    for (entry, count) in quarantine.iter().zip(&report.per_entry) {
        match (&entry.pattern, &entry.category) {
            (Some(p), _) => {
                output::run_msg(&format!("quarantine {} ({p}): {count} pairs", entry.issue));
            }
            (None, Some(cat)) => {
                output::run_msg(&format!("quarantine {} (category {cat})", entry.issue));
            }
            (None, None) => {}
        }
    }
    // Package-level exclusion is outside the pair audit (the binaries
    // cannot even build); say so rather than hide it.
    for sweep in sweeps {
        if !sweep.test_exclude_packages.is_empty() {
            output::run_msg(&format!(
                "coverage: sweep {} excludes {} package(s) from tests - outside \
                 the pair audit",
                sweep.label,
                sweep.test_exclude_packages.len()
            ));
        }
    }

    let stale: Vec<&str> = quarantine
        .iter()
        .zip(&report.per_entry)
        .filter(|(q, n)| q.pattern.is_some() && **n == 0)
        .map(|(q, _)| q.issue.as_str())
        .collect();

    if !stale.is_empty() {
        output::error(&format!(
            "stale [[quarantine]] entries ({}): every matching pair runs (or no \
             pair matches). The ledger must shrink when a suppression is \
             removed - delete the entries.",
            stale.join(", ")
        ));
        return Err(DevError::Build("coverage failed".into()));
    }

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
        return Err(DevError::Build("coverage failed".into()));
    }

    let stats = report.stats;
    output::run_msg(&format!(
        "coverage: {} shapes, {} pairs - {} run, {} quarantined, {} ignored, 0 orphaned",
        shapes.len(),
        stats.pairs,
        stats.run,
        stats.quarantined,
        stats.ignored
    ));
    Ok(stats)
}

/// Group active sweeps by build shape and enumerate each shape once:
/// universe (`--include-ignored`, no filters), plain listing (to derive
/// the ignored set), and every lane's filtered ran-set.
fn enumerate_shapes(
    project_root: &Path,
    sweeps: &[ResolvedSweep],
    commands: bool,
) -> Result<Vec<ShapeCoverage>, DevError> {
    let mut order: Vec<profile::BuildShapeKey> = Vec::new();
    let mut groups: HashMap<profile::BuildShapeKey, Vec<&ResolvedSweep>> = HashMap::new();
    for sweep in sweeps {
        let key = sweep.build_shape_key();
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(sweep);
    }

    let mut out = Vec::with_capacity(order.len());
    for key in &order {
        let members = &groups[key];
        let first = members[0];
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
        // libtest's `--list` includes `#[ignore]`d tests regardless of
        // `--include-ignored` (verified empirically), so the ignored set
        // comes from `--list --ignored`, which lists ONLY ignored tests.
        let universe: BTreeSet<String> = coverage_list(
            project_root,
            &bare,
            &[],
            &["--include-ignored"],
            &env_refs,
            commands,
        )?
        .into_iter()
        .collect();
        let ignored: BTreeSet<String> =
            coverage_list(project_root, &bare, &[], &["--ignored"], &env_refs, commands)?
                .into_iter()
                .collect();

        let mut ran: BTreeSet<String> = BTreeSet::new();
        for sweep in members {
            let mut libtest: Vec<&str> = sweep.name_filters.iter().map(String::as_str).collect();
            libtest.extend(sweep.libtest_args.iter().map(String::as_str));
            let lane_ran = coverage_list(
                project_root,
                &bare,
                &sweep.cargo_test_filters,
                &libtest,
                &env_refs,
                commands,
            )?;
            // A lane without `--include-ignored` lists ignored names it
            // will never execute; subtract them or the lane claims
            // coverage it does not provide.
            if sweep.libtest_args.iter().any(|a| a == "--include-ignored") {
                ran.extend(lane_ran);
            } else {
                ran.extend(lane_ran.into_iter().filter(|t| !ignored.contains(t)));
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

/// One `cargo test … -- … --list` invocation, parsed into test names.
fn coverage_list(
    project_root: &Path,
    selection: &[String],
    cargo_filters: &[String],
    libtest_args: &[&str],
    env_refs: &[(&str, &str)],
    commands: bool,
) -> Result<Vec<String>, DevError> {
    let mut args: Vec<String> = vec!["test".into()];
    args.extend(selection.iter().cloned());
    args.extend(cargo_filters.iter().cloned());
    args.push("--tests".into());
    args.push("--".into());
    args.extend(libtest_args.iter().map(|s| (*s).to_owned()));
    args.push("--list".into());

    if commands {
        output::run_msg(&format!("cargo {}", args.join(" ")));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let captured = output::run_captured_with_env("cargo", &arg_refs, project_root, env_refs)?;

    if !captured.status.success() {
        output::error(&format!("failing command: cargo {}", args.join(" ")));
        output::error(&String::from_utf8_lossy(&captured.stderr));
        return Err(DevError::Build("coverage enumeration failed".into()));
    }
    Ok(parse_list_output(&String::from_utf8_lossy(&captured.stdout)))
}

struct CoverageReport {
    stats: CoverageStats,
    /// Pair count justified per `[[quarantine]]` entry, index-aligned.
    per_entry: Vec<usize>,
    /// `shape-label/test-name` for every unjustified non-run pair.
    orphans: Vec<String>,
}

/// Pure pair classification: universe minus ran, partitioned into
/// ignored / quarantined / orphaned per shape.
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
        for test in &shape.universe {
            stats.pairs += 1;

            if shape.ran.contains(test) {
                stats.run += 1;
                continue;
            }

            if shape.ignored.contains(test) {
                stats.ignored += 1;
                continue;
            }
            let hit = quarantine
                .iter()
                .position(|q| q.pattern.as_deref().is_some_and(|p| test.contains(p)));
            match hit {
                Some(i) => {
                    per_entry[i] += 1;
                    stats.quarantined += 1;
                }
                None => {
                    stats.orphaned += 1;
                    orphans.push(format!("{}/{test}", shape.label));
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

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn entry(pattern: &str, issue: &str) -> QuarantineEntry {
        QuarantineEntry {
            pattern: Some(pattern.into()),
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
                universe: set(&["serial_tests::a", "plain"]),
                ignored: set(&[]),
                ran: set(&["serial_tests::a", "plain"]),
            },
            ShapeCoverage {
                label: "tier1/ffi".into(),
                universe: set(&["serial_tests::a", "plain"]),
                ignored: set(&[]),
                ran: set(&["plain"]),
            },
        ];
        let report = classify(&shapes, &[]);
        assert_eq!(report.stats.orphaned, 1);
        assert_eq!(report.orphans, vec!["tier1/ffi/serial_tests::a"]);

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
            universe: set(&["a", "slow_manual"]),
            ignored: set(&["slow_manual"]),
            ran: set(&["a"]),
        }];
        let stats = stats_of(&shapes, &[]);
        assert_eq!(stats.ignored, 1);
        assert_eq!(stats.orphaned, 0);
        assert_eq!(stats.run, 1);
    }

    #[test]
    fn first_matching_entry_gets_the_credit() {
        let shapes = vec![ShapeCoverage {
            label: "default".into(),
            universe: set(&["test_bar_roundtrip"]),
            ignored: set(&[]),
            ran: set(&[]),
        }];
        let q = vec![entry("test_bar", "B50"), entry("roundtrip", "B99")];
        let report = classify(&shapes, &q);
        assert_eq!(report.per_entry, vec![1, 0]);
    }
}
