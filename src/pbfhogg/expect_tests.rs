#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::path::Path;

use super::*;
use crate::config::{Dataset, DatasetExpect, DiffVerdict, ResolvedPaths};
use crate::pbfhogg::commands::DiffFormat;

const A: &str = "0123456789abcdef0123456789abcdef";
const B: &str = "fedcba9876543210fedcba9876543210";

fn paths(dataset: &str) -> ResolvedPaths {
    let ds: Dataset = toml::from_str(dataset).unwrap();
    let root = Path::new("/nonexistent-brokkr-expect");
    ResolvedPaths {
        hostname: "h".into(),
        data_dir: root.to_path_buf(),
        scratch_dir: root.join("scratch"),
        output_dir: root.join("out"),
        target_dir: root.join("target"),
        drives: None,
        features: Vec::new(),
        datasets: HashMap::from([("d".to_owned(), ds)]),
    }
}

fn resolved(
    command: &PbfhoggCommand,
    params: &CommandParams,
    paths: &ResolvedPaths,
) -> Option<Expectation> {
    resolve(command, "d", "indexed", None, params, paths, Path::new("/")).unwrap()
}

#[test]
fn each_check_reads_its_own_declaration_on_the_selected_entry() {
    let paths = paths(
        r#"
[pbf.indexed]
file = "d.osm.pbf"
expect = { check-refs = "clean", check-index = "violations" }

[pbf.raw]
file = "d-raw.osm.pbf"
expect = { check-ids = "violations" }
"#,
    );
    let params = CommandParams::default();
    let refs = resolved(&PbfhoggCommand::CheckRefs, &params, &paths).unwrap();
    assert_eq!((refs.exit, refs.verdict), (0, "clean"));
    assert!(
        refs.source.contains("pbf.indexed.expect"),
        "{}",
        refs.source
    );
    let index = resolved(&PbfhoggCommand::CheckIndex, &params, &paths).unwrap();
    assert_eq!(index.exit, 1);
    // check-ids is declared on the raw variant only: unknown on indexed.
    assert!(resolved(&PbfhoggCommand::CheckIds { full: true }, &params, &paths).is_none());
    // A command that gives no answer has no expectation.
    assert!(resolved(&PbfhoggCommand::Sort, &params, &paths).is_none());
}

#[test]
fn a_dataset_without_declarations_expects_nothing() {
    let paths = paths("[pbf.indexed]\nfile = \"d.osm.pbf\"\n");
    let params = CommandParams::default();
    for command in [
        PbfhoggCommand::CheckRefs,
        PbfhoggCommand::CheckIds { full: false },
        PbfhoggCommand::CheckIndex,
        PbfhoggCommand::Diff {
            format: DiffFormat::Default,
        },
    ] {
        assert!(resolved(&command, &params, &paths).is_none(), "{command:?}");
    }
}

#[test]
fn a_diff_answer_is_keyed_by_the_osc_sequence() {
    let expect: PbfExpect = toml::from_str("diff = { \"4705\" = \"differs\" }").unwrap();
    let diff = PbfhoggCommand::Diff {
        format: DiffFormat::Osc,
    };
    let hit = for_entry(&diff, &expect, Some("4705"), "[t]").unwrap();
    assert_eq!((hit.exit, hit.verdict), (1, "differs"));
    assert!(for_entry(&diff, &expect, Some("4706"), "[t]").is_none());
    assert!(for_entry(&diff, &expect, None, "[t]").is_none());
}

#[test]
fn a_snapshot_pair_is_addressed_by_both_digests_in_either_order() {
    let dataset = format!(
        r#"
[pbf.indexed]
file = "base.osm.pbf"
xxhash = "{A}"

[snapshot.copy.pbf.indexed]
file = "copy.osm.pbf"
xxhash = "{B}"

[snapshot.loose.pbf.indexed]
file = "loose.osm.pbf"

[expect.diff-snapshots]
"{}+{A}" = "same"
"#,
        B.to_uppercase()
    );
    let paths = paths(&dataset);
    let snapshots = PbfhoggCommand::DiffSnapshots {
        format: DiffFormat::Default,
    };
    let params = |from: &str, to: &str| CommandParams {
        from_snapshot: Some(SnapshotRef::parse(from).unwrap()),
        to_snapshot: Some(SnapshotRef::parse(to).unwrap()),
        ..CommandParams::default()
    };
    for (from, to) in [("base", "copy"), ("copy", "base")] {
        let hit = resolved(&snapshots, &params(from, to), &paths).unwrap();
        assert_eq!((hit.exit, hit.verdict), (0, "same"), "{from} -> {to}");
    }
    // An entry without a registered digest cannot be addressed: unknown.
    assert!(resolved(&snapshots, &params("base", "loose"), &paths).is_none());
}

#[test]
fn one_pair_spelled_twice_is_a_load_error() {
    let text = format!("diff-snapshots = {{ \"{A}+{B}\" = \"same\", \"{B}+{A}\" = \"differs\" }}");
    let err = toml::from_str::<DatasetExpect>(&text)
        .unwrap_err()
        .to_string();
    assert!(err.contains("twice"), "{err}");
    let parsed: DatasetExpect =
        toml::from_str(&format!("diff-snapshots = {{ \"{A}+{B}\" = \"differs\" }}")).unwrap();
    assert_eq!(
        parsed.diff_snapshots.values().copied().collect::<Vec<_>>(),
        [DiffVerdict::Differs]
    );
}

#[test]
fn the_other_answer_is_restated_as_a_contradiction_of_the_declaration() {
    let declared = Expectation {
        exit: 0,
        verdict: "clean",
        source: "[here] check-refs".into(),
    };
    let failed = |code| DevError::Subprocess {
        program: "pbfhogg".into(),
        ending: crate::error::ProcessEnding::Code(code),
        stderr: String::new(),
    };
    let said = explain(&PbfhoggCommand::CheckRefs, Some(&declared), failed(1)).to_string();
    assert!(
        said.contains("[here] check-refs") && said.contains("clean"),
        "{said}"
    );
    // A crash is not an answer, and an undeclared run is not restated.
    let crash = explain(&PbfhoggCommand::CheckRefs, Some(&declared), failed(2)).to_string();
    assert!(!crash.contains("declares"), "{crash}");
    let plain = explain(&PbfhoggCommand::CheckRefs, None, failed(1)).to_string();
    assert!(!plain.contains("declares"), "{plain}");
}

#[test]
fn a_malformed_declaration_is_a_load_error() {
    assert!(toml::from_str::<PbfExpect>("check-refs = \"ok\"").is_err());
    assert!(toml::from_str::<PbfExpect>("check-everything = \"clean\"").is_err());
    assert!(toml::from_str::<DatasetExpect>("diff-snapshots = { \"abc\" = \"same\" }").is_err());
}
