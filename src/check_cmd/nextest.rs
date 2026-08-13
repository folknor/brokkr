// The nextest lane's coverage accounting: what a listing means, and which
// non-run pairs the ledger is allowed to accept.
//
// brokkr links nextest (see Cargo.toml) and reads `list::TestList` directly,
// so nothing here parses JSON. What it does own is the mapping from nextest's
// per-testcase verdict to the ledger buckets `coverage` already speaks in
// (run / ignored / quarantined / orphaned), and that mapping is a policy
// decision rather than a transcription - see `Disposition`.
//
// The enumeration-is-ground-truth rule from `coverage` is unchanged and is the
// reason this is a thin classifier: brokkr does not re-derive which tests a
// lane selects, it asks nextest and reads the answer. The one predicate brokkr
// evaluated itself (`QualifiedSkip`, because libtest has no package scoping)
// becomes `package(X) & test(~Y)` and stops being ours.

// THE UNIVERSE MUST COME FROM ITS OWN LISTING, never from a lane's.
//
// It looks like it comes free: a listing under a test-only filterset carries
// EVERY testcase, marking the unselected ones `mismatch/expression`, so one
// listing per lane appears to yield both the lane's selection and the shape's
// universe. It does, right up until a lane's expression scopes by package -
// and `package(X) & test(~Y)` is precisely what `QualifiedSkip` ports into.
//
// Measured against cargo-nextest 0.9.143 on a two-package workspace:
// `-E 'test(~shared_name)'` lists all 7 testcases (`test-count: 7`), while
// `-E 'package(pkg-a) & test(~shared_name)'` reports pkg-b's suite as
// `status: "skipped"` with an EMPTY testcase map and `test-count: 4`. The
// universe silently shrinks by a whole binary, and every pair in it would read
// as accounted-for rather than orphaned.
//
// So: one unfiltered listing per build shape establishes the universe, and lane
// listings are only ever read for their selections.
//
// THAT LISTING MUST PASS `--ignore-default-filter`, and `-E 'all()'` is NOT a
// substitute. Measured on the same workspace: a profile carrying
// `default-filter = "not test(~shared_name)"` lists identically with and without
// `-E 'all()'` - the default filter COMPOSES with the expression rather than
// being overridden by it. Only `--ignore-default-filter` reveals the excluded
// tests.
//
// The reason it matters is that the default filter bites at two levels, and
// only one of them is visible:
//
//   per-test    `shared_name` -> `mismatch/default-filter`, test-count
//               unchanged. Marked, attributable, universe intact.
//   per-binary  the whole suite -> `status: "skipped-default-filter"` with an
//               EMPTY testcase map and a reduced test-count. The pairs are
//               simply gone.
//
// Without the flag the universe silently loses a whole binary to the project's
// nextest config, and the ledger reports every pair in it as accounted for.
// Attribution still comes from the LANE listings, which keep the per-test
// `reason` and the suite status - so this costs no extra listing, only the flag.

use nextest_metadata::{FilterMatch, MismatchReason, RustBinaryId, TestCaseName};

/// The coverage key for a nextest lane: one test in one test binary.
///
/// **Finer than the libtest path's `(package, test)`**, deliberately. A package
/// with several test targets has one `RustBinaryId` per target, so two
/// integration binaries that both define `serial_tests::test_x` are two pairs
/// here and one pair under the libtest path. That is not a cosmetic
/// refinement: nautilus' B51 entry covers five binary ids inside
/// `nautilus-infrastructure` (four `tests/` binaries plus the lib), which is
/// exactly the shape where the coarser key merges pairs silently. Package-
/// qualified `[[quarantine]]` and skip entries keep their meaning across the
/// change - `package(X)` spans every binary id in X - but per-entry pair counts
/// shift once, upward, on adoption.
// NOT YET WIRED: no `[[check]]` entry can select the nextest harness, so
// nothing constructs this. The classifier and its key land ahead of the lane
// deliberately - they encode the three measured findings the lane depends on,
// and the coverage pair is the audit's key type, so it has to be right before
// there is code shaped around it rather than migrated after.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NextestPair {
    pub(crate) binary_id: RustBinaryId,
    pub(crate) test: TestCaseName,
}

/// What the ledger does with one testcase under one lane's listing.
///
/// Only [`Disposition::Selected`] and [`Disposition::Ignored`] are terminal on
/// their own; everything else has to be justified by another lane running the
/// pair, or by a `[[quarantine]]` entry, or it orphans.
#[allow(dead_code)] // Not yet wired - see NextestPair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// The lane runs this pair.
    Selected,
    /// `#[ignore]` at the source and the lane does not lift it. Counted and
    /// reported, never fatal - the same lane policy the libtest path applies.
    Ignored,
    /// The lane's filterset does not select it. An orphan unless some other
    /// lane runs it or a quarantine entry justifies it.
    Unmatched,
    /// Excluded by the nextest profile's `default-filter` rather than by
    /// anything brokkr wrote.
    ///
    /// NON-FATAL AND COUNTED, a third bucket of the same kind as `ignored` and
    /// `quarantined` - not an orphan. CI does not run these either, so failing
    /// on them would make the local gate stricter than the CI it exists to
    /// predict, and the only way to clear the failure would be a quarantine
    /// entry for a test nobody intended to run. That inverts the fidelity goal
    /// the whole port is for.
    ///
    /// Kept distinct from [`Disposition::Unmatched`] because the remedy lives
    /// in a different file: a default-filter exclusion is edited in the
    /// project's `.config/nextest.toml`, not in brokkr.toml, and a report that
    /// does not say so sends the reader to the wrong place.
    ///
    /// What this bucket does NOT do on its own is stop upstream shrinking the
    /// audited set silently - a drifting count is not something anyone tracks
    /// across runs. That needs the resolved default-filter pinned and diffed,
    /// so a change reports "default-filter changed from X to Y, N pairs moved
    /// out of the audited set" once, for a decision. Not implemented here, but
    /// the design is closed and needs no private API.
    ///
    /// The compiled expression cannot be the pin: `CompiledExpr` is an AST with
    /// no `Display` or `Serialize`, and the raw source accessor is private to
    /// nextest-runner's config module. Pin `(profile, section, raw string)`
    /// instead. `CompiledDefaultFilter` exposes `profile` and `section`
    /// publicly, and `CompiledDefaultFilterSection` is `Profile` or
    /// `Override(usize)` - the INDEX of the override that won. That is a config
    /// address, so the raw string is read from `profile.<name>.default-filter`
    /// or from override #N of `profile.<name>.overrides`, rather than assumed
    /// to be the top-level one. No override-resolution hole survives: editing
    /// an override that did not win cannot move the pin, because the pin
    /// records which one did.
    DefaultFiltered,
    /// A reason the ledger has no policy for: benchmark-mode filtering,
    /// partitioning, rerun-already-passed, string filters, or a variant added
    /// to nextest's `#[non_exhaustive]` enum after this was written. Refused
    /// rather than bucketed - see `classify`.
    Unclassified,
}

impl Disposition {
    /// Does this disposition settle the pair without needing another lane or a
    /// quarantine entry?
    #[allow(dead_code)] // Not yet wired - see NextestPair.
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Disposition::Selected | Disposition::Ignored)
    }
}

/// Where a resolved `default-filter` came from in the project's nextest
/// config - the address half of the pin described on
/// [`Disposition::DefaultFiltered`].
///
/// Mirrors nextest's own `CompiledDefaultFilterSection`, which is what supplies
/// these values: `Profile` for a top-level `profile.<name>.default-filter`, and
/// `Override(usize)` for the winning entry of `profile.<name>.overrides`.
/// Reproduced rather than reused because the pin is *stored* - it has to
/// survive across runs and nextest upgrades as plain data, and a foreign enum
/// with no `Serialize` cannot.
#[allow(dead_code)] // Not yet wired - see NextestPair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FilterAddress {
    /// `profile.<profile>.default-filter`.
    Profile { profile: String },
    /// `profile.<profile>.overrides[<index>].default-filter`.
    Override { profile: String, index: usize },
}

impl FilterAddress {
    /// The address as a config path, for a report that has to tell the reader
    /// which line to go and look at.
    #[allow(dead_code)] // Not yet wired - see NextestPair.
    pub(crate) fn config_path(&self) -> String {
        match self {
            FilterAddress::Profile { profile } => {
                format!("profile.{profile}.default-filter")
            }
            FilterAddress::Override { profile, index } => {
                format!("profile.{profile}.overrides[{index}].default-filter")
            }
        }
    }
}

/// Read the raw `default-filter` string at `address` out of a nextest config.
///
/// The pin's value half. Deliberately reads the SOURCE text rather than
/// rendering the compiled expression: the compiled form cannot be stored
/// (`CompiledExpr` has no `Display`/`Serialize`, and nextest's raw accessor is
/// private), and the source text is what a human diffs and edits anyway.
///
/// `Ok(None)` means the address exists but carries no `default-filter`, which
/// is a legitimate state - not every profile or override sets one. A missing
/// profile, or an override index past the end of the list, is an error: the
/// address came from nextest's own resolution, so it not being there means the
/// config moved under us and the pin is meaningless rather than absent.
#[allow(dead_code)] // Not yet wired - see NextestPair.
pub(crate) fn read_default_filter(
    config: &str,
    address: &FilterAddress,
) -> Result<Option<String>, DevError> {
    let doc: toml::Value = toml::from_str(config)
        .map_err(|e| DevError::Config(format!("nextest config: {e}")))?;

    let profile_name = match address {
        FilterAddress::Profile { profile } | FilterAddress::Override { profile, .. } => profile,
    };
    let profile = doc
        .get("profile")
        .and_then(|p| p.get(profile_name))
        .ok_or_else(|| {
            DevError::Config(format!(
                "nextest config has no profile.{profile_name} (pin address: {})",
                address.config_path()
            ))
        })?;

    let table = match address {
        FilterAddress::Profile { .. } => profile,
        FilterAddress::Override { index, .. } => profile
            .get("overrides")
            .and_then(|o| o.as_array())
            .and_then(|o| o.get(*index))
            .ok_or_else(|| {
                DevError::Config(format!(
                    "nextest config has no {} - the overrides list moved",
                    address.config_path()
                ))
            })?,
    };

    match table.get("default-filter") {
        None => Ok(None),
        Some(v) => v.as_str().map(|s| Some(s.to_owned())).ok_or_else(|| {
            DevError::Config(format!("{} is not a string", address.config_path()))
        }),
    }
}

/// Classify one testcase's filter verdict.
///
/// TWO POLICY NOTES, both load-bearing, both measured against cargo-nextest
/// 0.9.143 rather than read off the docs:
///
/// 1. `MismatchReason` is PRIORITY-ORDERED, not a partition. A test that is
///    both `#[ignore]`d and unmatched by the lane's filterset reports
///    `Ignored`, never `Expression` - so on a single listing it lands in the
///    legitimate terminal bucket and CANNOT be detected as an orphan. The
///    libtest path has the same effective policy (ignored is credited before
///    orphaning), so this is not a regression, but it stops being brokkr's
///    ordering to change.
///
///    DECIDED: one listing, inheriting nextest's precedence. A second listing
///    under `--run-ignored all` would report that same test as `Expression` and
///    hand the precedence back, but the only thing it buys is detecting a test
///    that is both `#[ignore]`d and matched by no lane - and there is nothing to
///    do with that. The remedy for an orphan is "cover it or quarantine it";
///    an ignored test needs neither, and its own attribute says so in-tree.
///
///    What settles it is that the single-listing scheme self-heals exactly when
///    the information starts to matter: delete the `#[ignore]` and the verdict
///    flips from `Ignored` to `Expression`, the pair orphans, and the gate
///    fails. nextest's precedence hides the condition only while it is
///    harmless.
///
///    Residual, stated: someone can `#[ignore]` a test to get it out of the
///    audit undetectably. That is true of the libtest path today (ignored is
///    credited before orphaning), so it is not a regression, and it is a
///    source-visible act rather than a config one.
///
/// 2. Unknown reasons are [`Disposition::Unclassified`] rather than folded into
///    the nearest bucket. `MismatchReason` is `#[non_exhaustive]`, and the
///    failure mode of guessing is a pair silently accounted as justified. A
///    ledger that cannot say why a test did not run has not audited it.
///
/// Feeds `coverage`'s `classify`, which owns the pair-level ledger; this only
/// says what one testcase's verdict means.
// Named for the lane rather than the action: `include!` puts every check_cmd
// file in one namespace, and `coverage`'s own `classify` is the pair-level
// ledger classifier this feeds.
#[allow(dead_code)] // Not yet wired - see NextestPair.
pub(crate) fn nextest_disposition(filter_match: &FilterMatch) -> Disposition {
    match filter_match {
        FilterMatch::Matches => Disposition::Selected,
        FilterMatch::Mismatch { reason } => match reason {
            MismatchReason::Ignored => Disposition::Ignored,
            MismatchReason::Expression => Disposition::Unmatched,
            MismatchReason::DefaultFilter => Disposition::DefaultFiltered,
            MismatchReason::NotBenchmark
            | MismatchReason::String
            | MismatchReason::Partition
            | MismatchReason::RerunAlreadyPassed => Disposition::Unclassified,
            _ => Disposition::Unclassified,
        },
    }
}

#[cfg(test)]
mod nextest_pin_tests {
    use super::*;

    // The shape the probe workspace used, plus an overrides list so both
    // address forms are exercised against one document.
    const CONFIG: &str = r#"
[profile.default]
default-filter = "not test(~shared_name)"

[profile.ci]
retries = 2

[[profile.ci.overrides]]
platform = "aarch64-apple-darwin"
default-filter = "not package(slow-crate)"

[[profile.ci.overrides]]
filter = "test(~flaky)"
retries = 5
"#;

    fn profile(name: &str) -> FilterAddress {
        FilterAddress::Profile {
            profile: name.to_owned(),
        }
    }

    fn override_at(name: &str, index: usize) -> FilterAddress {
        FilterAddress::Override {
            profile: name.to_owned(),
            index,
        }
    }

    #[test]
    fn reads_the_profile_level_filter() {
        let got = read_default_filter(CONFIG, &profile("default"));
        assert_eq!(got.ok().flatten().as_deref(), Some("not test(~shared_name)"));
    }

    // The whole point of carrying the override INDEX: override 0 sets a
    // default-filter and override 1 does not, so an address that ignored the
    // index would read the wrong entry (or the profile's, which is absent here).
    #[test]
    fn reads_the_winning_override_not_the_first_or_the_profile() {
        let got = read_default_filter(CONFIG, &override_at("ci", 0));
        assert_eq!(
            got.ok().flatten().as_deref(),
            Some("not package(slow-crate)")
        );

        // Override 1 exists but sets no filter - a legitimate None, not an error.
        let got = read_default_filter(CONFIG, &override_at("ci", 1));
        assert!(matches!(got, Ok(None)));

        // And profile.ci itself carries none, so a Profile address there is
        // None rather than override 0's value leaking upward.
        assert!(matches!(read_default_filter(CONFIG, &profile("ci")), Ok(None)));
    }

    // An address comes from nextest's own resolution, so if it is not in the
    // config the config moved underneath the pin - that is an error, and
    // distinguishable from "present but unset".
    #[test]
    fn a_moved_address_errors_rather_than_reading_as_absent() {
        assert!(read_default_filter(CONFIG, &profile("nonexistent")).is_err());
        assert!(read_default_filter(CONFIG, &override_at("ci", 7)).is_err());
    }

    #[test]
    fn config_paths_name_the_line_to_edit() {
        assert_eq!(profile("default").config_path(), "profile.default.default-filter");
        assert_eq!(
            override_at("ci", 3).config_path(),
            "profile.ci.overrides[3].default-filter"
        );
    }

    #[test]
    fn malformed_config_is_an_error() {
        assert!(read_default_filter("not = = toml", &profile("default")).is_err());
    }
}

#[cfg(test)]
mod nextest_classify_tests {
    use super::*;

    fn mismatch(reason: MismatchReason) -> FilterMatch {
        FilterMatch::Mismatch { reason }
    }

    #[test]
    fn matches_is_selected_and_terminal() {
        assert_eq!(nextest_disposition(&FilterMatch::Matches), Disposition::Selected);
        assert!(Disposition::Selected.is_terminal());
    }

    #[test]
    fn ignored_is_terminal_but_unmatched_is_not() {
        assert_eq!(
            nextest_disposition(&mismatch(MismatchReason::Ignored)),
            Disposition::Ignored
        );
        assert!(Disposition::Ignored.is_terminal());

        assert_eq!(
            nextest_disposition(&mismatch(MismatchReason::Expression)),
            Disposition::Unmatched
        );
        assert!(!Disposition::Unmatched.is_terminal());
    }

    // A default-filter exclusion is still an unrun pair, but it is the
    // project's nextest config that dropped it - the ledger keeps the
    // distinction so the orphan report names the right file.
    #[test]
    fn default_filter_is_distinct_from_expression_and_not_terminal() {
        let d = nextest_disposition(&mismatch(MismatchReason::DefaultFilter));
        assert_eq!(d, Disposition::DefaultFiltered);
        assert_ne!(d, Disposition::Unmatched);
        assert!(!d.is_terminal());
    }

    // The bucket that must never be silently absorbed: an unrun pair whose
    // reason the ledger has no policy for is not an audited pair.
    #[test]
    fn reasons_without_a_policy_are_unclassified_and_not_terminal() {
        for reason in [
            MismatchReason::NotBenchmark,
            MismatchReason::String,
            MismatchReason::Partition,
            MismatchReason::RerunAlreadyPassed,
        ] {
            let d = nextest_disposition(&mismatch(reason));
            assert_eq!(d, Disposition::Unclassified, "reason {reason:?}");
            assert!(!d.is_terminal(), "reason {reason:?}");
        }
    }
}
