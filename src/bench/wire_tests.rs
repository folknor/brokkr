//! The store's wire contract (`store::wire`) without HTTP: the frozen
//! encoding of the current protocol, the strictness of what it decodes, `serve`'s
//! mapping of every store answer, and the failure envelope a client reads.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use super::store::wire::{
    self, Answer, Failure, Kind, PROTOCOL_VERSION, Request, SHIPPED_PROTOCOLS,
};
use super::store::{
    BoundParam, Build, CacheFile, CacheMode, CachePreparation, CaptureOutcome, Child, ChildEnding,
    Clock, Conditions, DOCUMENT_FORMAT, Evidence, Executable, ExecutableIdentity, HotpathCapture,
    Iteration, IterationOutcome, Loss, Mark, MarkOrigin, MarkRequest, MarkView, Marked,
    Measurement, ParamValue, Published, Record, ResolvedBuild, Revision, RunDocument, RunFilter,
    RunOutcome, Sample, Selection, SinkEnd, StderrCapture, StderrTail, Store, StoreError,
    StoredRun,
};

/// The fixture child's one sample, every field distinct.
fn frozen_sample() -> Sample {
    Sample {
        timestamp_us: 100,
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

/// The fixture's one child: every record kind and a deadline ending.
fn frozen_child() -> Child {
    let sample = frozen_sample();
    Child {
        program: "pbfhogg".into(),
        argv: vec!["cat".into(), "--jobs".into(), "4".into()],
        env: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
        pid: Some(4242),
        origin_offset_us: 10,
        wall_us: 1_500,
        ending: ChildEnding::Deadline {
            secs: u64::MAX,
            nanos: 999_999_999,
        },
        evidence: Evidence {
            samples: vec![sample],
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
                    end: SinkEnd::Sealed,
                    drops: u64::MAX,
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
            calls_seen: BTreeSet::from(["c1".to_owned()]),
            calls_overflowed: false,
        },
        hotpath: Some(HotpathCapture {
            stage: "decode".into(),
            occurrence: 1,
            outcome: CaptureOutcome::Captured {
                report: serde_json::json!({"functions_timing": {"data": []}}),
            },
        }),
        executable: Some(Executable {
            path: "/usr/bin/lua5.4".into(),
            identity: ExecutableIdentity::Digest {
                digest: "00112233445566778899aabbccddeeff".into(),
            },
        }),
        stderr: Some(StderrCapture {
            released: true,
            bytes: Some(u64::MAX >> 1),
            tail: StderrTail::Kept {
                text: "line\nerror: \u{fffd}\n".into(),
                raw: 14,
                cut: true,
            },
        }),
        measured: true,
        cache: Some(frozen_cache()),
    }
}

/// The fixture child's page-cache preparation: both modes, an alias.
fn frozen_cache() -> CachePreparation {
    CachePreparation {
        procedure: "page-cache-v1".into(),
        scan_from_us: 2,
        scan_to_us: 9,
        files: vec![
            CacheFile {
                paths: vec!["/data/a.osm.pbf".into(), "/data/same.osm.pbf".into()],
                mode: CacheMode::Cold,
                bytes: u64::MAX >> 1,
                resident: 0,
            },
            CacheFile {
                paths: vec!["/data/b.osm.pbf".into()],
                mode: CacheMode::Warm,
                bytes: 4097,
                resident: 4097,
            },
        ],
    }
}

/// Every stderr capture but the fixture child's, frozen with the protocol.
fn frozen_stderr_captures() -> Vec<StderrCapture> {
    vec![
        StderrCapture {
            released: false,
            bytes: None,
            tail: StderrTail::Unreadable {
                reason: "gone".into(),
            },
        },
        StderrCapture {
            released: true,
            bytes: Some(0),
            tail: StderrTail::OverBudget,
        },
    ]
}

/// Every executable identity but the fixture child's, frozen with the
/// protocol.
fn frozen_executable_identities() -> Vec<ExecutableIdentity> {
    vec![ExecutableIdentity::Unread {
        reason: "permission denied".into(),
    }]
}

/// Every capture outcome but the fixture child's, frozen with the protocol.
fn frozen_capture_outcomes() -> Vec<CaptureOutcome> {
    vec![
        CaptureOutcome::Missing,
        CaptureOutcome::Invalid {
            reason: "not JSON".into(),
        },
        CaptureOutcome::Unreadable {
            reason: "gone".into(),
        },
        CaptureOutcome::OverBudget,
    ]
}

/// The protocol's fixture document. Frozen with the protocol: changing it
/// (or its child) changes the fingerprint, and so needs a new protocol
/// version.
fn frozen_document() -> RunDocument {
    let child = frozen_child();
    let mut record = serde_json::Map::new();
    record.insert("rate".into(), serde_json::json!(0.1));
    record.insert("nested".into(), serde_json::json!({"a": [1, "x", null]}));
    RunDocument {
        format: DOCUMENT_FORMAT,
        uuid: "0123456789abcdef0123456789abcdef".into(),
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
        measurement: Measurement::Hotpath,
        params: BTreeMap::from([(
            "osc".into(),
            BoundParam {
                many: true,
                from: Some("datasets.denmark.osc".into()),
                values: vec![
                    ParamValue {
                        key: "12".into(),
                        digest: Some("aa".into()),
                    },
                    ParamValue {
                        key: "10".into(),
                        digest: None,
                    },
                ],
            },
        )]),
        config: "project = \"pbfhogg\"\n".into(),
        iterations_requested: 1,
        outcome: RunOutcome::Completed,
        iterations: vec![Iteration {
            outcome: IterationOutcome::Completed { verdict: None },
            wall_us: Some(2_000),
            record: Some(record),
            children: vec![child],
        }],
    }
}

/// The protocol's mark fixtures: a view holding every map, and one request
/// of every mark kind. Frozen with the protocol.
fn frozen_marks() -> (MarkView, Vec<MarkRequest>) {
    let origin = MarkOrigin {
        uuid: "cd".repeat(16),
        at_unix_ms: 1_762_000_000_001,
        hostname: "bygg".into(),
        reason: "superseded".into(),
    };
    let view = MarkView {
        renames: BTreeMap::from([("cat-old".into(), "cat".into())]),
        retired: BTreeMap::from([("inspect".into(), origin.clone())]),
        invalidated: BTreeMap::from([("01".repeat(16), origin)]),
    };
    let mark = |mark: Mark| MarkRequest {
        uuid: "cd".repeat(16),
        project: "pbfhogg".into(),
        at_unix_ms: 1_762_000_000_001,
        hostname: "bygg".into(),
        mark,
    };
    let marks = vec![
        mark(Mark::Retire {
            name: "inspect".into(),
            reason: "superseded".into(),
        }),
        mark(Mark::Unretire {
            name: "inspect".into(),
        }),
        mark(Mark::Invalidate {
            run: "01".repeat(16),
            reason: "noisy host".into(),
        }),
        mark(Mark::Revalidate {
            run: "01".repeat(16),
        }),
        mark(Mark::Rename {
            from: "cat-old".into(),
            to: "cat".into(),
        }),
    ];
    (view, marks)
}

/// Every request and answer kind, encoded canonically, one per line.
fn frozen_encoding() -> String {
    let document = frozen_document();
    let filter = RunFilter {
        project: "pbfhogg".into(),
        uuid_prefix: Some("01".into()),
        commit_prefix: None,
        bench: Some("cat".into()),
        env: vec![("A".into(), "1".into())],
        params: vec![("osc".into(), "12,10".into())],
        grep: vec!["--jobs".into()],
        grep_v: Vec::new(),
        with_result: true,
        clean: false,
        hidden: true,
    };
    let (view, marks) = frozen_marks();
    let revision = Revision { marks: 3, runs: 7 };
    let stored = StoredRun {
        doc: document.clone(),
        config_digest: "ffeeddccbbaa99887766554433221100".into(),
        pairing_key: "[\"bench-pair-v1\"]".into(),
        best_iteration: Some(0),
        children_loaded: true,
    };
    let requests = [
        Request::Publish {
            document: Box::new(document),
        },
        Request::Select {
            filter: filter.clone(),
            at: Some(revision),
        },
        Request::Select { filter, at: None },
        Request::Run {
            uuid: "01".repeat(16),
            children: true,
        },
        Request::HasRuns {
            project: "pbfhogg".into(),
        },
        Request::Elsewhere {
            project: "pbfhogg".into(),
            prefix: "ab".into(),
        },
        Request::Mark {
            request: marks[0].clone(),
        },
        Request::Marks {
            project: "pbfhogg".into(),
        },
    ];
    let answers = [
        Answer::Published(Published::Stored),
        Answer::Published(Published::AlreadyStored),
        Answer::Selected(Selection {
            uuids: vec!["01".repeat(16)],
            revision,
            marks: view,
        }),
        Answer::Run(Some(Box::new(stored))),
        Answer::Run(None),
        Answer::HasRuns(true),
        Answer::Elsewhere(vec![("ab".repeat(16), "elivagar".into())]),
        Answer::Marked(Marked::Applied),
        Answer::Marked(Marked::AlreadyApplied),
        Answer::Marks(marks),
    ];
    let mut lines: Vec<String> = requests
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    lines.extend(answers.iter().map(|a| serde_json::to_string(a).unwrap()));
    lines.push(serde_json::to_string(&frozen_capture_outcomes()).unwrap());
    lines.push(serde_json::to_string(&frozen_executable_identities()).unwrap());
    lines.push(serde_json::to_string(&frozen_stderr_captures()).unwrap());
    lines.push(
        serde_json::to_string(&[
            Measurement::Timing,
            Measurement::Hotpath,
            Measurement::Alloc,
        ])
        .unwrap(),
    );
    lines.extend(
        [
            Kind::Refused,
            Kind::Conflict,
            Kind::Failed,
            Kind::CommitUncertain,
            Kind::BadRequest,
            Kind::TooLarge,
            Kind::Busy,
            Kind::ReadTimeout,
            Kind::WaitExpired { withdrawn: true },
            Kind::WaitExpired { withdrawn: false },
            Kind::Internal,
        ]
        .into_iter()
        .map(|kind| String::from_utf8(Failure::new(kind, "why").encode()).unwrap()),
    );
    lines.join("\n")
}

/// The route and the header are the protocol's too, and outlive it: a
/// client of any version finds the server there.
#[test]
fn the_route_header_and_body_cap_are_fixed() {
    assert_eq!(wire::PATH, "/bench");
    assert_eq!(wire::PROTOCOL_HEADER, "brokkr-bench-protocol");
    assert_eq!(wire::MAX_REQUEST_BYTES, 256 << 20);
}

#[test]
fn the_shipped_protocol_encodes_as_it_was_frozen() {
    let encoding = frozen_encoding();
    let digest = format!("{:032x}", xxhash_rust::xxh3::xxh3_128(encoding.as_bytes()));
    assert_eq!(
        SHIPPED_PROTOCOLS.len(),
        usize::try_from(PROTOCOL_VERSION).unwrap(),
        "every protocol version has exactly one shipped fingerprint"
    );
    assert_eq!(
        SHIPPED_PROTOCOLS.last().copied(),
        Some(digest.as_str()),
        "the wire encoding changed: append a fingerprint and bump PROTOCOL_VERSION; \
         a shipped fingerprint never changes. Encoding:\n{encoding}"
    );
    // Every frozen line decodes back to what made it.
    for line in encoding.lines().take(8) {
        let request: Request = serde_json::from_str(line).unwrap();
        assert_eq!(serde_json::to_string(&request).unwrap(), line);
    }
    for line in encoding.lines().skip(8).take(10) {
        let answer: Answer = serde_json::from_str(line).unwrap();
        assert_eq!(serde_json::to_string(&answer).unwrap(), line);
    }
}

fn refuses<T: serde::de::DeserializeOwned>(json: &str) {
    assert!(
        serde_json::from_str::<T>(json).is_err(),
        "decoded what the protocol refuses: {json}"
    );
}

/// What strictness promises, which an encoding fingerprint cannot see.
#[test]
fn the_protocol_refuses_what_it_does_not_define() {
    let document = serde_json::to_value(frozen_document()).unwrap();
    let request = |document: serde_json::Value| {
        serde_json::json!({"publish": {"document": document}}).to_string()
    };
    assert!(serde_json::from_str::<Request>(&request(document.clone())).is_ok());

    let mut extra = document.clone();
    extra["novel"] = serde_json::json!(1);
    refuses::<Request>(&request(extra));
    let mut nested = document.clone();
    nested["iterations"][0]["children"][0]["evidence"]["loss"]["novel"] = serde_json::json!(true);
    refuses::<Request>(&request(nested));
    let mut executable = document.clone();
    executable["iterations"][0]["children"][0]["executable"]["novel"] = serde_json::json!(1);
    refuses::<Request>(&request(executable));
    let mut identity = document.clone();
    identity["iterations"][0]["children"][0]["executable"]["identity"]["digest"]["novel"] =
        serde_json::json!(1);
    refuses::<Request>(&request(identity));
    let mut stderr = document.clone();
    stderr["iterations"][0]["children"][0]["stderr"]["tail"]["kept"]["novel"] =
        serde_json::json!(1);
    refuses::<Request>(&request(stderr));
    let mut missing = document.clone();
    missing.as_object_mut().unwrap().remove("project");
    refuses::<Request>(&request(missing));
    let mut wide = document.clone();
    wide["iterations_requested"] = serde_json::json!(4_294_967_296_u64);
    refuses::<Request>(&request(wide));

    refuses::<Request>(r#"{"publish": {"document": null}, "select": {}}"#);
    refuses::<Request>(r#"{"invalidate": {"uuid": "ab"}}"#);
    refuses::<Request>(r#"{"run": {"uuid": "ab"}}"#);
    refuses::<Request>(r#"{"run": {"uuid": "ab", "children": true, "deep": true}}"#);
    refuses::<Request>(r#"{"select": {"filter": {"project": "p"}}}"#);
    let mark = |mark: &str| {
        format!(
            r#"{{"mark": {{"request": {{"uuid": "{}", "project": "p", "at_unix_ms": 1, "hostname": "h", "mark": {mark}}}}}}}"#,
            "a".repeat(32)
        )
    };
    assert!(serde_json::from_str::<Request>(&mark(r#"{"unretire": {"name": "x"}}"#)).is_ok());
    refuses::<Request>(&mark(r#"{"unretire": {"name": "x", "reason": "r"}}"#));
    refuses::<Request>(&mark(r#"{"retire": {"name": "x"}}"#));
    refuses::<Request>(&mark(r#"{"unrename": {"from": "x"}}"#));
    refuses::<Answer>(r#"{"marked": "maybe"}"#);
    refuses::<Answer>(r#"{"published": "maybe"}"#);
    refuses::<Answer>(r#"{"has_runs": true, "extra": 1}"#);
}

fn store(name: &str) -> Store {
    let dir = crate::test_scratch::scratch("bench_wire", name);
    Store::open(&dir.join("bench.db")).unwrap()
}

fn serve(store: &mut Store, request: &Request) -> wire::Reply {
    wire::serve(store, &serde_json::to_vec(request).unwrap())
}

#[test]
fn serve_answers_every_store_outcome_with_its_kind() {
    let mut store = store("serve");
    let document = frozen_document();
    let publish = Request::Publish {
        document: Box::new(document.clone()),
    };
    let stored = serve(&mut store, &publish);
    assert_eq!(stored.failure, None);
    assert_eq!(
        serde_json::from_slice::<Answer>(&stored.body).unwrap(),
        Answer::Published(Published::Stored)
    );
    let replay = serve(&mut store, &publish);
    assert_eq!(
        serde_json::from_slice::<Answer>(&replay.body).unwrap(),
        Answer::Published(Published::AlreadyStored)
    );

    let mut changed = document.clone();
    changed.bench = "inspect".into();
    let conflict = serve(
        &mut store,
        &Request::Publish {
            document: Box::new(changed),
        },
    );
    assert_eq!(conflict.failure, Some(Kind::Conflict));

    let mut invalid = document.clone();
    invalid.uuid = "short".into();
    let refused = serve(
        &mut store,
        &Request::Publish {
            document: Box::new(invalid),
        },
    );
    assert_eq!(refused.failure, Some(Kind::Refused));

    let garbage = wire::serve(&mut store, b"not json");
    assert_eq!(garbage.failure, Some(Kind::BadRequest));

    let run = serve(
        &mut store,
        &Request::Run {
            uuid: document.uuid.clone(),
            children: true,
        },
    );
    let Answer::Run(Some(run)) = serde_json::from_slice::<Answer>(&run.body).unwrap() else {
        panic!("no run");
    };
    assert!(run.children_loaded);
    assert_eq!(
        run.doc, document,
        "the document survives the wire both ways"
    );
    assert_eq!(run.best_iteration, Some(0));
    assert!(!store.in_transaction());
}

fn envelope(json: &str) -> Option<Failure> {
    Failure::decode(json.as_bytes())
}

/// A client's reading is the envelope's, never more: a store error is
/// rebuilt only from an envelope that is exactly one.
#[test]
fn the_failure_envelope_is_tolerant_but_never_claims_more_than_it_proves() {
    let exact = envelope(
        r#"{"error": {"kind": "failed", "message": "disk", "applied": false, "retry": true}}"#,
    )
    .unwrap()
    .classify(None);
    assert!(exact.unapplied && exact.retry);
    assert_eq!(exact.store, Some(StoreError::Failed("disk".into())));

    // Without `applied`, a failed kind proves nothing: uncertain, and no
    // StoreError::Failed whose class would claim it unapplied.
    let unproved = envelope(r#"{"error": {"kind": "failed", "message": "disk", "retry": true}}"#)
        .unwrap()
        .classify(None);
    assert!(!unproved.unapplied && unproved.retry);
    assert_eq!(unproved.store, None);

    let uncertain = envelope(
        r#"{"error": {"kind": "commit_uncertain", "message": "io", "retry": true, "later": 1}}"#,
    )
    .unwrap()
    .classify(None);
    assert!(!uncertain.unapplied);
    assert_eq!(
        uncertain.store,
        Some(StoreError::CommitUncertain("io".into()))
    );

    // A known kind contradicting its own envelope proves nothing either:
    // commit_uncertain never carries applied false.
    let contradicted = envelope(
        r#"{"error": {"kind": "commit_uncertain", "message": "io", "applied": false, "retry": true}}"#,
    )
    .unwrap()
    .classify(None);
    assert!(!contradicted.unapplied);
    assert_eq!(contradicted.store, None);
    let final_busy = envelope(
        r#"{"error": {"kind": "busy", "message": "x", "applied": false, "retry": false}}"#,
    )
    .unwrap()
    .classify(None);
    assert!(!final_busy.unapplied);
    for withdrawn in [r#""applied": false, "#, ""] {
        let expired = envelope(&format!(
            r#"{{"error": {{"kind": "wait_expired", "message": "x", {withdrawn}"retry": true}}}}"#
        ))
        .unwrap()
        .classify(None);
        assert_eq!(expired.unapplied, !withdrawn.is_empty());
    }

    let applied_true = envelope(
        r#"{"error": {"kind": "refused", "message": "x", "applied": true, "retry": false}}"#,
    )
    .unwrap()
    .classify(None);
    assert!(!applied_true.unapplied);
    assert_eq!(applied_true.store, None);

    let future = envelope(
        r#"{"error": {"kind": "quota", "message": "x", "applied": false, "retry": false}}"#,
    )
    .unwrap()
    .classify(None);
    assert!(future.unapplied && !future.retry);
    assert_eq!(future.store, None);

    let conflict = envelope(
        r#"{"error": {"kind": "conflict", "message": "x", "applied": false, "retry": false}}"#,
    )
    .unwrap();
    assert_eq!(conflict.classify(None).store, None);
    assert_eq!(
        conflict.classify(Some("ab")).store,
        Some(StoreError::Conflict { uuid: "ab".into() })
    );

    assert_eq!(
        envelope(r#"{"error": {"kind": "busy", "message": "x"}}"#),
        None
    );
    assert_eq!(
        envelope(r#"{"error": {"kind": "busy", "kind": "x", "message": "x", "retry": true}}"#),
        None
    );
    assert_eq!(envelope("<html>Bad Gateway</html>"), None);

    for kind in [
        Kind::Refused,
        Kind::Conflict,
        Kind::Failed,
        Kind::CommitUncertain,
    ] {
        let error = match kind {
            Kind::Refused => StoreError::Refused("m".into()),
            Kind::Conflict => StoreError::Conflict { uuid: "ab".into() },
            Kind::Failed => StoreError::Failed("m".into()),
            _ => StoreError::CommitUncertain("m".into()),
        };
        let failure = Failure::new(Kind::of(&error), "m");
        let read = failure.classify(Some("ab"));
        assert_eq!(read.store, Some(error.clone()), "{kind:?}");
        assert_eq!(read.unapplied, !error.outcome_uncertain(), "{kind:?}");
    }
}
