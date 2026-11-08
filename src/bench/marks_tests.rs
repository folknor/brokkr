//! Marks against their contract: a replay is answered from the stored
//! request whatever undid it since, admission refuses what the rules refuse,
//! the fold hides and resolves as the rules say, and a pinned revision sees
//! one history.

#![allow(clippy::unwrap_used, clippy::panic)]

use super::store::{Mark, MarkRequest, Marked, Revision, RunFilter, Store, StoreError};
use super::store_tests::document;

fn store(name: &str) -> Store {
    let dir = crate::test_scratch::scratch("bench_marks", name);
    super::evidence::open_store(&dir.join("bench.db")).unwrap()
}

fn uuid(n: u32) -> String {
    format!("{n:032x}")
}

/// Publish run `n` of bench `bench`.
fn publish(store: &mut Store, n: u32, bench: &str) -> String {
    let mut doc = document(&uuid(n));
    doc.bench = bench.into();
    doc.started_unix_ms += i64::from(n);
    store.publish(&doc).unwrap();
    doc.uuid
}

fn request(n: u32, mark: Mark) -> MarkRequest {
    MarkRequest {
        uuid: format!("{:032x}", 0xf000_0000_u32 + n),
        project: "pbfhogg".into(),
        at_unix_ms: 1,
        hostname: "bygg".into(),
        mark,
    }
}

fn retire(name: &str) -> Mark {
    Mark::Retire {
        name: name.into(),
        reason: "gone".into(),
    }
}

fn rename(from: &str, to: &str) -> Mark {
    Mark::Rename {
        from: from.into(),
        to: to.into(),
    }
}

/// Apply `mark` as request `n`, which must be new.
fn apply(store: &mut Store, n: u32, mark: Mark) {
    assert_eq!(store.mark(&request(n, mark)).unwrap(), Marked::Applied);
}

fn refused(store: &mut Store, n: u32, mark: Mark, needle: &str) {
    match store.mark(&request(n, mark)) {
        Err(StoreError::Refused(why)) => assert!(why.contains(needle), "{why}"),
        other => panic!("not refused with {needle:?}: {other:?}"),
    }
}

fn filter(hidden: bool) -> RunFilter {
    RunFilter {
        project: "pbfhogg".into(),
        hidden,
        ..RunFilter::default()
    }
}

/// The visible runs, newest first.
fn visible(store: &Store) -> Vec<String> {
    store.select(&filter(false), None).unwrap().uuids
}

#[test]
fn a_replay_is_answered_from_the_stored_request_even_after_an_undo() {
    let mut store = store("replay");
    publish(&mut store, 1, "cat");
    let first = request(1, retire("cat"));
    assert_eq!(store.mark(&first).unwrap(), Marked::Applied);
    assert_eq!(store.mark(&first).unwrap(), Marked::AlreadyApplied);
    apply(&mut store, 2, Mark::Unretire { name: "cat".into() });
    // The retirement was undone, yet its request still answers its replay
    // rather than retiring again.
    assert_eq!(store.mark(&first).unwrap(), Marked::AlreadyApplied);
    assert_eq!(visible(&store).len(), 1);
    let mut changed = first.clone();
    changed.at_unix_ms = 2;
    let mut invalid = first.clone();
    invalid.hostname = String::new();
    for unequal in [changed, invalid] {
        assert_eq!(
            store.mark(&unequal),
            Err(StoreError::Conflict {
                uuid: first.uuid.clone()
            })
        );
    }
    assert_eq!(store.marks("pbfhogg").unwrap().len(), 2);
    assert!(store.marks("elivagar").unwrap().is_empty());
}

#[test]
fn retirement_and_invalidation_hide_runs_and_their_undo_shows_them() {
    let mut store = store("hide");
    let a = publish(&mut store, 1, "cat");
    let b = publish(&mut store, 2, "inspect");
    refused(&mut store, 1, retire("nothing"), "no bench named nothing");
    refused(
        &mut store,
        1,
        Mark::Retire {
            name: "cat".into(),
            reason: " ".into(),
        },
        "reason",
    );
    refused(
        &mut store,
        1,
        Mark::Unretire { name: "cat".into() },
        "not retired",
    );
    apply(&mut store, 1, retire("cat"));
    refused(&mut store, 2, retire("cat"), "already retired");
    assert_eq!(visible(&store), vec![b.clone()]);
    // Hidden runs are still selectable on request, and a later run under a
    // retired name is retired too.
    let late = publish(&mut store, 3, "cat");
    assert_eq!(
        store.select(&filter(true), None).unwrap().uuids,
        vec![late.clone(), b.clone(), a.clone()]
    );
    assert_eq!(visible(&store), vec![b.clone()]);

    let invalidate = |run: &str| Mark::Invalidate {
        run: run.into(),
        reason: "noisy".into(),
    };
    refused(&mut store, 2, invalidate(&uuid(99)), "no run");
    refused(&mut store, 2, invalidate("ab"), "full run uuid");
    apply(&mut store, 2, invalidate(&b));
    refused(&mut store, 3, invalidate(&b), "already invalidated");
    assert!(visible(&store).is_empty());
    refused(
        &mut store,
        3,
        Mark::Revalidate { run: a.clone() },
        "not invalidated",
    );
    apply(&mut store, 3, Mark::Revalidate { run: b.clone() });
    apply(&mut store, 4, Mark::Unretire { name: "cat".into() });
    assert_eq!(visible(&store), vec![late, b, a]);
    let view = store.select(&filter(false), None).unwrap().marks;
    assert!(view.retired.is_empty() && view.invalidated.is_empty());
}

#[test]
fn renames_chain_to_their_end_and_refuse_a_merge() {
    let mut store = store("rename");
    let a = publish(&mut store, 1, "a");
    publish(&mut store, 2, "other");
    refused(
        &mut store,
        1,
        rename("nothing", "x"),
        "no bench named nothing",
    );
    refused(&mut store, 1, rename("a", "other"), "would merge");
    apply(&mut store, 1, rename("a", "b"));
    refused(&mut store, 2, rename("a", "c"), "already renamed to b");
    refused(&mut store, 2, rename("other", "b"), "would merge");
    let b = publish(&mut store, 3, "b");
    apply(&mut store, 2, rename("b", "c"));
    let selection = store.select(&filter(false), None).unwrap();
    assert_eq!(selection.marks.name("a"), "c");
    assert_eq!(selection.marks.name("b"), "c");
    // The bench filter matches the resolved name, not the stored one.
    let by_name = |name: &str| {
        store
            .select(
                &RunFilter {
                    bench: Some(name.into()),
                    ..filter(false)
                },
                None,
            )
            .unwrap()
            .uuids
    };
    assert_eq!(by_name("C"), vec![b.clone(), a.clone()]);
    assert!(by_name("a").is_empty());
    // Retiring a renamed name retires its end.
    apply(&mut store, 3, retire("a"));
    assert!(selection.marks.retired.is_empty());
    let view = store.select(&filter(false), None).unwrap().marks;
    assert!(view.retired.contains_key("c"));
    assert_eq!(visible(&store).len(), 1);
}

/// The undo cancels the whole path, and every name it leaves an end keeps
/// the retirement: a rename never changes a run's visibility.
#[test]
fn renaming_back_cancels_the_path_and_keeps_every_run_hidden() {
    let mut store = store("undo");
    publish(&mut store, 1, "a");
    apply(&mut store, 1, rename("a", "b"));
    publish(&mut store, 2, "b");
    apply(&mut store, 2, rename("b", "c"));
    publish(&mut store, 3, "c");
    apply(&mut store, 3, rename("c", "d"));
    publish(&mut store, 4, "d");
    apply(&mut store, 4, retire("d"));
    assert!(visible(&store).is_empty());
    apply(&mut store, 5, rename("d", "a"));
    let view = store.select(&filter(false), None).unwrap().marks;
    assert!(view.renames.is_empty(), "{:?}", view.renames);
    for name in ["a", "b", "c", "d"] {
        assert!(
            view.retired.contains_key(name),
            "{name} lost its retirement"
        );
    }
    assert!(visible(&store).is_empty());
    // Each name is its own bench again, unretired one at a time.
    apply(&mut store, 6, Mark::Unretire { name: "b".into() });
    assert_eq!(visible(&store), vec![uuid(2)]);
}

#[test]
fn a_pinned_revision_sees_one_history_and_a_future_one_is_refused() {
    let mut store = store("revision");
    let a = publish(&mut store, 1, "cat");
    let first = store.select(&filter(false), None).unwrap();
    assert_eq!(first.revision, Revision { marks: 0, runs: 1 });
    publish(&mut store, 2, "cat");
    apply(&mut store, 1, retire("cat"));
    let pinned = store.select(&filter(false), Some(first.revision)).unwrap();
    assert_eq!(pinned, first);
    assert_eq!(pinned.uuids, vec![a]);
    assert!(visible(&store).is_empty());
    match store.select(&filter(false), Some(Revision { marks: 9, runs: 1 })) {
        Err(StoreError::Refused(why)) => assert!(why.contains("not one this store"), "{why}"),
        other => panic!("a future revision was taken: {other:?}"),
    }
}

#[test]
fn a_comparison_key_resolves_the_bench_and_keeps_the_algorithm() {
    let mut store = store("key");
    let doc = document(&uuid(1));
    store.publish(&doc).unwrap();
    apply(&mut store, 1, rename("cat", "concat"));
    let view = store.select(&filter(false), None).unwrap().marks;
    let mut renamed = doc.clone();
    renamed.bench = "concat".into();
    assert_eq!(
        view.comparison_key(&doc.pairing_key()),
        renamed.pairing_key()
    );
    let v2 = r#"["bench-pair-v2","hotpath","pbfhogg","cat",[]]"#;
    assert_eq!(
        view.comparison_key(v2),
        r#"["bench-pair-v2","hotpath","pbfhogg","concat",[]]"#
    );
    assert_eq!(
        view.comparison_key("[\"future\",\"cat\"]"),
        "[\"future\",\"cat\"]"
    );
    assert_eq!(view.comparison_key("not json"), "not json");
}
