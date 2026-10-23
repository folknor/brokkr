//! Verify: getid - pbfhogg getid vs osmium getid, plus getid --invert complement test.

use std::path::Path;

use super::commands::GETID_BENCH_IDS;
use super::verify::{Findings, Verdict, VerifyHarness, Writes};
use super::verify_multi_extract::inspect_counts;
use crate::error::DevError;
use crate::output::verify_msg;

/// Run getid cross-validation: pbfhogg getid vs osmium getid,
/// then pbfhogg getid --invert complement test.
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<Findings, DevError> {
    let outdir = harness.subdir("getid-removeid")?;
    let pbf_str = pbf.display().to_string();

    verify_msg("--- getid: pbfhogg vs osmium ---");

    // pbfhogg getid <pbf> -o <out> <ids...>
    let pbfhogg_getid = outdir.join("pbfhogg-getid.osm.pbf");
    let pbfhogg_getid_str = pbfhogg_getid.display().to_string();
    let mut pbfhogg_args: Vec<&str> = vec!["getid", &pbf_str, "-o", &pbfhogg_getid_str];
    if direct_io {
        pbfhogg_args.push("--direct-io");
    }
    pbfhogg_args.extend_from_slice(GETID_BENCH_IDS);
    let captured = harness.run_pbfhogg(&pbfhogg_args, Writes::OwnedOutput)?;
    harness.check_exit(&captured, "pbfhogg getid")?;

    let selected = inspect_counts(harness, &pbfhogg_getid)?;
    if selected.nodes == 0 && selected.ways == 0 && selected.relations == 0 {
        return Err(DevError::Config(
            "the fixed getid verification IDs do not occur in this PBF; the comparison would be vacuous"
                .into(),
        ));
    }
    // Verification commands have no result row; keep the dataset-dependent
    // hit counts in the verification transcript instead of treating the
    // fixed ID list itself as evidence that any IDs were selected.
    verify_msg(&format!(
        "  fixed IDs selected: {} nodes, {} ways, {} relations",
        selected.nodes, selected.ways, selected.relations
    ));

    // osmium getid <pbf> <ids...> -o <out> --overwrite
    let osmium_getid = outdir.join("osmium-getid.osm.pbf");
    let osmium_getid_str = osmium_getid.display().to_string();
    let mut osmium_args: Vec<&str> = vec!["getid", &pbf_str];
    osmium_args.extend_from_slice(GETID_BENCH_IDS);
    osmium_args.extend_from_slice(&["-o", &osmium_getid_str, "--overwrite"]);
    let captured = harness.run_tool("osmium", &osmium_args)?;
    harness.check_exit(&captured, "osmium getid")?;

    // Print inspect output for both getid outputs.
    harness.print_inspect("pbfhogg getid", &pbfhogg_getid)?;
    harness.print_inspect("osmium getid", &osmium_getid)?;

    let mut findings = Findings::new();

    // Diff and report.
    let diff = harness.diff_pbfs(&pbfhogg_getid, &osmium_getid)?;
    if diff.is_pass() {
        verify_msg("  getid: PASS (identical)");
    } else {
        verify_msg("  getid: FAIL (differences found)");
    }
    findings.record("getid diff", diff);

    // Compare sort feature flags.
    findings.record(
        "getid sort feature",
        harness.compare_sort_feature(&pbfhogg_getid, &osmium_getid)?,
    );

    // --- getid --invert: complement test ---
    verify_msg("--- getid --invert: complement test ---");

    let pbfhogg_invert = outdir.join("pbfhogg-getid-invert.osm.pbf");
    let pbfhogg_invert_str = pbfhogg_invert.display().to_string();
    let mut invert_args: Vec<&str> = vec!["getid", "--invert", &pbf_str, "-o", &pbfhogg_invert_str];
    if direct_io {
        invert_args.push("--direct-io");
    }
    invert_args.extend_from_slice(GETID_BENCH_IDS);
    let captured = harness.run_pbfhogg(&invert_args, Writes::OwnedOutput)?;
    harness.check_exit(&captured, "pbfhogg getid --invert")?;

    // The complement assertion: getid selects exactly the listed IDs (the
    // osmium diff above pins that) and getid --invert drops exactly them, so
    // per element kind the two outputs must add up to the original. A kind
    // whose sum is off means --invert kept or dropped something it shouldn't.
    let original = inspect_counts(harness, pbf)?;
    let selected = inspect_counts(harness, &pbfhogg_getid)?;
    let inverted = inspect_counts(harness, &pbfhogg_invert)?;
    for (kind, orig, sel, inv) in [
        ("nodes", original.nodes, selected.nodes, inverted.nodes),
        ("ways", original.ways, selected.ways, inverted.ways),
        (
            "relations",
            original.relations,
            selected.relations,
            inverted.relations,
        ),
    ] {
        let verdict = complement_verdict(orig, sel, inv);
        if verdict.is_pass() {
            verify_msg(&format!(
                "  complement ({kind}): PASS ({sel} selected + {inv} inverted = {orig} original)"
            ));
        } else {
            verify_msg(&format!(
                "  complement ({kind}): FAIL ({sel} selected + {inv} inverted != {orig} original)"
            ));
        }
        findings.record(&format!("getid --invert complement ({kind})"), verdict);
    }

    Ok(findings)
}

/// `selected + inverted == original`, without overflow on absurd counts.
fn complement_verdict(original: u64, selected: u64, inverted: u64) -> Verdict {
    Verdict::from_pass(selected.checked_add(inverted) == Some(original))
}

#[cfg(test)]
mod tests {
    use super::complement_verdict;
    use crate::pbfhogg::verify::Verdict;

    #[test]
    fn complement_holds_when_parts_sum_to_whole() {
        assert_eq!(complement_verdict(100, 3, 97), Verdict::Pass);
        assert_eq!(complement_verdict(0, 0, 0), Verdict::Pass);
    }

    #[test]
    fn complement_fails_on_leak_or_loss() {
        // --invert kept a listed element (leak) or dropped an extra one (loss).
        assert_eq!(complement_verdict(100, 3, 98), Verdict::Fail);
        assert_eq!(complement_verdict(100, 3, 96), Verdict::Fail);
        assert_eq!(complement_verdict(5, u64::MAX, 1), Verdict::Fail);
    }
}
