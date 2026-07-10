//! Verify: all - run all verify commands sequentially.

use std::path::Path;
use std::time::Instant;

use super::verify::{run_check, VerifyHarness};
use super::{
    verify_add_locations, verify_cat, verify_check_refs, verify_derive_changes, verify_diff,
    verify_extract, verify_getid_removeid, verify_merge, verify_multi_extract, verify_renumber,
    verify_sort, verify_tags_filter,
};
use crate::error::DevError;
use crate::output::verify_summary;

/// Tally a check's outcome into `(passed, failed)`. Kept a free function (not
/// a closure) so it doesn't hold a mutable borrow of the counters across the
/// suite - the final banner reads them directly.
fn tally(counts: &mut (u32, u32), result: &Result<(), DevError>) {
    match result {
        Ok(()) => counts.0 += 1,
        Err(_) => counts.1 += 1,
    }
}

/// Run all verify commands sequentially.
///
/// Each check runs under [`run_check`], so a failure is reported (with its
/// detail replayed unless `verbose`) but does not stop the remaining checks.
/// Passing checks print a single line; failures replay their captured detail.
/// Returns `Err(ExitCode(1))` when one or more checks failed - the per-check
/// lines and the banner have already reported everything.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn run(
    harness: &VerifyHarness,
    pbf: &Path,
    osc: Option<&Path>,
    bbox: Option<&str>,
    data_dir: &Path,
    project_root: &Path,
    direct_io: bool,
    dataset: &str,
    verbose: bool,
) -> Result<(), DevError> {
    let suite_start = Instant::now();
    let mut counts = (0u32, 0u32); // (passed, failed)
    let mut skipped: u32 = 0;

    // 1. sort
    tally(&mut counts, &run_check("sort", verbose, || {
        verify_sort::run(harness, pbf, direct_io)
    }));

    // 2. cat
    tally(&mut counts, &run_check("cat", verbose, || {
        verify_cat::run(harness, pbf, direct_io)
    }));

    // 3. extract
    if let Some(b) = bbox {
        tally(&mut counts, &run_check("extract", verbose, || {
            verify_extract::run(harness, pbf, b, direct_io)
        }));
    } else {
        verify_summary("extract: SKIPPED (no --bbox provided)");
        skipped += 1;
    }

    // 3b. multi-extract
    if let Some(b) = bbox {
        tally(&mut counts, &run_check("multi-extract", verbose, || {
            verify_multi_extract::run(harness, pbf, b, 5, direct_io)
        }));
    } else {
        verify_summary("multi-extract: SKIPPED (no --bbox provided)");
        skipped += 1;
    }

    // 4. tags-filter
    tally(&mut counts, &run_check("tags-filter", verbose, || {
        verify_tags_filter::run(harness, pbf, direct_io)
    }));

    // 5. getid-removeid
    tally(&mut counts, &run_check("getid-removeid", verbose, || {
        verify_getid_removeid::run(harness, pbf, direct_io)
    }));

    // 6. add-locations-to-ways
    tally(&mut counts, &run_check("add-locations-to-ways", verbose, || {
        verify_add_locations::run(harness, pbf, crate::cli::AltwMode::All, direct_io)
    }));

    // 7. check-refs
    tally(&mut counts, &run_check("check-refs", verbose, || {
        verify_check_refs::run(harness, pbf, direct_io)
    }));

    // 8. apply-changes
    if let Some(osc_path) = osc {
        // Best-effort osmosis setup - merge works without it. Done outside
        // run_check so any setup output isn't captured as check detail.
        let osmosis = crate::tools::ensure_osmosis(data_dir, project_root).ok();
        tally(&mut counts, &run_check("apply-changes", verbose, || {
            verify_merge::run(harness, pbf, osc_path, osmosis.as_ref(), direct_io)
        }));
    } else {
        verify_summary("apply-changes: SKIPPED (no --osc provided)");
        skipped += 1;
    }

    // 9. diff --format osc
    if let Some(osc_path) = osc {
        tally(&mut counts, &run_check("diff --format osc", verbose, || {
            verify_derive_changes::run(harness, pbf, osc_path, direct_io)
        }));
    } else {
        verify_summary("diff --format osc: SKIPPED (no --osc provided)");
        skipped += 1;
    }

    // 10. renumber
    tally(&mut counts, &run_check("renumber", verbose, || {
        verify_renumber::run(harness, pbf, dataset, None, false)
    }));

    // 11. diff
    if let Some(osc_path) = osc {
        tally(&mut counts, &run_check("diff", verbose, || {
            verify_diff::run(harness, pbf, osc_path)
        }));
    } else {
        verify_summary("diff: SKIPPED (no --osc provided)");
        skipped += 1;
    }

    // Summary
    let (passed, failed) = counts;
    let total = passed + failed + skipped;
    let total_ms = suite_start.elapsed().as_millis();
    verify_summary(&format!(
        "all done: {passed} passed, {failed} failed, {skipped} skipped out of {total} ({total_ms}ms)"
    ));

    if failed > 0 {
        // Failures were already reported per-check + in the banner; exit
        // non-zero without main re-printing an error.
        return Err(DevError::ExitCode(1));
    }
    Ok(())
}
