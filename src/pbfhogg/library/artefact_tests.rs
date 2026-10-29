#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

const DESCRIBE: &str = r#"{"protocol": 1, "scenarios": [
  {"id": "pair", "description": "two calls", "deadline_s": 60,
   "roles": [],
   "calls": [{"id": "a", "label": "first"}, {"id": "b", "label": "second"}],
   "assertions": [{"id": "a.outcome", "call": "a"}, {"id": "b.outcome", "call": "b"},
                  {"id": "whole", "call": null}]}
]}"#;

fn scenario() -> Scenario {
    describe::parse(DESCRIBE.as_bytes())
        .unwrap()
        .scenarios
        .remove(0)
}

/// A valid call document for `a`, every top-level field named.
const CALL: &str = r#"{"protocol": 1, "call": "a", "entry_us": 10, "return_us": 20, "seal_us": 30,
 "outcome": {"status": "ok"},
 "publications": [{"ordinal": 1, "decision": "authorized", "installed": true},
                  {"ordinal": 2, "decision": "refused_cancelled", "installed": false}],
 "cancel": {"result": "after_decision", "ordinal": 1},
 "seal": {"state": "sealed", "outstanding_blocks": 0},
 "snapshot": {"writer_blocks": 3, "pipeline wait_ns": 18446744073709551615},
 "stream": {"state": "sealed", "drops": 0},
 "context": {"parallelism": 4, "executor": "ambient"},
 "diagnostics": [{"kind": "note", "key": "sort.distrust_metadata", "message": "m", "truncated": false}],
 "assertions": [{"id": "a.outcome", "status": "pass", "detail": "line one\nline two"}]}"#;

fn call(text: &str) -> Result<CallDoc, String> {
    parse_call(text.as_bytes(), "a", &["/run/dir"])
}

fn edited(from: &str, to: &str) -> String {
    assert!(CALL.contains(from), "{from}");
    CALL.replacen(from, to, 1)
}

#[test]
fn a_valid_call_document_parses() {
    let doc = call(CALL).unwrap();
    assert_eq!((doc.entry_us, doc.return_us, doc.seal_us), (10, 20, 30));
    assert_eq!(doc.seal, Seal::Sealed);
    assert_eq!(doc.outcome, CallOutcome::Ok);
    assert_eq!(doc.diagnostics[0].key, "sort.distrust_metadata");
    assert_eq!(doc.results[0].detail, "line one\nline two");
}

#[test]
fn readings_must_be_ordered_and_in_range() {
    let e = call(&edited(r#""return_us": 20"#, r#""return_us": 5"#)).unwrap_err();
    assert!(e.contains("not in order"), "{e}");
    let e = call(&edited(
        r#""seal_us": 30"#,
        r#""seal_us": 9223372036854775808"#,
    ))
    .unwrap_err();
    assert!(e.contains("seal_us"), "{e}");
}

#[test]
fn the_call_field_names_its_own_file() {
    let e = parse_call(CALL.as_bytes(), "b", &[]).unwrap_err();
    assert!(e.contains(r#""a", not "b""#), "{e}");
}

#[test]
fn a_seal_decides_its_snapshot() {
    let outstanding = edited(
        r#""state": "sealed", "outstanding_blocks": 0"#,
        r#""state": "outstanding", "outstanding_blocks": 2"#,
    );
    let e = call(&outstanding).unwrap_err();
    assert!(e.contains("must be null unless the call sealed"), "{e}");
    let ok = outstanding.replacen(
        r#""snapshot": {"writer_blocks": 3, "pipeline wait_ns": 18446744073709551615}"#,
        r#""snapshot": null"#,
        1,
    );
    assert_eq!(call(&ok).unwrap().seal, Seal::Outstanding(2));
    let e = call(&edited(
        r#""outstanding_blocks": 0"#,
        r#""outstanding_blocks": 1"#,
    ))
    .unwrap_err();
    assert!(e.contains("outstanding_blocks"), "{e}");
    let refused = edited(
        r#""state": "sealed", "outstanding_blocks": 0"#,
        r#""state": "refused", "outstanding_blocks": 0"#,
    );
    assert!(
        call(&refused)
            .unwrap_err()
            .contains("not allowed on a refused seal")
    );
    let e = call(&edited(r#""writer_blocks": 3"#, r#""a=b": 3"#)).unwrap_err();
    assert!(e.contains("not a counter name"), "{e}");
}

#[test]
fn publications_and_cancel_must_agree() {
    let e = call(&edited(r#""ordinal": 2"#, r#""ordinal": 3"#)).unwrap_err();
    assert!(e.contains("without gaps"), "{e}");
    let e = call(&edited(
        r#""decision": "refused_cancelled", "installed": false"#,
        r#""decision": "refused_cancelled", "installed": true"#,
    ))
    .unwrap_err();
    assert!(e.contains("cannot be installed"), "{e}");
    let e = call(&edited(
        r#""result": "after_decision", "ordinal": 1"#,
        r#""result": "after_decision", "ordinal": 2"#,
    ))
    .unwrap_err();
    assert!(e.contains("not an authorized publication"), "{e}");
    let e = call(&edited(
        r#""decision": "refused_cancelled", "installed": false"#,
        r#""decision": "authorized", "installed": false"#,
    ))
    .unwrap_err();
    assert!(e.contains("authorized after the cancel"), "{e}");
    let pending = edited(
        r#""result": "after_decision", "ordinal": 1"#,
        r#""result": "no_publication_pending""#,
    );
    assert!(call(&pending).is_ok());
    let e = call(&edited(
        r#""result": "after_decision", "ordinal": 1"#,
        r#""result": "no_publication_pending", "ordinal": 1"#,
    ))
    .unwrap_err();
    assert!(e.contains("unknown key \"ordinal\""), "{e}");
}

#[test]
fn context_and_plan_stay_in_the_plain_value_domain() {
    let e = call(&edited(r#""parallelism": 4"#, r#""parallelism": 4.0"#)).unwrap_err();
    assert!(e.contains("expected an integer"), "{e}");
    let e = call(&edited(
        r#""executor": "ambient""#,
        r#""executor": "/run/dir/x""#,
    ))
    .unwrap_err();
    assert!(e.contains("run-specific path"), "{e}");
    let e = call(&edited(r#""executor": "ambient""#, r#""/run/dir": 1"#)).unwrap_err();
    assert!(e.contains("key contains the run-specific path"), "{e}");
    let e = call(&edited(r#""parallelism": 4, "executor": "ambient""#, "")).unwrap_err();
    assert!(e.contains("an empty object"), "{e}");

    let scenario = scenario();
    let doc = |plan: &str| {
        format!(r#"{{"protocol": 1, "scenario": "pair", "plan": {plan}, "assertions": []}}"#)
    };
    let ok = doc(r#"{"calls": {"a": {"cmd": "cat"}, "b": {"cmd": "cat"}}, "order": [1, -2]}"#);
    assert!(parse_scenario(ok.as_bytes(), &scenario, &[]).is_ok());
    for (plan, why) in [
        (
            r#"{"calls": {"a": {"cmd": "cat"}}}"#,
            "call \"b\" is missing",
        ),
        (
            r#"{"calls": {"a": {"cmd": "cat"}, "b": {}}}"#,
            "an empty object",
        ),
        (
            r#"{"calls": {"a": {"x": 1}, "b": {"x": 1}, "c": {"x": 1}}}"#,
            "not a declared call",
        ),
        (
            r#"{"calls": {"a": {"x": 1}, "b": {"x": -0}}}"#,
            "expected an integer",
        ),
        (
            r#"{"calls": {"a": {"x": 1}, "b": {"x": 1, "x": 2}}}"#,
            "duplicate key",
        ),
        (
            r#"{"calls": {"a": {"x": "a\u0000"}, "b": {"x": 1}}}"#,
            "NUL",
        ),
        (r#"{}"#, "missing key \"calls\""),
    ] {
        let e = parse_scenario(doc(plan).as_bytes(), &scenario, &[]).unwrap_err();
        assert!(e.contains(why), "{plan}: {e}");
    }
}

#[test]
fn diagnostics_carry_kind_key_and_a_bounded_message() {
    let e = call(&edited(r#""kind": "note""#, r#""kind": "Note""#)).unwrap_err();
    assert!(e.contains("not an identifier"), "{e}");
    let long = "m".repeat(4001);
    let e = call(&edited(
        r#""message": "m""#,
        &format!(r#""message": "{long}""#),
    ))
    .unwrap_err();
    assert!(e.contains("0 to 4000 bytes"), "{e}");
    let e = call(&edited(r#", "truncated": false"#, "")).unwrap_err();
    assert!(e.contains("missing key \"truncated\""), "{e}");
}

#[test]
fn results_are_closed_and_unique() {
    let e = call(&edited(r#""status": "pass""#, r#""status": "skip""#)).unwrap_err();
    assert!(e.contains("unknown status"), "{e}");
    let twice = edited(
        r#"[{"id": "a.outcome", "status": "pass", "detail": "line one\nline two"}]"#,
        r#"[{"id": "a.outcome", "status": "pass", "detail": ""}, {"id": "a.outcome", "status": "fail", "detail": ""}]"#,
    );
    assert!(call(&twice).unwrap_err().contains("reported twice"));
    let e = call(&edited(
        r#""stream": {"state": "sealed""#,
        r#""stream": {"state": "draining""#,
    ))
    .unwrap_err();
    assert!(e.contains("unknown stream state"), "{e}");
}

#[test]
fn the_inventory_must_be_reported_exactly_where_declared() {
    let scenario = scenario();
    let result = |id: &str| Outcome {
        id: id.to_owned(),
        status: Status::Pass,
        detail: String::new(),
    };
    let ok = artefact_ok(
        &scenario,
        &[result("whole")],
        &[("a", &["a.outcome"]), ("b", &["b.outcome"])],
    );
    assert!(ok.is_empty(), "{ok:?}");
    let failures = artefact_ok(
        &scenario,
        &[],
        &[("a", &["a.outcome", "b.outcome", "other"]), ("b", &[])],
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("scenario.json has no result for \"whole\""))
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("which belongs to calls/b.json"))
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("\"other\", which describe does not declare"))
    );
    assert!(
        failures
            .iter()
            .any(|f| f.contains("calls/b.json has no result for \"b.outcome\""))
    );

    fn artefact_ok(scenario: &Scenario, top: &[Outcome], calls: &[(&str, &[&str])]) -> Vec<String> {
        let calls: Vec<(String, Vec<Outcome>)> = calls
            .iter()
            .map(|(c, ids)| {
                (
                    (*c).to_owned(),
                    ids.iter()
                        .map(|id| Outcome {
                            id: (*id).to_owned(),
                            status: Status::Pass,
                            detail: String::new(),
                        })
                        .collect(),
                )
            })
            .collect();
        inventory_failures(scenario, top, &calls)
    }
}

#[test]
fn the_tree_admits_exactly_its_documents_and_never_follows_a_link() {
    let root = crate::test_scratch::scratch("pbfhogg-library-artefact", "tree");
    let scenario = scenario();
    let dir = root.join("artefacts");
    std::fs::create_dir_all(dir.join(CALLS_DIR)).unwrap();
    std::fs::write(dir.join(SCENARIO_FILE), b"{}").unwrap();
    std::fs::write(dir.join("calls/a.json"), b"{}").unwrap();
    let e = read_tree(&dir, &scenario).err().unwrap();
    assert!(e.contains("calls/b.json is missing"), "{e}");
    std::os::unix::fs::symlink(root.join("elsewhere"), dir.join("calls/b.json")).unwrap();
    let e = read_tree(&dir, &scenario).err().unwrap();
    assert!(e.contains("calls/b.json is not a regular file"), "{e}");
    std::fs::remove_file(dir.join("calls/b.json")).unwrap();
    std::fs::write(dir.join("calls/b.json"), b"{}").unwrap();
    assert!(read_tree(&dir, &scenario).is_ok());
    std::fs::write(dir.join("extra"), b"").unwrap();
    let e = read_tree(&dir, &scenario).err().unwrap();
    assert!(e.contains("unexpected entry \"extra\""), "{e}");
}

#[test]
fn a_regular_read_refuses_links_specials_and_oversize() {
    let root = crate::test_scratch::scratch("pbfhogg-library-artefact", "read");
    let file = root.join("f");
    std::fs::write(&file, b"12345").unwrap();
    assert_eq!(read_regular(&file, 5).unwrap(), b"12345");
    assert!(
        read_regular(&file, 4)
            .unwrap_err()
            .contains("over the 4-byte limit")
    );
    let link = root.join("l");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    assert!(
        read_regular(&link, 5).is_err(),
        "a symlink is never followed"
    );
    assert!(
        read_regular(&root, 5)
            .unwrap_err()
            .contains("not a regular file")
    );
    // A FIFO with no writer would block an ordinary open; this one returns.
    let fifo = root.join("p");
    let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    assert!(
        read_regular(&fifo, 5)
            .unwrap_err()
            .contains("not a regular file")
    );
}

#[test]
fn a_refusal_names_its_reason() {
    assert_eq!(
        parse_refusal(br#"{"protocol": 1, "reason": "role out-a is bound twice"}"#).unwrap(),
        "role out-a is bound twice"
    );
    assert!(parse_refusal(br#"{"protocol": 1, "reason": "two\nlines"}"#).is_err());
}
