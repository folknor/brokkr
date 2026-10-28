//! Verify: all - run all verify commands sequentially.

use std::path::Path;
use std::time::Instant;

use super::verify::{CheckStatus, ENVIRONMENT_EXIT_CODE, VerifyHarness, labels, run_check_status};
use super::{
    verify_add_locations, verify_cat, verify_check_refs, verify_derive_changes, verify_diff,
    verify_extract, verify_getid_removeid, verify_merge, verify_multi_extract, verify_renumber,
    verify_sort, verify_tags_filter,
};
use crate::datasets::Bbox;
use crate::error::DevError;
use crate::output::verify_summary;

/// The suite's tally. Environment outcomes are their own column: a check a
/// reference tool could not finish is neither a pass nor a pbfhogg failure.
#[derive(Default)]
struct Counts {
    passed: u32,
    failed: u32,
    environment: u32,
}

/// Tally a check's outcome. Kept a free function (not a closure) so it
/// doesn't hold a mutable borrow of the counters across the suite - the
/// final banner reads them directly.
///
/// An interrupted check is not counted: it is returned, so the suite stops
/// there and `main` runs the interrupt cleanup. Every later check would only
/// have been interrupted at its first child.
fn tally(counts: &mut Counts, result: Result<CheckStatus, DevError>) -> Result<(), DevError> {
    match result? {
        CheckStatus::Passed => counts.passed += 1,
        CheckStatus::Failed => counts.failed += 1,
        CheckStatus::Environment => counts.environment += 1,
    }
    Ok(())
}

/// The suite's exit: any pbfhogg failure is 1, else any environment outcome
/// is [`ENVIRONMENT_EXIT_CODE`], else success.
fn suite_result(counts: &Counts) -> Result<(), DevError> {
    if counts.failed > 0 {
        Err(DevError::ExitCode(1))
    } else if counts.environment > 0 {
        Err(DevError::ExitCode(ENVIRONMENT_EXIT_CODE))
    } else {
        Ok(())
    }
}

/// Why the OSC-consuming checks skip. `osc` is `None` only when nothing is
/// configured - a configured OSC that fails to resolve is an error in the
/// caller (`cmd::verify`), never a skip.
const NO_OSC: &str = "SKIPPED (dataset has no osc configured; pass --osc-seq to pick one)";
/// Why the bbox-consuming checks skip; same contract as [`NO_OSC`].
const NO_BBOX: &str = "SKIPPED (no --bbox and the dataset has no bbox configured)";

/// Result-line name of the `apply-changes --locations-on-ways` merge, which
/// runs on its own base and so is a check of its own.
const APPLY_CHANGES_LOCATIONS: &str = "apply-changes -l";

/// The base of the suite's `--locations-on-ways` merge: a LocationsOnWays
/// variant that keeps its untagged nodes, registered under
/// [`KEEP_UNTAGGED_VARIANT`]. A variant that is registered but fails to
/// resolve is an error in the caller, never a skip.
pub enum LocationsBase {
    Resolved(std::path::PathBuf),
    Skipped(&'static str),
}

/// The variant name `verify all` looks for. A plain `altw` variant drops
/// untagged nodes, so `-l` cannot resolve every ref on it.
pub const KEEP_UNTAGGED_VARIANT: &str = "altw-keep-untagged";

/// Run all verify commands sequentially.
///
/// Each check runs under [`run_check_status`], so a failure is reported (with
/// its detail replayed unless `verbose`) but does not stop the remaining
/// checks. Passing checks print a single line; failures replay their captured
/// detail. Returns `Err(ExitCode(1))` when one or more checks failed, else
/// `Err(ExitCode(ENVIRONMENT_EXIT_CODE))` when one or more could not be
/// completed for environmental reasons - the per-check lines and the banner
/// have already reported everything. A shutdown request
/// stops the suite at the interrupted check and returns `Err(Interrupted)`,
/// with no banner.
///
/// `osc`/`bbox` are `None` only when the input is not configured at all; the
/// checks needing them then print a SKIPPED line and count as skipped.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    osc: Option<&Path>,
    bbox: Option<&Bbox>,
    locations_base: &LocationsBase,
    data_dir: &Path,
    direct_io: bool,
    dataset: &str,
    verbose: bool,
) -> Result<(), DevError> {
    let suite_start = Instant::now();
    let mut counts = Counts::default();
    let mut skipped: u32 = 0;

    // 1. sort
    tally(
        &mut counts,
        run_check_status(labels::SORT, verbose, || {
            verify_sort::run(harness, pbf, direct_io)
        }),
    )?;

    // 2. cat
    tally(
        &mut counts,
        run_check_status(labels::CAT, verbose, || {
            verify_cat::run(harness, pbf, direct_io)
        }),
    )?;

    // 3. extract
    if let Some(b) = bbox {
        tally(
            &mut counts,
            run_check_status(labels::EXTRACT, verbose, || {
                verify_extract::run(harness, pbf, b, direct_io)
            }),
        )?;
    } else {
        verify_summary(&format!("extract: {NO_BBOX}"));
        skipped += 1;
    }

    // 3b. multi-extract
    if let Some(b) = bbox {
        tally(
            &mut counts,
            run_check_status(labels::MULTI_EXTRACT, verbose, || {
                verify_multi_extract::run(
                    harness,
                    pbf,
                    b,
                    super::commands::DEFAULT_MULTI_EXTRACT_REGIONS,
                    direct_io,
                )
            }),
        )?;
    } else {
        verify_summary(&format!("multi-extract: {NO_BBOX}"));
        skipped += 1;
    }

    // 4. tags-filter
    tally(
        &mut counts,
        run_check_status(labels::TAGS_FILTER, verbose, || {
            verify_tags_filter::run(harness, pbf, direct_io)
        }),
    )?;

    // 5. getid-removeid
    tally(
        &mut counts,
        run_check_status(labels::GETID_REMOVEID, verbose, || {
            verify_getid_removeid::run(harness, pbf, direct_io)
        }),
    )?;

    // 6. add-locations-to-ways
    tally(
        &mut counts,
        run_check_status(labels::ADD_LOCATIONS_TO_WAYS, verbose, || {
            verify_add_locations::run(harness, pbf, crate::cli::AltwMode::All, direct_io)
        }),
    )?;

    // 7. check-refs
    tally(
        &mut counts,
        run_check_status(labels::CHECK_REFS, verbose, || {
            verify_check_refs::run(harness, pbf, direct_io)
        }),
    )?;

    // 8. apply-changes
    if let Some(osc_path) = osc {
        // Best-effort osmosis setup - merge works without it. Done outside
        // run_check so any setup output isn't captured as check detail, and
        // narrated on failure the way single `verify merge` does, rather than
        // dropped: a silently missing reference tool reads as a covered one.
        let osmosis = match crate::tools::ensure_osmosis(data_dir) {
            Ok(tools) => Some(tools),
            Err(DevError::Interrupted) => return Err(DevError::Interrupted),
            Err(e) => {
                verify_summary(&format!("osmosis not available (non-fatal): {e}"));
                None
            }
        };
        tally(
            &mut counts,
            run_check_status(labels::APPLY_CHANGES, verbose, || {
                verify_merge::run(harness, pbf, osc_path, osmosis.as_ref(), direct_io, false)
            }),
        )?;
    } else {
        verify_summary(&format!("apply-changes: {NO_OSC}"));
        skipped += 1;
    }

    // 8b. apply-changes --locations-on-ways, on the keep-untagged base with
    // the same OSC (see verify_merge for the oracle).
    match (osc, locations_base) {
        (Some(osc_path), LocationsBase::Resolved(base)) => tally(
            &mut counts,
            run_check_status(APPLY_CHANGES_LOCATIONS, verbose, || {
                verify_merge::run(harness, base, osc_path, None, direct_io, true)
            }),
        )?,
        (None, _) => {
            verify_summary(&format!("{APPLY_CHANGES_LOCATIONS}: {NO_OSC}"));
            skipped += 1;
        }
        (Some(_), LocationsBase::Skipped(why)) => {
            verify_summary(&format!("{APPLY_CHANGES_LOCATIONS}: {why}"));
            skipped += 1;
        }
    }

    // 9. diff --format osc
    if let Some(osc_path) = osc {
        tally(
            &mut counts,
            run_check_status(labels::DIFF_OSC, verbose, || {
                verify_derive_changes::run(harness, pbf, osc_path, direct_io)
            }),
        )?;
    } else {
        verify_summary(&format!("diff --format osc: {NO_OSC}"));
        skipped += 1;
    }

    // 10. renumber
    tally(
        &mut counts,
        run_check_status(labels::RENUMBER, verbose, || {
            verify_renumber::run(harness, pbf, dataset, None, false)
        }),
    )?;

    // 11. diff
    if let Some(osc_path) = osc {
        tally(
            &mut counts,
            run_check_status(labels::DIFF, verbose, || {
                verify_diff::run(harness, pbf, osc_path)
            }),
        )?;
    } else {
        verify_summary(&format!("diff: {NO_OSC}"));
        skipped += 1;
    }

    // Summary
    let total = counts.passed + counts.failed + counts.environment + skipped;
    let total_ms = suite_start.elapsed().as_millis();
    verify_summary(&format!(
        "all done: {} passed, {} failed, {} environment, {skipped} skipped out of {total} \
         ({total_ms}ms)",
        counts.passed, counts.failed, counts.environment
    ));

    // Outcomes were already reported per-check + in the banner; exit
    // non-zero without main re-printing an error.
    suite_result(&counts)
}

#[cfg(test)]
mod tests {
    use super::{Counts, suite_result};
    use crate::error::DevError;

    #[test]
    fn a_pbfhogg_failure_outranks_an_environment_outcome() {
        let code = |passed, failed, environment| match suite_result(&Counts {
            passed,
            failed,
            environment,
        }) {
            Ok(()) => 0,
            Err(DevError::ExitCode(code)) => code,
            Err(e) => panic!("unexpected {e}"),
        };
        assert_eq!(code(3, 0, 0), 0);
        assert_eq!(code(3, 1, 1), 1);
        assert_eq!(code(3, 0, 2), super::ENVIRONMENT_EXIT_CODE);
    }
}
