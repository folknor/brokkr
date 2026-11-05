//! Selecting a script-bench run's child, and reading its stored evidence
//! back in the forms the views render.

#![allow(clippy::unwrap_used, clippy::panic)]

use super::select;
use crate::bench::results::tests::run;
use crate::bench::store::{
    Child, IterationOutcome, Record, RunDocument, RunOutcome, SinkEnd, StoredRun,
};
use crate::sidecar::SinkEndKind;
use crate::sidecar_fmt::Process;

fn stored(doc: RunDocument) -> StoredRun {
    StoredRun {
        best_iteration: doc.best_iteration(),
        pairing_key: doc.pairing_key(),
        config_digest: "0".repeat(32),
        children_loaded: true,
        doc,
    }
}

/// A completed run of three iterations, walls 30, 10 and 20 us, one child
/// each, so the result is iteration 1.
fn three() -> RunDocument {
    let mut doc = run(1, "inspect", 'a', 0, 30);
    let template = doc.iterations[0].clone();
    doc.iterations = [30, 10, 20]
        .into_iter()
        .map(|wall| {
            let mut it = template.clone();
            it.wall_us = Some(wall);
            it
        })
        .collect();
    doc.iterations_requested = 3;
    doc
}

fn child_with(records: Vec<Record>) -> Child {
    let mut child = run(1, "x", 'a', 0, 30).iterations[0].children[0].clone();
    child.evidence.records = records;
    child.evidence.loss.output_truncated = false;
    child
}

fn message<T>(result: Result<T, crate::error::DevError>) -> String {
    match result {
        Ok(_) => panic!("expected a refusal"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn the_default_is_the_result_else_the_last_iteration() {
    let selected = select(stored(three()), None, None).unwrap();
    assert_eq!(selected.filter(), Some(1));
    assert_eq!(selected.one(), Some((1, 0)));

    let mut failed = three();
    failed.outcome = RunOutcome::Failed { reason: "x".into() };
    failed.iterations[2].outcome = IterationOutcome::Failed { reason: "x".into() };
    let selected = select(stored(failed), None, None).unwrap();
    assert_eq!(selected.filter(), Some(2));

    let all = select(stored(three()), Some("all"), None).unwrap();
    assert_eq!(all.filter(), None);
    assert_eq!(all.one(), None);
    let err = message(select(stored(three()), Some("3"), None));
    assert!(err.contains("3 iterations"), "{err}");
}

#[test]
fn every_offending_iteration_is_named_before_anything_renders() {
    let mut doc = three();
    let extra = doc.iterations[0].children[0].clone();
    doc.iterations[0].children.push(extra);
    doc.iterations[2].children.clear();

    // Without --child, several children and none are told apart.
    let err = message(select(stored(doc.clone()), Some("all"), None));
    assert!(
        err.contains("iteration 0 launched 2 children; pass --child N"),
        "{err}"
    );
    assert!(err.contains("iteration 2 launched no child"), "{err}");
    assert!(!err.contains("iteration 1"), "{err}");

    // An explicit child every selected iteration must have.
    let err = message(select(stored(doc.clone()), Some("all"), Some(1)));
    assert!(err.contains("iteration 1 launched 1 child"), "{err}");
    assert!(err.contains("iteration 2 launched no child"), "{err}");
    assert!(!err.contains("iteration 0"), "{err}");

    // One selected iteration that launched nothing is an answer, unless a
    // child was named.
    let lone = select(stored(doc.clone()), Some("2"), None).unwrap();
    assert!(!lone.has_child());
    assert_eq!(lone.no_child_message(), "iteration 2 launched no child");
    assert!(select(stored(doc.clone()), Some("2"), Some(0)).is_err());

    // So is `--run all` over a run whose only iteration launched nothing.
    let mut single = doc.clone();
    single.iterations.truncate(1);
    single.iterations[0].children.clear();
    single.iterations_requested = 1;
    let all = select(stored(single), Some("all"), None).unwrap();
    assert!(!all.has_child());
    assert_eq!(all.no_child_message(), "iteration 0 launched no child");

    // A run that attempted nothing.
    let mut empty = doc;
    empty.outcome = RunOutcome::Failed {
        reason: "build".into(),
    };
    empty.iterations.clear();
    let none = select(stored(empty.clone()), None, None).unwrap();
    assert!(!none.has_child());
    assert!(none.no_child_message().contains("attempted no iteration"));
    assert!(select(stored(empty), None, Some(0)).is_err());
}

#[test]
fn stored_evidence_reads_back_with_its_positions_and_iteration() {
    let mut doc = three();
    doc.iterations[1].children[0] = child_with(vec![
        Record::Marker {
            timestamp_us: 5,
            call: None,
            name: "A".into(),
        },
        Record::Counter {
            timestamp_us: 6,
            call: Some("c".into()),
            name: "n".into(),
            value: 3,
        },
        Record::Terminal {
            timestamp_us: 7,
            call: "c".into(),
            end: SinkEnd::Sealed,
            drops: 17,
        },
        Record::Marker {
            timestamp_us: 8,
            call: Some("c".into()),
            name: "B".into(),
        },
    ]);
    doc.iterations[1].children[0].wall_us = 900;
    let selected = select(stored(doc), Some("1"), None).unwrap();
    let markers = selected.markers(Some(1));
    assert_eq!(
        markers
            .iter()
            .map(|m| (m.marker_idx, m.run_idx, m.name.as_str()))
            .collect::<Vec<_>>(),
        [(0, 1, "A"), (1, 1, "B")]
    );
    let counters = selected.counters(None);
    assert_eq!(counters.len(), 1);
    assert_eq!(counters[0].run_idx, 1);
    let ends = selected.sink_ends(None);
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].record_idx, 2);
    assert!(matches!(
        ends[0].kind,
        SinkEndKind::Terminal { drops: 17, .. }
    ));
    assert_eq!(selected.process(1), Process::Child { wall_us: 900 });
    selected.check_call(None, "c").unwrap();
    assert!(selected.check_call(None, "d").is_err());
}

#[test]
fn a_call_named_only_by_the_census_is_known() {
    let mut doc = three();
    let child = &mut doc.iterations[1].children[0];
    child.evidence.calls_seen.insert("dropped".into());
    let selected = select(stored(doc.clone()), None, None).unwrap();
    selected.check_call(None, "dropped").unwrap();

    // An overflowed census says absence is not proof.
    doc.iterations[1].children[0].evidence.calls_overflowed = true;
    let selected = select(stored(doc), None, None).unwrap();
    let err = message(selected.check_call(None, "other"));
    assert!(err.contains("census of call ids overflowed"), "{err}");
}
