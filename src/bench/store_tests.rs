//! The bench store against its contract: a document reads back as published,
//! a replay is answered and a conflicting one refused, every refusal the
//! evidence model owes, and the schema's own guards. Outside the store
//! module, which stays includable without brokkr's test helpers.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use super::evidence;
use super::store::{
    BoundParam, Build, CacheFile, CacheMode, CachePreparation, CaptureOutcome, Child, ChildEnding,
    Clock, Conditions, DOCUMENT_FORMAT, Evidence, Executable, ExecutableIdentity, HotpathCapture,
    Iteration, IterationOutcome, Loss, Measurement, ParamValue, Published, Record, ResolvedBuild,
    RunDocument, RunOutcome, STDERR_RUN_BYTES, STDERR_TAIL_BYTES, Sample, SinkEnd, StderrCapture,
    StderrTail, Store, StoreError,
};

fn store(name: &str) -> Store {
    let dir = crate::test_scratch::scratch("bench_store", name);
    evidence::open_store(&dir.join("brokkr").join("bench.db")).unwrap()
}

/// The store sits beside history.db under the data home, whatever this
/// host's data home is; the environment is read, never changed. A test
/// publishes to a scratch store instead.
#[test]
fn the_store_lives_in_the_per_user_data_home() {
    assert_ne!(
        evidence::store_path().ok(),
        evidence::user_store_path().ok(),
        "a test must never reach the user's bench.db"
    );
    match (
        evidence::user_store_path(),
        crate::user_dirs::xdg_data_home(),
    ) {
        (Ok(path), Some(home)) => assert_eq!(path, home.join("brokkr").join("bench.db")),
        (Err(_), None) => {}
        (found, home) => panic!("store path {found:?} disagrees with data home {home:?}"),
    }
}

fn sample(timestamp_us: i64) -> Sample {
    Sample {
        timestamp_us,
        rss_kb: 1,
        anon_kb: 2,
        file_kb: 3,
        shmem_kb: 4,
        swap_kb: 5,
        vsize_kb: 6,
        vm_hwm_kb: 7,
        utime: 8,
        stime: 9,
        num_threads: 10,
        minflt: 11,
        majflt: 12,
        rchar: 13,
        wchar: 14,
        read_bytes: 15,
        write_bytes: 16,
        cancelled_write_bytes: 17,
        syscr: 18,
        syscw: 19,
        vol_cs: 20,
        nonvol_cs: i64::MAX,
    }
}

/// A child with every record kind, an off-contract stamp and a census.
fn child(offset: i64, ending: ChildEnding) -> Child {
    Child {
        program: "pbfhogg".into(),
        argv: vec!["cat".into(), "--jobs".into(), "4".into()],
        env: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
        pid: Some(4242),
        origin_offset_us: offset,
        wall_us: 1_500,
        ending,
        evidence: Evidence {
            samples: vec![sample(100), sample(200)],
            records: vec![
                Record::Marker {
                    timestamp_us: -5,
                    call: None,
                    name: "start".into(),
                },
                Record::Counter {
                    timestamp_us: 120,
                    call: Some("c1".into()),
                    name: "nodes".into(),
                    value: -3,
                },
                Record::Sealing {
                    timestamp_us: 130,
                    call: "c1".into(),
                    name: "blobs".into(),
                    value: 9,
                },
                Record::Terminal {
                    timestamp_us: 140,
                    call: "c1".into(),
                    end: SinkEnd::Failed,
                    drops: u64::MAX,
                },
                Record::Marker {
                    timestamp_us: 150,
                    call: Some("c1".into()),
                    name: "end".into(),
                },
            ],
            loss: Loss {
                rejected_lines: 2,
                fifo_unread: true,
                records_capped: false,
                samples_capped: true,
                output_truncated: false,
            },
            clock: Clock {
                outliers: 1,
                min_us: Some(-5),
            },
            calls_seen: BTreeSet::from(["c1".to_owned(), "c2".to_owned()]),
            calls_overflowed: true,
        },
        hotpath: None,
        executable: None,
        stderr: Some(StderrCapture {
            released: true,
            bytes: Some(70_000),
            tail: StderrTail::Kept {
                text: "partial\nerror: out of space\n".into(),
                raw: 28,
                cut: true,
            },
        }),
        measured: false,
        cache: None,
    }
}

/// `child` marked with stage `decode`, its `occurrence`, and `outcome`.
fn marked(offset: i64, occurrence: u32, outcome: CaptureOutcome) -> Child {
    let mut child = child(offset, ChildEnding::Exited { code: 0 });
    child.hotpath = Some(HotpathCapture {
        stage: "decode".into(),
        occurrence,
        outcome,
    });
    child
}

fn captured() -> CaptureOutcome {
    CaptureOutcome::Captured {
        report: serde_json::json!({"functions_timing": {"data": [{"name": "f", "calls": 2}]}}),
    }
}

fn completed(wall_us: i64, verdict: Option<&str>) -> Iteration {
    let mut record = serde_json::Map::new();
    record.insert("rate".into(), serde_json::json!(0.1));
    record.insert("nested".into(), serde_json::json!({"a": [1, "x", null]}));
    Iteration {
        outcome: IterationOutcome::Completed {
            verdict: verdict.map(str::to_owned),
        },
        wall_us: Some(wall_us),
        record: Some(record),
        children: vec![
            child(10, ChildEnding::Exited { code: 1 }),
            child(
                900,
                ChildEnding::Deadline {
                    secs: u64::MAX,
                    nanos: 999_999_999,
                },
            ),
        ],
    }
}

fn param(many: bool, from: Option<&str>, values: &[(&str, Option<&str>)]) -> BoundParam {
    BoundParam {
        many,
        from: from.map(str::to_owned),
        values: values
            .iter()
            .map(|(key, digest)| ParamValue {
                key: (*key).to_owned(),
                digest: digest.map(str::to_owned),
            })
            .collect(),
    }
}

/// A completed run of three iterations, every shape the model holds.
pub(super) fn document(uuid: &str) -> RunDocument {
    RunDocument {
        format: DOCUMENT_FORMAT,
        uuid: uuid.into(),
        project: "pbfhogg".into(),
        bench: "cat".into(),
        checkout: "/src/pbfhogg".into(),
        hostname: "bygg".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        dirty: false,
        subject: Some("feat: something".into()),
        started_unix_ms: 1_762_000_000_000,
        brokkr_version: "0.1.0".into(),
        argv: vec!["brokkr".into(), "bench".into(), "cat".into()],
        conditions: Conditions {
            kernel: Some("7.3.0".into()),
            cpu_governor: None,
            avail_memory_mb: Some(64_000),
            captured_env: BTreeMap::from([("MALLOC_ARENA_MAX".into(), "2".into())]),
        },
        build: Build {
            target: None,
            profile: "release".into(),
            features: vec!["hotpath".into()],
            default_features: true,
            resolved: Some(ResolvedBuild {
                bin: "pbfhogg".into(),
                features: vec!["default".into(), "hotpath".into()],
            }),
            artifact: Some("00112233445566778899aabbccddeeff".into()),
        },
        measurement: Measurement::Timing,
        params: BTreeMap::from([
            (
                "dataset".into(),
                param(false, Some("datasets"), &[("denmark", None)]),
            ),
            (
                "osc".into(),
                param(
                    true,
                    Some("datasets.denmark.osc"),
                    &[("12", Some("aa")), ("10", Some("bb"))],
                ),
            ),
            ("none".into(), param(true, Some("data.empty"), &[])),
            ("label".into(), param(false, None, &[("", None)])),
        ]),
        config: "[datasets.denmark]\npbf = \"x\"\n".into(),
        iterations_requested: 3,
        outcome: RunOutcome::Completed,
        iterations: vec![
            completed(300, Some("")),
            completed(200, Some("")),
            completed(200, Some("")),
        ],
    }
}

#[test]
fn a_published_run_reads_back_as_the_same_document() {
    let mut store = store("round_trip");
    let doc = document("0123456789abcdef0123456789abcdef");
    assert_eq!(store.publish(&doc).unwrap(), Published::Stored);
    assert_eq!(store.load(&doc.uuid).unwrap().as_ref(), Some(&doc));
    assert_eq!(
        store.load("ffffffffffffffffffffffffffffffff").unwrap(),
        None
    );
    // The first fastest iteration is the result.
    let best: i64 = store
        .conn()
        .query_row("SELECT best_iteration FROM runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(best, 1);
}

#[test]
fn a_replay_is_answered_and_a_different_document_under_its_uuid_refused() {
    let mut store = store("replay");
    let doc = document("00000000000000000000000000000001");
    store.publish(&doc).unwrap();
    assert_eq!(store.publish(&doc).unwrap(), Published::AlreadyStored);
    let mut changed = doc.clone();
    changed.iterations[0].children[0].evidence.samples[0].rss_kb += 1;
    let refused = store.publish(&changed).unwrap_err();
    assert_eq!(
        refused,
        StoreError::Conflict {
            uuid: doc.uuid.clone()
        }
    );
    assert!(!refused.outcome_uncertain());
    let runs: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(runs, 1);
}

#[test]
fn a_config_text_is_stored_once_and_failed_attempts_keep_their_evidence() {
    let mut store = store("attempts");
    let mut build_failed = document("00000000000000000000000000000002");
    build_failed.outcome = RunOutcome::Failed {
        reason: "cargo build exited 101".into(),
    };
    build_failed.build.artifact = None;
    build_failed.iterations.clear();
    store.publish(&build_failed).unwrap();

    let mut interrupted = document("00000000000000000000000000000003");
    interrupted.outcome = RunOutcome::Interrupted;
    interrupted.iterations = vec![
        completed(10, None),
        Iteration {
            outcome: IterationOutcome::Interrupted,
            wall_us: None,
            record: None,
            children: Vec::new(),
        },
    ];
    store.publish(&interrupted).unwrap();

    // Completed iterations that disagree fail the run, and are kept.
    let mut disagreed = document("00000000000000000000000000000004");
    disagreed.outcome = RunOutcome::Failed {
        reason: "verdicts disagree".into(),
    };
    disagreed.iterations[2] = completed(200, None);
    store.publish(&disagreed).unwrap();

    for doc in [&build_failed, &interrupted, &disagreed] {
        assert_eq!(store.load(&doc.uuid).unwrap().as_ref(), Some(doc));
    }
    let (configs, results): (i64, i64) = store
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM configs),
                    (SELECT COUNT(*) FROM runs WHERE best_iteration IS NOT NULL)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((configs, results), (1, 0));
}

fn refused(doc: &RunDocument, needle: &str) {
    match doc.validate() {
        Err(StoreError::Refused(why)) => assert!(why.contains(needle), "{why}"),
        other => panic!("expected a refusal naming {needle:?}, got {other:?}"),
    }
}

/// A stderr capture whose tail, size and cut disagree, or that keeps more
/// than a tail or a run may, is refused; one with none of that publishes and
/// reads back.
#[test]
fn a_stderr_capture_must_be_consistent_and_within_bounds() {
    let base = document("00000000000000000000000000000010");
    let mut store_ = store("stderr-capture");
    store_.publish(&base).unwrap();
    assert_eq!(store_.load(&base.uuid).unwrap(), Some(base.clone()));
    let with = |tail: StderrTail, bytes: Option<u64>| {
        let mut doc = base.clone();
        doc.iterations[0].children[0].stderr = Some(StderrCapture {
            released: true,
            bytes,
            tail,
        });
        doc
    };
    let kept = |text: &str, raw: u64, cut: bool| StderrTail::Kept {
        text: text.into(),
        raw,
        cut,
    };
    refused(&with(kept("x", 1, false), None), "no observed size");
    refused(&with(kept("x", 1, false), Some(0)), "disagrees");
    refused(&with(kept("x", 1, false), Some(5)), "disagrees");
    refused(&with(kept("xxxx", 1, true), Some(5)), "cannot hold");
    let big = STDERR_TAIL_BYTES as u64 + 1;
    refused(
        &with(
            kept(&"x".repeat(STDERR_TAIL_BYTES + 1), big, true),
            Some(big + 1),
        ),
        "past the",
    );
    refused(
        &with(
            StderrTail::Unreadable {
                reason: String::new(),
            },
            None,
        ),
        "empty reason",
    );
    with(StderrTail::OverBudget, None).validate().unwrap();
    // Past the run's budget across children.
    let mut wide = base.clone();
    let tail = "x".repeat(STDERR_TAIL_BYTES);
    let raw = STDERR_TAIL_BYTES as u64;
    let one = wide.iterations[0].children[0].clone();
    wide.iterations[0].children = (0..=STDERR_RUN_BYTES / STDERR_TAIL_BYTES)
        .map(|_| {
            let mut child = one.clone();
            child.stderr = Some(StderrCapture {
                released: true,
                bytes: Some(raw),
                tail: kept(&tail, raw, false),
            });
            child
        })
        .collect();
    refused(&wide, "a run keeps");
}

#[test]
fn a_completed_outcome_its_iterations_do_not_support_is_refused() {
    let base = document("00000000000000000000000000000005");
    base.validate().unwrap();

    let mut nil_and_empty = base.clone();
    nil_and_empty.iterations[1] = completed(200, None);
    refused(&nil_and_empty, "disagree on the verdict");

    let mut short = base.clone();
    short.iterations.pop();
    refused(&short, "holds 2 of 3");

    let mut failed_inside = base.clone();
    failed_inside.iterations[0].outcome = IterationOutcome::Failed {
        reason: "exit 2".into(),
    };
    refused(&failed_inside, "did not complete");

    let mut unbuilt = base.clone();
    unbuilt.build.artifact = None;
    refused(&unbuilt, "without a built artifact");

    let mut no_record = base.clone();
    no_record.iterations[0].record = None;
    refused(&no_record, "without an extracted record");

    let mut too_many = base.clone();
    too_many.outcome = RunOutcome::Interrupted;
    too_many.iterations.push(completed(1, Some("")));
    refused(&too_many, "4 iterations stored where 3");
}

#[test]
fn malformed_identity_params_and_evidence_are_refused() {
    let base = document("00000000000000000000000000000006");
    let mut doc = base.clone();
    doc.uuid = "ABC".into();
    refused(&doc, "run uuid");

    let mut doc = base.clone();
    doc.commit = "0123456".into();
    refused(&doc, "full lowercase hex sha");

    let mut doc = base.clone();
    doc.params
        .insert("dataset".into(), param(false, Some("datasets"), &[]));
    refused(&doc, "scalar param dataset has 0 values");

    let mut doc = base.clone();
    doc.params.insert(
        "osc".into(),
        param(true, Some("x"), &[("1", None), ("1", None)]),
    );
    refused(&doc, "binds \"1\" twice");

    let mut doc = base.clone();
    doc.params
        .insert("label".into(), param(false, None, &[("a", Some("d"))]));
    refused(&doc, "carries a digest");

    let mut doc = base.clone();
    doc.build.resolved = None;
    refused(&doc, "artifact without a resolved build");

    let mut doc = base.clone();
    doc.iterations[0].children[0].origin_offset_us = -1;
    refused(&doc, "negative origin offset");

    let mut doc = base.clone();
    doc.iterations[0].children[0].evidence.clock.min_us = None;
    refused(&doc, "clock outlier count");

    // What the store could not give back as it came is refused up front.
    let mut doc = base.clone();
    doc.iterations[0].children[0].evidence.loss.rejected_lines = u64::MAX;
    refused(&doc, "past the store's range");

    let mut doc = base.clone();
    doc.iterations[0].children[0].evidence.clock.outliers = u64::MAX;
    refused(&doc, "past the store's range");

    let mut doc = base.clone();
    let mut deep = serde_json::json!(1);
    for _ in 0..70 {
        deep = serde_json::json!([deep]);
    }
    doc.iterations[0]
        .record
        .as_mut()
        .unwrap()
        .insert("deep".into(), deep);
    refused(&doc, "nests deeper");

    let mut doc = base.clone();
    doc.iterations[0].children[0].ending = ChildEnding::Deadline {
        secs: 0,
        nanos: 1_000_000_000,
    };
    refused(&doc, "nanoseconds past a second");

    let mut doc = base;
    doc.format = DOCUMENT_FORMAT + 1;
    refused(&doc, "document format");
}

#[test]
fn a_document_with_a_field_this_store_does_not_know_does_not_parse() {
    let doc = document("00000000000000000000000000000007");
    let mut value = serde_json::to_value(&doc).unwrap();
    assert_eq!(
        serde_json::from_value::<RunDocument>(value.clone()).unwrap(),
        doc
    );
    let parses = |patch: &dyn Fn(&mut serde_json::Value)| {
        let mut value = value.clone();
        patch(&mut value);
        serde_json::from_value::<RunDocument>(value).is_ok()
    };
    assert!(!parses(&|v| {
        v["iterations"][0]["children"][0]["evidence"]["spans"] = serde_json::json!([]);
    }));
    // A unit variant carries no fields to drop, and a variant with fields
    // refuses one it does not know.
    assert!(!parses(&|v| {
        v["outcome"] = serde_json::json!({"interrupted": {"cause": "x"}});
    }));
    assert!(parses(&|v| v["outcome"] = serde_json::json!("interrupted")));
    assert!(!parses(&|v| {
        v["outcome"] = serde_json::json!({"failed": {"reason": "r", "stage": "build"}});
    }));
    value["iterations"][0]["outcome"] = serde_json::json!({"completed": {"verdict": null}});
    assert!(serde_json::from_value::<RunDocument>(value).is_ok());
}

#[test]
fn the_pairing_key_keeps_collection_order_and_digests_and_ignores_the_rest() {
    let base = document("00000000000000000000000000000008");
    let key = base.pairing_key();

    let mut elsewhere = base.clone();
    elsewhere.uuid = "00000000000000000000000000000009".into();
    elsewhere.commit = "f".repeat(40);
    elsewhere.config = "other".into();
    elsewhere.iterations[0].record = None;
    assert_eq!(elsewhere.pairing_key(), key);

    let mut reordered = base.clone();
    reordered.params.get_mut("osc").unwrap().values.reverse();
    assert_ne!(reordered.pairing_key(), key);

    let mut redownloaded = base.clone();
    redownloaded.params.get_mut("osc").unwrap().values[0].digest = Some("cc".into());
    assert_ne!(redownloaded.pairing_key(), key);

    let mut unbound = base;
    unbound.params.remove("none");
    assert_ne!(unbound.pairing_key(), key);
}

/// `document` as a `--hotpath` run: each iteration's first child captured
/// stage `decode` once.
fn profiled(uuid: &str) -> RunDocument {
    let mut doc = document(uuid);
    doc.measurement = Measurement::Hotpath;
    for iteration in &mut doc.iterations {
        iteration.children[0] = marked(10, 1, captured());
    }
    doc
}

#[test]
fn captures_and_the_measurement_read_back_as_published() {
    let mut store = store("captures");
    let doc = profiled("0000000000000000000000000000000a");
    store.publish(&doc).unwrap();
    assert_eq!(store.load(&doc.uuid).unwrap().as_ref(), Some(&doc));

    // Every outcome, in a timing run that fails, where none is required.
    let mut timing = document("0000000000000000000000000000000b");
    timing.outcome = RunOutcome::Failed {
        reason: "later".into(),
    };
    timing.iterations[0].children = vec![
        marked(1, 1, CaptureOutcome::Missing),
        marked(
            2,
            2,
            CaptureOutcome::Invalid {
                reason: "not JSON".into(),
            },
        ),
        marked(
            3,
            3,
            CaptureOutcome::Unreadable {
                reason: "gone".into(),
            },
        ),
        marked(4, 4, CaptureOutcome::OverBudget),
    ];
    store.publish(&timing).unwrap();
    assert_eq!(store.load(&timing.uuid).unwrap().as_ref(), Some(&timing));

    // An alloc run is stored as a hotpath run flagged alloc, and reads back
    // as alloc.
    let mut alloc = profiled("00000000000000000000000000000011");
    alloc.measurement = Measurement::Alloc;
    store.publish(&alloc).unwrap();
    assert_eq!(store.load(&alloc.uuid).unwrap().as_ref(), Some(&alloc));
    let columns: (String, bool) = store
        .conn()
        .query_row(
            "SELECT measurement, alloc FROM runs WHERE uuid = ?1",
            [&alloc.uuid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(columns, ("hotpath".to_owned(), true));
    assert!(
        store
            .conn()
            .execute(
                "UPDATE runs SET measurement = 'timing' WHERE uuid = ?1",
                [&alloc.uuid]
            )
            .is_err(),
        "an alloc timing run is refused"
    );
}

/// Timing keeps `bench-pair-v1` as it was; a profiled run and a timing run
/// that captured anyway each pair in a class of their own.
#[test]
fn the_pairing_class_separates_profiled_and_incidental_runs() {
    let timing = document("0000000000000000000000000000000c");
    let key = timing.pairing_key();
    assert!(
        key.starts_with("[\"bench-pair-v1\",\"pbfhogg\",\"cat\","),
        "{key}"
    );

    let mut uncaptured = timing.clone();
    uncaptured.iterations[0].children[0] = marked(10, 1, CaptureOutcome::Missing);
    assert_eq!(uncaptured.pairing_key(), key, "nothing captured is timing");

    let hotpath = profiled("0000000000000000000000000000000d").pairing_key();
    assert!(
        hotpath.starts_with("[\"bench-pair-v2\",\"hotpath\","),
        "{hotpath}"
    );

    let mut incidental = timing.clone();
    incidental.outcome = RunOutcome::Failed { reason: "x".into() };
    incidental.iterations[2].children[0] = marked(10, 1, captured());
    let incidental = incidental.pairing_key();
    assert!(
        incidental.starts_with("[\"bench-pair-v2\",\"incidental\","),
        "{incidental}"
    );
    assert_ne!(incidental, hotpath);

    let mut alloc = profiled("0000000000000000000000000000000d");
    alloc.measurement = Measurement::Alloc;
    let alloc = alloc.pairing_key();
    assert!(
        alloc.starts_with("[\"bench-pair-v2\",\"alloc\","),
        "{alloc}"
    );
    assert_ne!(alloc, hotpath);
}

/// `document` with child 1 of each iteration marked measured, at these walls.
fn child_measured(uuid: &str, walls: [i64; 3]) -> RunDocument {
    let mut doc = document(uuid);
    for (iteration, wall) in doc.iterations.iter_mut().zip(walls) {
        iteration.children[1].measured = true;
        iteration.children[1].wall_us = wall;
    }
    doc
}

/// A run that marked a measured call ranks on that child's wall, reads back
/// with its marks, and pairs apart from one ranked on the block's wall.
#[test]
fn a_measured_child_is_the_result_and_pairs_apart() {
    let mut store = store("measured_child");
    let doc = child_measured("0000000000000000000000000000001a", [500, 900, 400]);
    assert_eq!(document(&doc.uuid).best_iteration(), Some(1));
    assert_eq!(doc.best_iteration(), Some(2), "ranked on the child's wall");
    assert_eq!(doc.result_wall_us(), Some(400));
    assert!(doc.child_measured());
    assert_eq!(store.publish(&doc).unwrap(), Published::Stored);
    assert_eq!(store.load(&doc.uuid).unwrap().as_ref(), Some(&doc));
    let best: i64 = store
        .conn()
        .query_row("SELECT best_iteration FROM runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(best, 2);

    let key = doc.pairing_key();
    assert!(
        key.starts_with("[\"bench-pair-v3\",\"timing\",\"measured-child\","),
        "{key}"
    );
    assert_ne!(key, document(&doc.uuid).pairing_key());
}

/// Two measured children in one iteration, or a completed run whose
/// iterations disagree on marking one, are refused; a failed run may stop
/// before its later iterations mark theirs.
#[test]
fn a_measured_child_must_be_one_and_agreed_on() {
    let mut twice = child_measured("0000000000000000000000000000001b", [1, 2, 3]);
    twice.iterations[0].children[0].measured = true;
    refused(&twice, "more than one measured child");

    let mut partial = child_measured("0000000000000000000000000000001c", [1, 2, 3]);
    partial.iterations[2].children[1].measured = false;
    refused(&partial, "disagree on a measured call");
    partial.outcome = RunOutcome::Failed { reason: "x".into() };
    partial.validate().unwrap();
}

#[test]
fn a_profiled_run_must_capture_every_marked_call() {
    profiled("0000000000000000000000000000000e")
        .validate()
        .unwrap();

    let mut none_marked = profiled("0000000000000000000000000000000e");
    none_marked.iterations[1].children[0].hotpath = None;
    refused(&none_marked, "did not capture every marked call");

    let mut missing = profiled("0000000000000000000000000000000e");
    missing.iterations[0].children[1] = marked(900, 2, CaptureOutcome::Missing);
    refused(&missing, "did not capture every marked call");

    // A failed profiled run keeps what it got.
    missing.outcome = RunOutcome::Failed {
        reason: "--hotpath".into(),
    };
    missing.validate().unwrap();

    let mut gap = document("0000000000000000000000000000000e");
    gap.iterations[0].children[1] = marked(900, 2, captured());
    refused(&gap, "occurrence 2 where 1 follows");

    let mut blank = document("0000000000000000000000000000000e");
    blank.iterations[0].children[0] = marked(10, 1, captured());
    blank.iterations[0].children[0]
        .hotpath
        .as_mut()
        .unwrap()
        .stage = String::new();
    refused(&blank, "empty stage");

    let mut scalar = document("0000000000000000000000000000000e");
    scalar.iterations[0].children[0] = marked(
        10,
        1,
        CaptureOutcome::Captured {
            report: serde_json::json!([1]),
        },
    );
    refused(&scalar, "not a JSON object");
}

/// A store made before captures existed upgrades in place: its runs read
/// back as timing runs whose children captured nothing.
#[test]
fn a_version_one_store_upgrades_and_its_runs_read_as_timing() {
    let dir = crate::test_scratch::scratch("bench_store", "upgrade");
    let path = dir.join("bench.db");
    let mut doc = document("0000000000000000000000000000000f");
    // A version one store held no stderr.
    for child in doc.iterations.iter_mut().flat_map(|i| &mut i.children) {
        child.stderr = None;
    }
    {
        let mut store = Store::open(&path).unwrap();
        store.publish(&doc).unwrap();
        // Back to version 1 as it shipped: none of the later tables or the
        // column.
        store
            .conn()
            .execute_batch(
                "DROP TABLE child_cache_path;
                 DROP TABLE child_cache_file;
                 DROP TABLE child_cache;
                 ALTER TABLE children DROP COLUMN measured;
                 DROP TABLE child_stderr;
                 DROP TABLE marks;
                 DROP TABLE child_executable;
                 DROP TABLE child_hotpath;
                 ALTER TABLE runs DROP COLUMN alloc;
                 ALTER TABLE runs DROP COLUMN measurement;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let version: i64 = store
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 8);
    assert!(store.marks(&doc.project).unwrap().is_empty());
    assert_eq!(store.load(&doc.uuid).unwrap(), Some(doc));
}

/// A `program` child's executable reads back as published, a digest and an
/// unread one alike; one that names no absolute path, a malformed digest or
/// an empty reason is refused. A document without the field encodes without
/// it, as it did before the field existed.
#[test]
fn an_external_executable_reads_back_and_a_malformed_one_is_refused() {
    let mut store = store("executables");
    let mut doc = document("00000000000000000000000000000010");
    doc.iterations[0].children[0].executable = Some(Executable {
        path: "/usr/bin/lua5.4".into(),
        identity: ExecutableIdentity::Digest {
            digest: "00112233445566778899aabbccddeeff".into(),
        },
    });
    doc.iterations[0].children[1].executable = Some(Executable {
        path: "/opt/x".into(),
        identity: ExecutableIdentity::Unread {
            reason: "permission denied".into(),
        },
    });
    store.publish(&doc).unwrap();
    assert_eq!(store.load(&doc.uuid).unwrap().as_ref(), Some(&doc));

    let plain = serde_json::to_string(&document("00000000000000000000000000000011")).unwrap();
    assert!(!plain.contains("executable"), "{plain}");

    let with = |executable: Executable| {
        let mut doc = document("00000000000000000000000000000012");
        doc.iterations[0].children[0].executable = Some(executable);
        doc
    };
    refused(
        &with(Executable {
            path: "lua5.4".into(),
            identity: ExecutableIdentity::Digest {
                digest: "00112233445566778899aabbccddeeff".into(),
            },
        }),
        "not absolute",
    );
    refused(
        &with(Executable {
            path: "/x".into(),
            identity: ExecutableIdentity::Digest {
                digest: "XY".into(),
            },
        }),
        "not 32 lowercase hex",
    );
    refused(
        &with(Executable {
            path: "/x".into(),
            identity: ExecutableIdentity::Unread {
                reason: String::new(),
            },
        }),
        "empty reason",
    );
}

#[test]
fn a_cache_preparation_reads_back_and_an_unestablished_one_is_refused() {
    let file = |mode, bytes, resident, paths: &[&str]| CacheFile {
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        mode,
        bytes,
        resident,
    };
    let prepared = |files: Vec<CacheFile>| CachePreparation {
        procedure: "page-cache-v1".into(),
        scan_from_us: 0,
        scan_to_us: 0,
        files,
    };
    let mut store = store("cache");
    let mut doc = document("00000000000000000000000000000020");
    doc.iterations[0].children[0].cache = Some(prepared(vec![
        file(CacheMode::Cold, 10, 0, &["/d/a", "/d/alias"]),
        file(CacheMode::Warm, 4097, 4097, &["/d/b"]),
    ]));
    store.publish(&doc).unwrap();
    assert_eq!(store.load(&doc.uuid).unwrap().as_ref(), Some(&doc));
    // A stored account its mode rules out never reads as established.
    store
        .conn()
        .execute_batch("UPDATE child_cache_file SET resident = 1 WHERE mode = 'cold'")
        .unwrap();
    assert!(store.load(&doc.uuid).is_err());

    let plain = serde_json::to_string(&document("00000000000000000000000000000021")).unwrap();
    assert!(!plain.contains("cache"), "{plain}");

    let with = |cache: CachePreparation| {
        let mut doc = document("00000000000000000000000000000022");
        doc.iterations[0].children[0].cache = Some(cache);
        doc
    };
    refused(
        &with(prepared(vec![file(CacheMode::Cold, 10, 1, &["/d/a"])])),
        "with 1 resident",
    );
    refused(
        &with(prepared(vec![file(CacheMode::Warm, 10, 9, &["/d/a"])])),
        "with 9 resident",
    );
    refused(
        &with(prepared(vec![file(CacheMode::Cold, 1, 0, &["d/a"])])),
        "absolute path",
    );
    refused(&with(prepared(Vec::new())), "no file");
    let mut late = prepared(vec![file(CacheMode::Cold, 1, 0, &["/d/a"])]);
    late.scan_to_us = i64::MAX;
    refused(&with(late), "does not end by the launch");
}

#[test]
fn a_newer_schema_and_a_foreign_file_are_refused() {
    let dir = crate::test_scratch::scratch("bench_store", "guards");
    let newer = dir.join("newer.db");
    {
        let conn = rusqlite::Connection::open(&newer).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    assert!(matches!(Store::open(&newer), Err(StoreError::Refused(why)) if why.contains("newer")));

    let negative = dir.join("negative.db");
    {
        let conn = rusqlite::Connection::open(&negative).unwrap();
        conn.pragma_update(None, "user_version", -1).unwrap();
    }
    assert!(
        matches!(Store::open(&negative), Err(StoreError::Refused(why)) if why.contains("no version"))
    );

    let foreign = dir.join("foreign.db");
    {
        let conn = rusqlite::Connection::open(&foreign).unwrap();
        conn.execute_batch("CREATE TABLE runs (id INTEGER)")
            .unwrap();
    }
    assert!(
        matches!(Store::open(&foreign), Err(StoreError::Refused(why)) if why.contains("not a bench store"))
    );

    // Opening twice is a no-op the second time.
    let fresh = dir.join("fresh.db");
    drop(Store::open(&fresh).unwrap());
    let store = Store::open(&fresh).unwrap();
    let mode: String = store
        .conn()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

/// The frozen sample columns are the store's sample fields, and the store's
/// sample fields are the sidecar's: a new `/proc` field has to reach both.
#[test]
fn sample_columns_match_the_schema_and_the_sidecar() {
    let store = store("columns");
    let mut stmt = store
        .conn()
        .prepare("SELECT name FROM pragma_table_info('samples') ORDER BY cid")
        .unwrap();
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut expected = vec!["child_id".to_owned(), "sample_idx".to_owned()];
    expected.extend(Sample::COLUMNS.iter().map(|c| (*c).to_owned()));
    assert_eq!(columns, expected);

    let sidecar: Vec<&str> = crate::sidecar::Sample::db_column_names()
        .into_iter()
        .filter(|c| *c != "sample_idx")
        .collect();
    assert_eq!(sidecar, Sample::COLUMNS);
}

#[test]
fn a_capture_converts_without_loss() {
    use crate::sidecar::{Counter, Marker, Terminal};

    let mut data = crate::sidecar::SidecarData::for_test(vec![
        crate::sidecar::Record::Marker(Marker {
            marker_idx: 0,
            timestamp_us: 5,
            name: "m".into(),
            run_idx: 0,
            call: None,
        }),
        crate::sidecar::Record::Counter(Counter {
            timestamp_us: 6,
            name: "c".into(),
            value: 7,
            run_idx: 0,
            call: Some("k".into()),
        }),
        crate::sidecar::Record::Terminal(Terminal {
            timestamp_us: 8,
            call: "k".into(),
            end: crate::sidecar::SinkEnd::Sealed,
            drops: 3,
        }),
    ]);
    data.calls_seen.ids.insert("k".into());
    data.summary.loss.rejected_lines = 4;
    let converted = evidence::evidence(&data);
    assert_eq!(
        converted.records,
        vec![
            Record::Marker {
                timestamp_us: 5,
                call: None,
                name: "m".into()
            },
            Record::Counter {
                timestamp_us: 6,
                call: Some("k".into()),
                name: "c".into(),
                value: 7
            },
            Record::Terminal {
                timestamp_us: 8,
                call: "k".into(),
                end: SinkEnd::Sealed,
                drops: 3
            },
        ]
    );
    assert_eq!(converted.loss.rejected_lines, 4);
    assert_eq!(converted.calls_seen, BTreeSet::from(["k".to_owned()]));

    let deadline = std::time::Duration::from_nanos(1_500_000_001);
    assert_eq!(
        evidence::child_ending(&crate::output::Ending::Deadline(deadline)),
        ChildEnding::Deadline {
            secs: 1,
            nanos: 500_000_001
        }
    );
    assert_eq!(
        evidence::child_ending(&crate::output::Ending::Deadline(std::time::Duration::MAX)),
        ChildEnding::Deadline {
            secs: u64::MAX,
            nanos: 999_999_999
        }
    );
    let env = evidence::redacted_env(&BTreeMap::from([
        ("API_TOKEN".into(), "hunter2".into()),
        ("PLAIN".into(), "kept".into()),
    ]));
    assert_eq!(env["API_TOKEN"], "<redacted>");
    assert_eq!(env["PLAIN"], "kept");
}
