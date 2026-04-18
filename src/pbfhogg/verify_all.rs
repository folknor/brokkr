//! Verify: all - run all verify commands sequentially.

use std::path::Path;
use std::time::Instant;

use super::verify::VerifyHarness;
use super::{
    verify_add_locations, verify_cat, verify_check_refs, verify_derive_changes, verify_diff,
    verify_extract, verify_getid_removeid, verify_merge, verify_multi_extract, verify_renumber,
    verify_sort, verify_tags_filter,
};
use crate::error::DevError;
use crate::output::verify_msg;

/// Elapsed milliseconds as u64 (truncation is safe - verify commands won't run for 584M years).
#[allow(clippy::cast_possible_truncation)]
fn elapsed_ms(t: &Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

/// Run all verify commands sequentially.
///
/// Each command is wrapped so that a failure is logged but does not prevent
/// the remaining commands from running. Returns `Err` when one or more
/// commands failed; individual failures are reported inline via `verify_msg`.
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
) -> Result<(), DevError> {
    let mut passed: u32 = 0;
    let mut failed: u32 = 0;
    let mut skipped: u32 = 0;
    let mut timings: Vec<(String, u64)> = Vec::new();

    // Helper: run one verify command, track pass/fail and elapsed time.
    let mut run_one = |name: &str, result: Result<(), DevError>, elapsed_ms: u64| {
        timings.push((name.to_owned(), elapsed_ms));
        match result {
            Ok(()) => {
                verify_msg(&format!("{name}: PASS ({elapsed_ms}ms)"));
                passed += 1;
            }
            Err(e) => {
                verify_msg(&format!("{name} failed ({elapsed_ms}ms): {e}"));
                failed += 1;
            }
        }
    };

    // Helper: log a skipped command.
    let mut skip = |name: &str, reason: &str| {
        verify_msg(&format!("{name}: SKIPPED ({reason})"));
        skipped += 1;
    };

    // 1. sort
    verify_msg("========== sort ==========");
    let t = Instant::now();
    run_one("sort", verify_sort::run(harness, pbf, direct_io), elapsed_ms(&t));

    // 2. cat
    verify_msg("========== cat ==========");
    let t = Instant::now();
    run_one("cat", verify_cat::run(harness, pbf, direct_io), elapsed_ms(&t));

    // 3. extract
    verify_msg("========== extract ==========");
    if let Some(b) = bbox {
        let t = Instant::now();
        run_one("extract", verify_extract::run(harness, pbf, b, direct_io), elapsed_ms(&t));
    } else {
        skip("extract", "no --bbox provided");
    }

    // 3b. multi-extract
    verify_msg("========== multi-extract ==========");
    if let Some(b) = bbox {
        let t = Instant::now();
        run_one(
            "multi-extract",
            verify_multi_extract::run(harness, pbf, b, 5, direct_io),
            elapsed_ms(&t),
        );
    } else {
        skip("multi-extract", "no --bbox provided");
    }

    // 4. tags-filter
    verify_msg("========== tags-filter ==========");
    let t = Instant::now();
    run_one(
        "tags-filter",
        verify_tags_filter::run(harness, pbf, direct_io),
        elapsed_ms(&t),
    );

    // 5. getid-removeid
    verify_msg("========== getid-removeid ==========");
    let t = Instant::now();
    run_one(
        "getid-removeid",
        verify_getid_removeid::run(harness, pbf, direct_io),
        elapsed_ms(&t),
    );

    // 6. add-locations-to-ways
    verify_msg("========== add-locations-to-ways ==========");
    let t = Instant::now();
    run_one(
        "add-locations-to-ways",
        verify_add_locations::run(harness, pbf, crate::cli::AltwMode::All, direct_io),
        elapsed_ms(&t),
    );

    // 7. check-refs
    verify_msg("========== check-refs ==========");
    let t = Instant::now();
    run_one(
        "check-refs",
        verify_check_refs::run(harness, pbf, direct_io),
        elapsed_ms(&t),
    );

    // 8. apply-changes
    verify_msg("========== apply-changes ==========");
    if let Some(osc_path) = osc {
        // Best-effort osmosis setup - merge works without it.
        let osmosis = match crate::tools::ensure_osmosis(data_dir, project_root) {
            Ok(tools) => Some(tools),
            Err(e) => {
                verify_msg(&format!("osmosis not available (non-fatal): {e}"));
                None
            }
        };
        let t = Instant::now();
        run_one(
            "apply-changes",
            verify_merge::run(harness, pbf, osc_path, osmosis.as_ref(), direct_io),
            elapsed_ms(&t),
        );
    } else {
        skip("apply-changes", "no --osc provided");
    }

    // 9. diff --format osc
    verify_msg("========== diff --format osc ==========");
    if let Some(osc_path) = osc {
        let t = Instant::now();
        run_one(
            "diff --format osc",
            verify_derive_changes::run(harness, pbf, osc_path, direct_io),
            elapsed_ms(&t),
        );
    } else {
        skip("diff --format osc", "no --osc provided");
    }

    // 10. renumber
    verify_msg("========== renumber ==========");
    let t = Instant::now();
    run_one(
        "renumber",
        verify_renumber::run(harness, pbf, dataset, None, false),
        elapsed_ms(&t),
    );

    // 11. diff
    verify_msg("========== diff ==========");
    if let Some(osc_path) = osc {
        let t = Instant::now();
        run_one("diff", verify_diff::run(harness, pbf, osc_path), elapsed_ms(&t));
    } else {
        skip("diff", "no --osc provided");
    }

    // Summary
    let total = passed + failed + skipped;
    let total_ms: u64 = timings.iter().map(|(_, ms)| ms).sum();
    verify_msg(&format!(
        "===== all done: {passed} passed, {failed} failed, {skipped} skipped out of {total} ({total_ms}ms) ====="
    ));
    for (name, ms) in &timings {
        verify_msg(&format!("  {name}: {ms}ms"));
    }

    if failed > 0 {
        return Err(DevError::Verify(format!(
            "{failed} verify command(s) failed"
        )));
    }
    Ok(())
}
