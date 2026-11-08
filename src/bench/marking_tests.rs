//! Mark requests minted by `brokkr results`' flags, appended to a local store.

#![allow(clippy::unwrap_used)]

use super::super::results::tests::run;
use super::super::store::{Mark, Marked, Store};
use super::{MarkAction, Subject, local, mint};
use crate::bench::results::BenchReader;

fn subject() -> Subject<'static> {
    Subject {
        named: "mark x (retire cat)",
        uncertain_hint: "look",
    }
}

#[test]
fn a_minted_request_is_valid_and_carries_the_action() {
    let retire = MarkAction::Retire {
        name: "cat".into(),
        reason: "superseded".into(),
    };
    let request = mint(&retire, "pbfhogg").unwrap();
    request.validate().unwrap();
    assert_eq!(
        request.mark,
        Mark::Retire {
            name: "cat".into(),
            reason: "superseded".into()
        }
    );
    assert_ne!(request.uuid, mint(&retire, "pbfhogg").unwrap().uuid);
}

#[test]
fn a_missing_store_is_an_error_and_is_not_created() {
    let path = crate::test_scratch::scratch("bench_marking", "missing").join("bench.db");
    let request = mint(&MarkAction::Unretire { name: "cat".into() }, "pbfhogg").unwrap();
    let error = local(&path, &request, &subject()).unwrap_err();
    assert!(error.to_string().contains("no bench store"), "{error}");
    assert!(!path.exists());
}

#[test]
fn retire_lands_once_replays_as_already_applied_and_refusals_are_final() {
    let path = crate::test_scratch::scratch("bench_marking", "retire").join("bench.db");
    Store::open(&path)
        .unwrap()
        .publish(&run(1, "cat", 'a', 1_000, 500))
        .unwrap();
    let retire = MarkAction::Retire {
        name: "cat".into(),
        reason: "superseded".into(),
    };
    let request = mint(&retire, "pbfhogg").unwrap();
    assert_eq!(local(&path, &request, &subject()).unwrap(), Marked::Applied);
    assert_eq!(
        local(&path, &request, &subject()).unwrap(),
        Marked::AlreadyApplied
    );
    // A second command is a new request, refused by admission.
    let again = mint(&retire, "pbfhogg").unwrap();
    let error = local(&path, &again, &subject()).unwrap_err().to_string();
    assert!(
        error.contains("was not stored") && error.contains("already retired"),
        "{error}"
    );
    let unretire = mint(&MarkAction::Unretire { name: "cat".into() }, "pbfhogg").unwrap();
    assert_eq!(
        local(&path, &unretire, &subject()).unwrap(),
        Marked::Applied
    );
}

#[test]
fn a_run_prefix_resolves_to_one_full_uuid_or_is_refused() {
    let path = crate::test_scratch::scratch("bench_marking", "resolve").join("bench.db");
    let mut store = Store::open(&path).unwrap();
    let one = run(1, "cat", 'a', 1_000, 500);
    store.publish(&one).unwrap();
    store.publish(&run(0x20, "cat", 'a', 2_000, 500)).unwrap();
    drop(store);
    let reader = BenchReader::at(&path, "pbfhogg").unwrap();

    let mut ambiguous = MarkAction::Revalidate { run: "0".into() };
    let error = ambiguous
        .resolved_by("pbfhogg", reader.as_ref())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("starts 2 runs") && error.contains(&one.uuid),
        "{error}"
    );

    let mut missing = MarkAction::Revalidate { run: "f".into() };
    let error = missing
        .resolved_by("pbfhogg", reader.as_ref())
        .unwrap_err()
        .to_string();
    assert!(error.contains("no script-bench run"), "{error}");

    let mut none = MarkAction::Revalidate { run: "0".into() };
    let error = none.resolved_by("pbfhogg", None).unwrap_err().to_string();
    assert!(error.contains("no bench store"), "{error}");

    // A retire names no run and is left as it is.
    let mut retire = MarkAction::Unretire { name: "cat".into() };
    retire.resolved_by("pbfhogg", None).unwrap();

    // The second run's uuid ends in 20, so this prefix names the first alone.
    let prefix = &one.uuid[..31];
    let mut invalidate = MarkAction::Invalidate {
        run: prefix.into(),
        reason: "noisy".into(),
    };
    invalidate.resolved_by("pbfhogg", reader.as_ref()).unwrap();
    let request = mint(&invalidate, "pbfhogg").unwrap();
    request.validate().unwrap();
    assert_eq!(
        request.mark,
        Mark::Invalidate {
            run: one.uuid.clone(),
            reason: "noisy".into()
        }
    );
    assert_eq!(local(&path, &request, &subject()).unwrap(), Marked::Applied);
    let again = mint(&invalidate, "pbfhogg").unwrap();
    let error = local(&path, &again, &subject()).unwrap_err().to_string();
    assert!(error.contains("already invalidated"), "{error}");

    let mut revalidate = MarkAction::Revalidate { run: prefix.into() };
    revalidate.resolved_by("pbfhogg", reader.as_ref()).unwrap();
    let request = mint(&revalidate, "pbfhogg").unwrap();
    assert_eq!(local(&path, &request, &subject()).unwrap(), Marked::Applied);
}
