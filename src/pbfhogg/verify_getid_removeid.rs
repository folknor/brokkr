//! Verify: getid - pbfhogg getid vs osmium getid, plus getid --invert complement test.

use std::path::Path;

use super::pbf_frames::{self, IndexSummary};
use super::verify::{ExternalTool, Findings, PbfArgv, Verdict, VerifyHarness};
use super::verify_multi_extract::{Counts, inspect_counts};
use crate::error::DevError;
use crate::output::verify_msg;

const KIND_LETTERS: [char; 3] = ['n', 'w', 'r'];

/// The IDs to request, per kind (node, way, relation): the lowest and highest
/// ID the input's indexdata claims for that kind, so both exist in any input
/// whose claims are true, whatever the dataset. Negative IDs are left out
/// (pbfhogg getid does not accept them); a kind with none left requests
/// nothing.
fn endpoint_ids(summary: &IndexSummary) -> [Vec<i64>; 3] {
    let mut ids: [Vec<i64>; 3] = Default::default();
    for (kind, claim) in summary.kinds.iter().enumerate() {
        let Some(claim) = claim else { continue };
        for id in [claim.min_id, claim.max_id] {
            if id >= 0 && !ids[kind].contains(&id) {
                ids[kind].push(id);
            }
        }
    }
    ids
}

fn id_args(ids: &[Vec<i64>; 3]) -> Vec<String> {
    ids.iter()
        .zip(KIND_LETTERS)
        .flat_map(|(ids, letter)| ids.iter().map(move |id| format!("{letter}{id}")))
        .collect()
}

/// The per-kind cardinality assertion: on a snapshot (one object per kind
/// and ID) getid must select exactly the requested IDs that exist, and every
/// requested ID exists by the indexdata claim.
fn selection_verdicts(ids: &[Vec<i64>; 3], selected: Counts) -> [(&'static str, usize, u64); 3] {
    [
        ("nodes", ids[0].len(), selected.nodes),
        ("ways", ids[1].len(), selected.ways),
        ("relations", ids[2].len(), selected.relations),
    ]
}

/// Run getid cross-validation: pbfhogg getid vs osmium getid,
/// then pbfhogg getid --invert complement test.
///
/// The IDs come from the input itself (see [`endpoint_ids`]), read by a
/// header-only walk of its blob framing ([`pbf_frames::walk`]): no element
/// data is read to choose them, so the cost is the blob count even at
/// planet scale. The walk refuses an input that is not fully indexed in a
/// form pbfhogg reads, and a history file (the cardinality assertion needs
/// one object per kind and ID).
#[allow(clippy::too_many_lines)]
pub fn run(harness: &VerifyHarness, pbf: &Path, direct_io: bool) -> Result<Findings, DevError> {
    let outdir = harness.subdir("getid-removeid")?;
    let pbf_str = pbf.display().to_string();

    verify_msg("--- getid: choosing IDs from the input's indexdata ---");
    let summary = {
        let _step = super::verify::step("brokkr index walk");
        pbf_frames::walk(pbf)?
    };
    if summary.is_history() {
        return Err(DevError::Config(format!(
            "{} declares HistoricalInformation; getid-removeid asserts one object per kind \
             and ID, which a history file does not have",
            pbf.display()
        )));
    }
    let ids = endpoint_ids(&summary);
    let requested = id_args(&ids);
    if requested.is_empty() {
        return Err(DevError::Config(format!(
            "{} has no element with a non-negative ID to request; the comparison would be vacuous",
            pbf.display()
        )));
    }
    verify_msg(&format!("  requesting: {}", requested.join(" ")));

    let mut findings = Findings::new();

    verify_msg("--- getid: pbfhogg vs osmium ---");

    // pbfhogg getid <pbf> -o <out> <ids...>
    let pbfhogg_getid = outdir.join("pbfhogg-getid.osm.pbf");
    let captured = harness.run_pbfhogg(
        PbfArgv::new("getid")
            .input(pbf)?
            .output(&pbfhogg_getid)?
            .arg_if(direct_io, "--direct-io")
            .args_from(requested.iter().map(String::as_str)),
    )?;
    harness.check_exit(&captured, "pbfhogg getid")?;

    let selected = inspect_counts(harness, &pbfhogg_getid)?;
    for (kind, want, got) in selection_verdicts(&ids, selected) {
        let verdict = Verdict::from_pass(u64::try_from(want).ok() == Some(got));
        if verdict.is_pass() {
            verify_msg(&format!(
                "  selected {kind}: {got} of {want} requested PASS"
            ));
        } else {
            verify_msg(&format!(
                "  selected {kind}: {got}, but {want} requested IDs exist FAIL"
            ));
        }
        findings.record(&format!("getid selected {kind}"), verdict);
    }

    // osmium getid <pbf> <ids...> -o <out> --overwrite
    let osmium_getid = outdir.join("osmium-getid.osm.pbf");
    let osmium_getid_str = osmium_getid.display().to_string();
    let mut osmium_args: Vec<&str> = vec!["getid", &pbf_str];
    osmium_args.extend(requested.iter().map(String::as_str));
    osmium_args.extend_from_slice(&["-o", &osmium_getid_str, "--overwrite"]);
    let captured = harness.run_tool(
        ExternalTool::Osmium,
        &osmium_args,
        &outdir.join("osmium-getid.stdout"),
    )?;
    harness.check_exit(&captured, "osmium getid")?;

    // Print inspect output for both getid outputs.
    harness.print_inspect("pbfhogg getid", &pbfhogg_getid)?;
    harness.print_inspect("osmium getid", &osmium_getid)?;

    // Diff and report.
    let diff = harness.diff_pbfs(
        &pbfhogg_getid,
        &osmium_getid,
        &super::verify::listing_beside(&pbfhogg_getid),
    )?;
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
    let captured = harness.run_pbfhogg(
        PbfArgv::new("getid")
            .arg("--invert")
            .input(pbf)?
            .output(&pbfhogg_invert)?
            .arg_if(direct_io, "--direct-io")
            .args_from(requested.iter().map(String::as_str)),
    )?;
    harness.check_exit(&captured, "pbfhogg getid --invert")?;

    // The complement assertion: getid selects exactly the listed IDs (the
    // osmium diff above pins that) and getid --invert drops exactly them, so
    // per element kind the two outputs must add up to the original. A kind
    // whose sum is off means --invert kept or dropped something it shouldn't.
    // The original's counts are its indexdata claims, from the walk above -
    // what pbfhogg inspect reports for it too, without another pass.
    let original = Counts {
        nodes: summary.count(0),
        ways: summary.count(1),
        relations: summary.count(2),
    };
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
    use super::{complement_verdict, endpoint_ids, id_args};
    use crate::pbfhogg::pbf_frames::{IndexSummary, KindClaim};
    use crate::pbfhogg::verify::Verdict;

    #[test]
    fn endpoints_are_deduplicated_and_skip_negatives_and_empty_kinds() {
        let claim = |min_id, max_id| {
            Some(KindClaim {
                min_id,
                max_id,
                count: 1,
            })
        };
        let summary = IndexSummary {
            kinds: [claim(1, 13_000_000_000), claim(-5, 7), None],
            required_features: Vec::new(),
        };
        let ids = endpoint_ids(&summary);
        assert_eq!(ids[0], vec![1, 13_000_000_000]);
        assert_eq!(ids[1], vec![7]);
        assert!(ids[2].is_empty());
        assert_eq!(id_args(&ids), vec!["n1", "n13000000000", "w7"]);

        let single = IndexSummary {
            kinds: [None, None, claim(42, 42)],
            required_features: Vec::new(),
        };
        assert_eq!(id_args(&endpoint_ids(&single)), vec!["r42"]);
        let negative = IndexSummary {
            kinds: [claim(-9, -1), None, None],
            required_features: Vec::new(),
        };
        assert!(id_args(&endpoint_ids(&negative)).is_empty());
    }

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
