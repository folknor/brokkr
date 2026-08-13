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
    /// out of the audited set" once, for a decision. Not implemented here, and
    /// the obvious route is closed: `CompiledDefaultFilter` exposes `profile`
    /// and `section` but its `expr` is a `CompiledExpr` AST with no `Display`
    /// or `Serialize`, and the raw source accessor is private to
    /// nextest-runner's config module. So the pin has to come from reading the
    /// project's `.config/nextest.toml` directly, which is diffable and names
    /// the file to edit but does not capture which platform override won.
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
