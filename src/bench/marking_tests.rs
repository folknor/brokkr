//! Mark requests minted by `brokkr results`' flags, appended to a local store.

#![allow(clippy::unwrap_used)]

use super::super::results::tests::run;
use super::super::store::{Mark, Marked, Store};
use super::{MarkAction, Subject, local, mint};

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
