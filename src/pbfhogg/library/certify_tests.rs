#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use super::*;
use crate::pbfhogg::library::describe;

/// Two scenarios: `one` covers `cancel.v1` by a call assertion and
/// `counters.v1` by its call's stream; `two` covers `cancel.v1` by a
/// scenario-level assertion and declares `extra.v1`, which nothing requires.
const DOC: &str = r#"{"protocol": 1, "scenarios": [
  {"id": "one", "description": "d", "deadline_s": 60, "roles": [],
   "calls": [{"id": "a", "label": "l", "stream": ["counters.v1"]}],
   "assertions": [{"id": "a.cancel", "call": "a", "covers": ["cancel.v1"]},
                  {"id": "a.other", "call": "a"}]},
  {"id": "two", "description": "d", "deadline_s": 60, "roles": [],
   "calls": [{"id": "b", "label": "l"}],
   "assertions": [{"id": "whole", "call": null, "covers": ["cancel.v1", "extra.v1"]}]}
]}"#;

fn doc() -> describe::Describe {
    describe::parse(DOC.as_bytes()).unwrap()
}

fn require(ids: &[&str]) -> Requirements {
    Requirements::new(
        ids.iter()
            .map(|id| ((*id).to_owned(), format!("meaning of {id}")))
            .collect(),
    )
}

fn outcome(call: Option<&str>, id: &str, status: Status) -> (Option<String>, Outcome) {
    (
        call.map(str::to_owned),
        Outcome {
            id: id.to_owned(),
            status,
            detail: String::new(),
        },
    )
}

fn attempt<'a>(
    scenario: &'a describe::Scenario,
    results: Vec<(Option<String>, Outcome)>,
) -> Attempt<'a> {
    let passed = results.iter().all(|(_, o)| o.status == Status::Pass);
    Attempt {
        scenario,
        bindings: Vec::new(),
        passed,
        results,
        failures: Vec::new(),
        kept: None,
    }
}

fn passing<'a>(doc: &'a describe::Describe) -> Vec<Attempt<'a>> {
    vec![
        attempt(
            &doc.scenarios[0],
            vec![
                outcome(Some("a"), "a.cancel", Status::Pass),
                outcome(Some("a"), "a.other", Status::Pass),
                outcome(Some("a"), "brokkr.seal", Status::Pass),
                outcome(Some("a"), "brokkr.stream", Status::Pass),
            ],
        ),
        attempt(
            &doc.scenarios[1],
            vec![
                outcome(None, "whole", Status::Pass),
                outcome(Some("b"), "brokkr.seal", Status::Pass),
            ],
        ),
    ]
}

fn class_of(verdicts: &[Verdict], id: &str) -> Class {
    verdicts.iter().find(|v| v.id == id).unwrap().class
}

#[test]
fn every_covering_result_passing_passes_and_certifies() {
    let doc = doc();
    let attempts = passing(&doc);
    let req = require(&["cancel.v1", "counters.v1"]);
    let verdicts = classify(&req, &attempts);
    assert_eq!(class_of(&verdicts, "cancel.v1"), Class::Passed);
    assert_eq!(class_of(&verdicts, "counters.v1"), Class::Passed);
    let cancel = verdicts.iter().find(|v| v.id == "cancel.v1").unwrap();
    let names: Vec<String> = cancel.covering.iter().map(Covering::name).collect();
    assert_eq!(names, ["one/a/a.cancel", "two/whole"]);
    assert!(certified(&attempts, &verdicts));
    assert_eq!(not_required(&req, &attempts), ["extra.v1"]);
}

#[test]
fn a_failing_unevaluated_or_missing_covering_result_fails_its_capability() {
    let doc = doc();
    let req = require(&["cancel.v1", "counters.v1"]);
    for status in [Status::Fail, Status::Unevaluated] {
        let mut attempts = passing(&doc);
        attempts[1].results[0].1.status = status;
        attempts[1].passed = false;
        let verdicts = classify(&req, &attempts);
        assert_eq!(class_of(&verdicts, "cancel.v1"), Class::Failed);
        assert_eq!(class_of(&verdicts, "counters.v1"), Class::Passed);
        assert!(!certified(&attempts, &verdicts));
        let line = verdict_line(verdicts.iter().find(|v| v.id == "cancel.v1").unwrap());
        assert!(
            line.contains(&format!("two/whole {}", status.name())),
            "{line}"
        );
    }

    // A declared stream whose run reported nothing is failed, never
    // unsupported; so is everything a run that did not run declares.
    let mut attempts = passing(&doc);
    attempts[0] = Attempt::errored(&doc.scenarios[0], Vec::new(), "did not run".into());
    let verdicts = classify(&req, &attempts);
    assert_eq!(class_of(&verdicts, "counters.v1"), Class::Failed);
    assert_eq!(class_of(&verdicts, "cancel.v1"), Class::Failed);
    let line = verdict_line(verdicts.iter().find(|v| v.id == "counters.v1").unwrap());
    assert!(line.contains("one/a/brokkr.stream missing"), "{line}");

    // A duplicate result fails when any copy is not a pass.
    let mut attempts = passing(&doc);
    attempts[0]
        .results
        .push(outcome(Some("a"), "a.cancel", Status::Fail));
    let verdicts = classify(&req, &attempts);
    assert_eq!(class_of(&verdicts, "cancel.v1"), Class::Failed);
}

#[test]
fn a_result_reported_under_another_call_does_not_cover() {
    let doc = doc();
    let mut attempts = passing(&doc);
    attempts[0].results[0].0 = None;
    let verdicts = classify(&require(&["cancel.v1"]), &attempts);
    assert_eq!(class_of(&verdicts, "cancel.v1"), Class::Failed);
}

#[test]
fn a_capability_nothing_covers_is_unsupported_and_fails_certification() {
    let doc = doc();
    let attempts = passing(&doc);
    let verdicts = classify(&require(&["cancel.v1", "seal.v2"]), &attempts);
    assert_eq!(class_of(&verdicts, "seal.v2"), Class::Unsupported);
    assert!(!certified(&attempts, &verdicts));
    let line = verdict_line(verdicts.iter().find(|v| v.id == "seal.v2").unwrap());
    assert!(
        line.contains("unsupported") && !line.contains("regress"),
        "{line}"
    );
}

#[test]
fn a_failed_scenario_fails_certification_even_with_every_capability_passed() {
    let doc = doc();
    let mut attempts = passing(&doc);
    attempts[1].passed = false;
    let verdicts = classify(&require(&["cancel.v1"]), &attempts);
    assert_eq!(class_of(&verdicts, "cancel.v1"), Class::Passed);
    assert!(!certified(&attempts, &verdicts));
}

#[test]
fn an_empty_requirement_list_certifies_on_the_scenarios_alone() {
    let doc = doc();
    let attempts = passing(&doc);
    let req = require(&[]);
    let verdicts = classify(&req, &attempts);
    assert!(verdicts.is_empty());
    assert!(certified(&attempts, &verdicts));
}

#[test]
fn the_digest_covers_the_ids_in_byte_order_and_not_the_meanings() {
    let a = require(&["b.v1", "a.v1"]);
    let reworded = Requirements::new(BTreeMap::from([
        ("a.v1".to_owned(), "other words".to_owned()),
        ("b.v1".to_owned(), "more".to_owned()),
    ]));
    assert_eq!(a.digest(), reworded.digest());
    assert_eq!(
        a.digest(),
        format!(
            "require-v1:{:032x}",
            xxhash_rust::xxh3::xxh3_128(b"a.v1\nb.v1\n")
        )
    );
    assert_ne!(a.digest(), require(&["a.v1"]).digest());
    assert_eq!(
        require(&[]).digest(),
        format!("require-v1:{:032x}", xxhash_rust::xxh3::xxh3_128(b""))
    );
}

#[test]
fn the_record_holds_the_list_each_scenario_and_each_capability() {
    let doc = doc();
    let mut attempts = passing(&doc);
    attempts[0].bindings.push(Binding {
        role: "input".into(),
        variant: "indexed".into(),
        path: "/data/x.osm.pbf".into(),
        xxhash: None,
    });
    let req = require(&["cancel.v1", "seal.v2"]);
    let verdicts = classify(&req, &attempts);
    let harness = crate::config::HarnessConfig {
        package: "pbfhogg-harness".into(),
        binary: None,
        features: Vec::new(),
        debug: None,
        profile: None,
    };
    let record = record(
        &Provenance {
            uuid: "u",
            unix_time_s: 7,
            hostname: "h",
            commit: "c",
            clean: true,
            commit_ref: None,
            harness: &harness,
            profile: "release",
            harness_xxh128: "hx",
            cli_xxh128: "cx",
            dataset: "denmark",
            snapshot: "base",
        },
        &req,
        &attempts,
        &verdicts,
    );
    assert_eq!(record["record"], "brokkr-library-certification");
    assert_eq!(record["certified"], false);
    assert_eq!(record["require"]["digest"], req.digest());
    assert_eq!(
        record["require"]["capabilities"]["seal.v2"],
        "meaning of seal.v2"
    );
    assert_eq!(
        record["scenarios"][0]["bindings"][0]["xxhash"],
        serde_json::Value::Null
    );
    assert_eq!(
        record["scenarios"][1]["coverage"]["assertions"][0]["covers"][1],
        "extra.v1"
    );
    assert_eq!(record["capabilities"][1]["class"], "unsupported");
    assert_eq!(
        record["capabilities"][0]["covering"][1]["call"],
        serde_json::Value::Null
    );
    assert_eq!(
        record["not_required"],
        serde_json::json!(["counters.v1", "extra.v1"])
    );
}
