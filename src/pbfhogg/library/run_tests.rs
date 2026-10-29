#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{Places, Report, printable, verify};
use crate::pbfhogg::binary::LibraryHarness;
use crate::pbfhogg::library::describe;

const DESCRIBE: &str = r#"{"protocol": 1, "scenarios": [
  {"id": "repeat-cat", "description": "cat once", "deadline_s": 60,
   "roles": [
     {"id": "input", "kind": "pbf", "source": "dataset", "variant": "indexed"},
     {"id": "out-a", "kind": "pbf", "source": "output", "owner": "a"},
     {"id": "tmp-a", "kind": "directory", "source": "scratch", "owner": "a"}
   ],
   "calls": [{"id": "a", "label": "cat input -> out-a"}],
   "assertions": [{"id": "a.outcome", "call": "a"}, {"id": "whole", "call": null}]}
]}"#;

const SCENARIO_DOC: &str = r#"{"protocol": 1, "scenario": "repeat-cat",
 "plan": {"calls": {"a": {"command": "cat", "input": "input"}}},
 "assertions": [{"id": "whole", "status": "pass", "detail": ""}]}"#;

const CALL_DOC: &str = r#"{"protocol": 1, "call": "a", "entry_us": 10, "return_us": 20, "seal_us": 30,
 "outcome": {"status": "ok"},
 "publications": [{"ordinal": 1, "decision": "authorized", "installed": true}],
 "cancel": null,
 "seal": {"state": "sealed", "outstanding_blocks": 0},
 "snapshot": {"writer_blocks": 3},
 "stream": {"state": "sealed", "drops": 0},
 "context": {"parallelism": 4},
 "diagnostics": [{"kind": "note", "key": "sort.x", "message": "hello", "truncated": false}],
 "assertions": [{"id": "a.outcome", "status": "pass", "detail": ""}]}"#;

/// A fixture harness: checks its argv and environment, writes its output
/// role and one FIFO marker, then `body` (which writes the artefacts) and
/// exits `code`.
fn script(body: &str, code: i32) -> String {
    format!(
        r#"[ "$1" = "--test-harness" ] || exit 9
[ "$3" = "--private-dir" ] && [ -d "$4" ] || exit 8
[ -d "$BROKKR_HARNESS_ARTEFACT_DIR" ] || exit 7
[ -p "$BROKKR_MARKER_FIFO" ] || exit 6
[ "$(pwd)" = "$(dirname "$2")/cwd" ] || exit 5
A="$BROKKR_HARNESS_ARTEFACT_DIR"
R=$(dirname "$2")
: > "$R/roles/out-a"
echo "15 %a CAT_START" > "$BROKKR_MARKER_FIFO"
{body}
exit {code}"#
    )
}

/// Write the scenario and call documents given.
fn documents(scenario: &str, call: &str) -> String {
    format!(
        "mkdir \"$A/calls\"\ncat > \"$A/scenario.json\" <<'EOF'\n{scenario}\nEOF\ncat > \"$A/calls/a.json\" <<'EOF'\n{call}\nEOF"
    )
}

struct Fixture {
    root: PathBuf,
    harness: LibraryHarness,
    input: PathBuf,
}

fn fixture(name: &str, body: &str) -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let root = crate::test_scratch::scratch("pbfhogg-library-run", name);
    let path = root.join("harness");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let input = root.join("input.osm.pbf");
    std::fs::write(&input, b"pbf").unwrap();
    for dir in ["library", "state"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let harness = LibraryHarness::fixture(&path, &root).unwrap();
    Fixture {
        root,
        harness,
        input,
    }
}

fn run(f: &Fixture) -> Report {
    let scenario = describe::parse(DESCRIBE.as_bytes())
        .unwrap()
        .scenarios
        .remove(0);
    let datasets = BTreeMap::from([("input".to_owned(), f.input.clone())]);
    verify(
        &f.harness,
        &scenario,
        &datasets,
        &Places {
            library_root: &f.root.join("library"),
            fifo_dir: &f.root.join("state"),
        },
    )
    .unwrap()
}

fn failing(name: &str, body: &str) -> Report {
    let f = fixture(name, body);
    let report = run(&f);
    assert!(!report.passed, "passed: {:?}", report.results);
    assert!(report.kept && report.run_dir.is_dir());
    report
}

fn has(report: &Report, needle: &str) -> bool {
    report.failures.iter().any(|f| f.contains(needle))
}

#[test]
fn a_passing_run_is_removed_and_reports_its_results() {
    let f = fixture("pass", &script(&documents(SCENARIO_DOC, CALL_DOC), 0));
    let report = run(&f);
    assert!(report.passed, "{:?}", report.failures);
    assert!(!report.kept && !report.run_dir.exists());
    let ids: Vec<String> = report
        .results
        .iter()
        .map(|(c, r)| format!("{}/{}", c.as_deref().unwrap_or("-"), r.id))
        .collect();
    assert_eq!(ids, ["-/whole", "a/a.outcome", "a/brokkr.seal"]);
    assert_eq!(report.diagnostics, ["a note sort.x: hello"]);
}

#[test]
fn the_request_binds_every_role_and_a_failed_run_keeps_its_evidence() {
    let body = format!(
        "cp \"$2\" \"$A/../../request-copy\"\n{}",
        documents(
            SCENARIO_DOC,
            &CALL_DOC.replacen(r#""status": "pass""#, r#""status": "fail""#, 1)
        )
    );
    let report = failing("fail", &script(&body, 1));
    assert!(has(&report, "a/a.outcome: fail"), "{:?}", report.failures);
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.run_dir.join("../request-copy")).unwrap())
            .unwrap();
    assert_eq!(request["protocol"], 1);
    assert_eq!(request["iteration"], 0);
    let bindings = request["bindings"].as_object().unwrap();
    assert_eq!(bindings.len(), 3);
    assert!(
        bindings["out-a"]
            .as_str()
            .unwrap()
            .ends_with("/roles/out-a")
    );
    assert!(
        bindings["input"]
            .as_str()
            .unwrap()
            .ends_with("/input.osm.pbf")
    );
    let evidence = report.run_dir.join("evidence");
    let stream = std::fs::read_to_string(evidence.join("stream.txt")).unwrap();
    assert_eq!(
        stream,
        "# markers, in arrival order\n15 %a CAT_START\n# counters, in arrival order\n"
    );
    assert!(
        std::fs::read_to_string(evidence.join("failures.txt"))
            .unwrap()
            .contains("fail")
    );
}

#[test]
fn an_exit_that_disagrees_with_the_results_fails() {
    let report = failing("exit1", &script(&documents(SCENARIO_DOC, CALL_DOC), 1));
    assert!(has(&report, "exited 1 but every result it reported passed"));
    let call = CALL_DOC.replacen(r#""status": "pass""#, r#""status": "unevaluated""#, 1);
    let report = failing("exit0", &script(&documents(SCENARIO_DOC, &call), 0));
    assert!(has(
        &report,
        "exited 0 but reported a result that did not pass"
    ));
    assert!(has(&report, "a/a.outcome: unevaluated"));
}

#[test]
fn any_output_fails_and_is_printed_framed() {
    let body = format!(
        "{}\nprintf 'noise\\nmore' >&2",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("stderr", &script(&body, 0));
    assert!(has(&report, "wrote to stderr"), "{:?}", report.failures);
    assert!(
        has(&report, "    | noise\n    | more"),
        "{:?}",
        report.failures
    );
}

#[test]
fn a_seal_that_did_not_complete_fails_as_brokkr_seal() {
    let call = CALL_DOC
        .replacen(
            r#""state": "sealed", "outstanding_blocks": 0"#,
            r#""state": "outstanding", "outstanding_blocks": 2"#,
            1,
        )
        .replacen(
            r#""snapshot": {"writer_blocks": 3}"#,
            r#""snapshot": null"#,
            1,
        );
    let report = failing("seal", &script(&documents(SCENARIO_DOC, &call), 0));
    assert!(has(&report, "a/brokkr.seal: fail"), "{:?}", report.failures);
    assert!(
        !has(&report, "exited 0"),
        "a brokkr result never makes exit 0 inconsistent"
    );
}

#[test]
fn a_missing_call_document_and_a_misplaced_result_fail() {
    let body =
        format!("mkdir \"$A/calls\"\ncat > \"$A/scenario.json\" <<'EOF'\n{SCENARIO_DOC}\nEOF");
    let report = failing("missing", &script(&body, 0));
    assert!(
        has(&report, "calls/a.json is missing"),
        "{:?}",
        report.failures
    );

    // The scenario-level result reported in the call's document.
    let scenario = SCENARIO_DOC.replacen(
        r#"[{"id": "whole", "status": "pass", "detail": ""}]"#,
        "[]",
        1,
    );
    let call = CALL_DOC.replacen(
        r#"[{"id": "a.outcome", "status": "pass", "detail": ""}]"#,
        r#"[{"id": "a.outcome", "status": "pass", "detail": ""}, {"id": "whole", "status": "pass", "detail": ""}]"#,
        1,
    );
    let report = failing("misplaced", &script(&documents(&scenario, &call), 0));
    assert!(
        has(
            &report,
            "calls/a.json reports \"whole\", which belongs to scenario.json"
        ),
        "{:?}",
        report.failures
    );
    assert!(has(&report, "scenario.json has no result for \"whole\""));
}

#[test]
fn a_run_directory_replaced_by_a_symlink_is_never_entered() {
    let body = format!(
        "{}\nmv \"$R\" \"$R.real\"\nln -s \"$R.real\" \"$R\"",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("run-link", &script(&body, 0));
    assert!(
        has(&report, "is no longer the directory"),
        "{:?}",
        report.failures
    );
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    let real = report.run_dir.with_file_name("run-1.real");
    assert!(real.join("artefacts").is_dir());
    assert!(
        !real.join("evidence").exists(),
        "nothing was written through it"
    );
}

#[test]
fn a_directory_replaced_by_a_symlink_is_never_traversed() {
    let body = format!(
        "{}\nmkdir \"$R/../decoy\"\n: > \"$R/../decoy/planted\"\nrm -r \"$R/roles\"\nln -s \"$R/../decoy\" \"$R/roles\"",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("roles-link", &script(&body, 0));
    assert!(
        has(&report, "roles has the wrong type"),
        "{:?}",
        report.failures
    );
    assert!(
        !has(&report, "planted"),
        "the link was followed: {:?}",
        report.failures
    );

    // On exit 2, a refusal behind a symlinked artefact directory is not read.
    let body = "mkdir \"$R/../elsewhere\"\ncat > \"$R/../elsewhere/refusal.json\" <<'EOF'\n{\"protocol\": 1, \"reason\": \"planted\"}\nEOF\nrmdir \"$A\"\nln -s \"$R/../elsewhere\" \"$A\"";
    let report = failing("artefacts-link", &script(body, 2));
    assert!(
        has(&report, "artefact directory is not a directory"),
        "{:?}",
        report.failures
    );
    assert!(!has(&report, "planted"), "{:?}", report.failures);
}

#[test]
fn the_layout_check_catches_what_survived_in_the_run_directory() {
    let body = format!(
        "{}\n: > cwd-leftover\n: > \"$R/stray\"\nmkdir \"$R/roles/out-a.tmp\"\necho x >> \"$2\"",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let report = failing("layout", &script(&body, 0));
    for needle in [
        "cwd/ is not empty",
        "an unexpected entry \"stray\"",
        "roles/\"out-a.tmp\" is not a bound role",
        "request.json was changed",
    ] {
        assert!(has(&report, needle), "{needle}: {:?}", report.failures);
    }
}

#[test]
fn an_output_role_never_created_is_legal() {
    let body = format!(
        "rm \"$R/roles/out-a\"\n{}",
        documents(SCENARIO_DOC, CALL_DOC)
    );
    let f = fixture("absent-output", &script(&body, 0));
    assert!(run(&f).passed);
}

#[test]
fn a_refusal_is_reported_with_its_reason() {
    let body = "cat > \"$A/refusal.json\" <<'EOF'\n{\"protocol\": 1, \"reason\": \"cannot bind tmp-a\"}\nEOF";
    let report = failing("refusal", &script(body, 2));
    assert!(
        has(
            &report,
            "could not run the request, exit 2: cannot bind tmp-a"
        ),
        "{:?}",
        report.failures
    );
}

#[test]
fn a_wall_longer_than_the_process_fails() {
    let call = CALL_DOC.replacen(r#""seal_us": 30"#, r#""seal_us": 999000000"#, 1);
    let report = failing("wall", &script(&documents(SCENARIO_DOC, &call), 0));
    assert!(has(&report, "seal_us - entry_us"), "{:?}", report.failures);
}

#[test]
fn a_plan_naming_the_run_directory_fails() {
    let scenario = SCENARIO_DOC.replacen(r#""input": "input""#, r#""input": "RUNDIR""#, 1);
    let body = format!(
        "{}\nsed -i \"s|RUNDIR|$R/x|\" \"$A/scenario.json\"",
        documents(&scenario, CALL_DOC)
    );
    let report = failing("path", &script(&body, 0));
    assert!(has(&report, "run-specific path"), "{:?}", report.failures);
}

#[test]
fn a_harness_past_its_deadline_is_killed_and_fails() {
    let f = fixture("deadline", &script("sleep 30", 0));
    let mut scenario = describe::parse(DESCRIBE.as_bytes())
        .unwrap()
        .scenarios
        .remove(0);
    scenario.deadline_s = 1;
    let datasets = BTreeMap::from([("input".to_owned(), f.input.clone())]);
    let report = verify(
        &f.harness,
        &scenario,
        &datasets,
        &Places {
            library_root: &f.root.join("library"),
            fifo_dir: &f.root.join("state"),
        },
    )
    .unwrap();
    assert!(!report.passed);
    assert!(
        has(&report, "killed at its 1.0s deadline"),
        "{:?}",
        report.failures
    );
}

#[test]
fn a_lease_held_past_the_harness_leaves_the_directory_untouched() {
    // A descendant keeps the inherited lease descriptor past the release
    // window, so nothing in the run directory may be trusted or written.
    // A layout breach and a malformed document are left too: neither may be
    // looked at.
    let body = "mkdir \"$A/calls\"\necho '{' > \"$A/scenario.json\"\n: > \"$R/stray\"\n(sleep 4 </dev/null >/dev/null 2>&1 &)";
    let report = failing("held", &script(body, 0));
    assert!(has(&report, "was not released"), "{:?}", report.failures);
    assert!(!has(&report, "stray"), "{:?}", report.failures);
    assert!(!has(&report, "artefacts"), "{:?}", report.failures);
    assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
    assert!(!report.run_dir.join("evidence").exists());
}

#[test]
fn printable_escapes_controls_but_tabs() {
    assert_eq!(printable("a\tb\nc\x1b[2J"), "a\tb\\nc\\u{1b}[2J");
}
