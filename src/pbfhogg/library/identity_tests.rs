#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use super::*;

fn plan(text: &str) -> Document {
    Document::of("plan", &json::parse("plan", text.as_bytes()).unwrap()).unwrap()
}

fn identity(plan_text: &str, context: &str, generated: &str) -> Identity {
    Identity {
        plan: plan(plan_text),
        contexts: BTreeMap::from([(
            "a".to_owned(),
            Document::of("context", &json::parse("c", context.as_bytes()).unwrap()).unwrap(),
        )]),
        datasets: BTreeMap::from([("input".to_owned(), "xxh128:00".to_owned())]),
        generated: BTreeMap::from([("gen".to_owned(), generated.to_owned())]),
        seeds: BTreeMap::new(),
        features: String::new(),
    }
}

#[test]
fn a_digest_is_over_the_canonical_bytes_whatever_the_spelling() {
    let a = plan(r#"{"calls": {"a": {"n": 1, "m": [true, null]}}}"#);
    let b = plan(r#"{"calls":{"a":{"m":[true,null],"n":1}}}"#);
    assert_eq!(a, b);
    assert_eq!(a.canonical, r#"{"calls":{"a":{"m":[true,null],"n":1}}}"#);
    assert_eq!(
        a.digest,
        format!(
            "plan-v1:{:032x}",
            xxhash_rust::xxh3::xxh3_128(a.canonical.as_bytes())
        )
    );
    assert_ne!(a, plan(r#"{"calls":{"a":{"m":[null,true],"n":1}}}"#));
}

#[test]
fn the_qualifier_is_canonical_and_a_disagreement_names_its_field() {
    let base = identity(r#"{"calls":{"a":{"n":1}}}"#, r#"{"p":4}"#, "absent");
    assert!(
        base.qualifier()
            .starts_with(r#"{"contexts":{"a":"context-v1:"#)
    );
    assert!(base.qualifier().ends_with(r#","v":1}"#));
    assert_eq!(base.disagreement(&base.clone()), None);
    let other = identity(r#"{"calls":{"a":{"n":2}}}"#, r#"{"p":4}"#, "absent");
    let why = other.disagreement(&base).unwrap();
    assert!(
        why.contains("the plan differs") && why.ends_with("first at calls.a.n (1 vs 2)"),
        "{why}"
    );
    let other = identity(r#"{"calls":{"a":{"n":1}}}"#, r#"{"p":8}"#, "absent");
    assert!(
        other
            .disagreement(&base)
            .unwrap()
            .contains("the context of call a differs")
    );
    let other = identity(r#"{"calls":{"a":{"n":1}}}"#, r#"{"p":4}"#, "xxh128:11");
    assert_eq!(
        other.disagreement(&base).unwrap(),
        "generated role gen differs (absent vs xxh128:11)"
    );
    assert_eq!(base.documents().len(), 2);
}

#[test]
fn seeds_enter_the_qualifier_only_when_a_role_is_seeded() {
    let base = identity(r#"{"calls":{"a":{"n":1}}}"#, r#"{"p":4}"#, "absent");
    assert!(!base.qualifier().contains("seeds"));
    let mut seeded = base.clone();
    seeded
        .seeds
        .insert("out-a".to_owned(), "seed-v1:pbf".to_owned());
    assert!(
        seeded
            .qualifier()
            .contains(r#""seeds":{"out-a":"seed-v1:pbf"}"#),
        "{}",
        seeded.qualifier()
    );
    assert!(
        seeded
            .disagreement(&base)
            .unwrap()
            .contains("seeded role out-a differs")
    );
}

#[test]
fn a_generated_tree_is_identified_by_layout_and_bytes_without_framing_collisions() {
    use std::os::unix::ffi::OsStrExt as _;
    let root = crate::test_scratch::scratch("pbfhogg-library-identity", "tree");
    let make = |name: &str, link_target: &[u8], file: &[u8]| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(link_target), dir.join("a"))
            .unwrap();
        std::fs::write(dir.join(std::ffi::OsStr::from_bytes(file)), b"same").unwrap();
        dir
    };
    // A link target holding a newline beside a file whose name holds one:
    // a newline-framed encoding of the two trees would be byte-equal.
    let a = make("a", b"x\nb", b"c");
    let b = make("b", b"x", b"b\nc");
    let ia = generated(&a, RoleKind::Directory).unwrap();
    let ib = generated(&b, RoleKind::Directory).unwrap();
    assert!(ia.starts_with("tree-v1:"));
    assert_ne!(ia, ib);

    let empty = root.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(
        generated(&empty, RoleKind::Directory)
            .unwrap()
            .starts_with("tree-v1:")
    );
    assert_eq!(
        generated(&root.join("missing"), RoleKind::File).unwrap(),
        "absent"
    );
    let file = root.join("f.pbf");
    std::fs::write(&file, b"pbf").unwrap();
    assert_eq!(
        generated(&file, RoleKind::Pbf).unwrap(),
        format!(
            "xxh128:{}",
            crate::preflight::digest::compute_file_digest(&file).unwrap()
        )
    );
    assert!(generated(&file, RoleKind::Directory).is_err());
    assert!(
        generated(&a.join("a"), RoleKind::File).is_err(),
        "a link is never followed"
    );
}
