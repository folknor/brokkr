//! `brokkr results` over the bench store, against documents published
//! through the store's own write path.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::super::evidence;
use super::super::store::{
    BoundParam, Build, CaptureOutcome, Child, ChildEnding, Clock, Conditions, DOCUMENT_FORMAT,
    Evidence, HotpathCapture, Iteration, IterationOutcome, Loss, Measurement, ParamValue,
    ResolvedBuild, RunDocument, RunOutcome, Store,
};
use super::{BenchFilter, BenchReader, format_compare, format_single, format_table};

fn scratch(name: &str) -> PathBuf {
    crate::test_scratch::scratch("bench_results", name)
        .join("brokkr")
        .join("bench.db")
}

fn param(many: bool, values: &[(&str, Option<&str>)]) -> BoundParam {
    BoundParam {
        many,
        from: Some("datasets".into()),
        values: values
            .iter()
            .map(|(key, digest)| ParamValue {
                key: (*key).to_owned(),
                digest: digest.map(str::to_owned),
            })
            .collect(),
    }
}

fn iteration(wall_us: i64, verdict: Option<&str>, record: &serde_json::Value) -> Iteration {
    Iteration {
        outcome: IterationOutcome::Completed {
            verdict: verdict.map(str::to_owned),
        },
        wall_us: Some(wall_us),
        record: Some(record.as_object().unwrap().clone()),
        children: vec![Child {
            program: "pbfhogg".into(),
            argv: vec!["cat".into(), "/data/denmark.osm.pbf".into()],
            env: BTreeMap::from([("RAYON_NUM_THREADS".into(), "4".into())]),
            pid: Some(7),
            origin_offset_us: 12,
            wall_us: (wall_us - 20).max(0),
            ending: ChildEnding::Exited { code: 0 },
            evidence: Evidence {
                samples: Vec::new(),
                records: Vec::new(),
                loss: Loss {
                    rejected_lines: 0,
                    fifo_unread: false,
                    records_capped: false,
                    samples_capped: false,
                    output_truncated: true,
                },
                clock: Clock {
                    outliers: 0,
                    min_us: None,
                },
                calls_seen: std::collections::BTreeSet::new(),
                calls_overflowed: false,
            },
            hotpath: None,
            executable: None,
            stderr: None,
        }],
    }
}

/// A completed run of `bench` at `commit`, started at `started`.
pub(crate) fn run(
    uuid_tail: u8,
    bench: &str,
    commit: char,
    started: i64,
    wall_us: i64,
) -> RunDocument {
    RunDocument {
        format: DOCUMENT_FORMAT,
        uuid: format!("{:032x}", u128::from(uuid_tail) | (0xabcd << 64)),
        project: "pbfhogg".into(),
        bench: bench.into(),
        checkout: "/src/pbfhogg".into(),
        hostname: "bygg".into(),
        commit: commit.to_string().repeat(40),
        dirty: false,
        subject: Some("feat: x".into()),
        started_unix_ms: started,
        brokkr_version: "0.1.0".into(),
        argv: vec![
            "brokkr".into(),
            "bench".into(),
            bench.into(),
            "--dataset".into(),
            "denmark".into(),
        ],
        conditions: Conditions {
            kernel: Some("7.3.0".into()),
            cpu_governor: Some("performance".into()),
            avail_memory_mb: Some(64_000),
            captured_env: BTreeMap::new(),
        },
        build: Build {
            target: None,
            profile: "release".into(),
            features: Vec::new(),
            default_features: true,
            resolved: Some(ResolvedBuild {
                bin: "pbfhogg".into(),
                features: vec!["default".into()],
            }),
            artifact: Some("00112233445566778899aabbccddeeff".into()),
        },
        measurement: Measurement::Timing,
        params: BTreeMap::from([("dataset".into(), param(false, &[("denmark", None)]))]),
        config: "[datasets.denmark]\nbbox = \"1,2,3,4\"\n".into(),
        iterations_requested: 1,
        outcome: RunOutcome::Completed,
        iterations: vec![iteration(wall_us, None, &serde_json::json!({"nodes": 10}))],
    }
}

fn publish(path: &std::path::Path, docs: &[RunDocument]) {
    let mut store = evidence::open_store(path).unwrap();
    for doc in docs {
        store.publish(doc).unwrap();
    }
}

pub(in crate::bench) fn filter() -> BenchFilter {
    BenchFilter {
        limit: 20,
        ..BenchFilter::default()
    }
}

fn reader(path: &std::path::Path) -> BenchReader {
    BenchReader::at(path, "pbfhogg").unwrap().unwrap()
}

/// The single view of the run `uuid` names, under its lookup's marks.
fn single(reader: &BenchReader, uuid: &str) -> String {
    let lookup = reader.lookup(uuid, &filter()).unwrap();
    format_single(&lookup.runs[0], &lookup.marks, 10)
}

fn compare(c: &super::Comparison, a: &str, b: &str, limit: usize) -> String {
    format_compare(a, b, c, 10, limit)
}

fn benches(runs: &[super::StoredRun]) -> Vec<(String, i64)> {
    runs.iter()
        .map(|r| (r.doc.bench.clone(), r.doc.started_unix_ms))
        .collect()
}

#[test]
fn a_read_only_open_creates_nothing_and_refuses_what_a_writer_refuses() {
    let path = scratch("open");
    assert!(BenchReader::at(&path, "pbfhogg").unwrap().is_none());
    assert!(!path.exists(), "a reader must not create the store");
    assert!(Store::open_read_only(&path).unwrap().is_none());

    publish(&path, &[run(1, "cat", 'a', 1_000, 500)]);
    let reader = reader(&path);
    assert!(reader.has_runs().unwrap());

    let newer = path.with_file_name("newer.db");
    rusqlite::Connection::open(&newer)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    assert!(BenchReader::at(&newer, "pbfhogg").is_err());

    // A foreign file stamped with the current version opens, and its first
    // query fails naming the file.
    let current: i64 = Store::open(&path.with_file_name("current.db"))
        .unwrap()
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let stamped = path.with_file_name("stamped.db");
    rusqlite::Connection::open(&stamped)
        .unwrap()
        .pragma_update(None, "user_version", current)
        .unwrap();
    let error = BenchReader::at(&stamped, "pbfhogg")
        .unwrap()
        .unwrap()
        .has_runs()
        .unwrap_err();
    assert!(error.to_string().contains("stamped.db"), "{error}");

    // An empty file at version 0 is migrated in place, then read.
    let empty = path.with_file_name("empty.db");
    std::fs::write(&empty, b"").unwrap();
    let store = Store::open_read_only(&empty).unwrap().unwrap();
    assert!(!store.has_runs("pbfhogg").unwrap());
}

#[test]
fn the_listing_is_one_project_newest_first_by_start_whatever_the_publication_order() {
    let path = scratch("listing");
    let mut other = run(9, "cat", 'a', 9_000, 100);
    other.project = "dellingr".into();
    publish(
        &path,
        &[
            run(2, "cat", 'a', 2_000, 100),
            run(1, "inspect", 'a', 3_000, 100),
            run(3, "cat", 'b', 1_000, 100),
            other,
        ],
    );
    let reader = reader(&path);
    let all = reader.table(&filter()).unwrap().runs;
    assert_eq!(
        benches(&all),
        vec![
            ("inspect".into(), 3_000),
            ("cat".into(), 2_000),
            ("cat".into(), 1_000)
        ]
    );
    let capped = reader
        .table(&BenchFilter {
            limit: 1,
            ..filter()
        })
        .unwrap();
    assert_eq!(capped.runs.len(), 1);

    let by = |f: BenchFilter| benches(&reader.table(&f).unwrap().runs);
    assert_eq!(
        by(BenchFilter {
            commit: Some("BBBB".into()),
            ..filter()
        }),
        vec![("cat".into(), 1_000)]
    );
    assert_eq!(
        by(BenchFilter {
            bench: Some("INSP".into()),
            ..filter()
        }),
        vec![("inspect".into(), 3_000)]
    );
    // A filter no bench run can answer excludes every one.
    assert!(
        by(BenchFilter {
            excluded: true,
            ..filter()
        })
        .is_empty()
    );
    assert!(
        reader
            .table(&BenchFilter {
                commit: Some(String::new()),
                ..filter()
            })
            .is_err()
    );
}

#[test]
fn env_param_and_grep_filters_reach_every_invocation_source() {
    let path = scratch("filters");
    let mut env = run(1, "cat", 'a', 1_000, 100);
    env.conditions
        .captured_env
        .insert("MALLOC_ARENA_MAX".into(), "2".into());
    let mut many = run(2, "merge", 'a', 2_000, 100);
    many.params.insert(
        "osc".into(),
        param(true, &[("12", Some("aa")), ("10", Some("bb"))]),
    );
    many.params.insert("none".into(), param(true, &[]));
    publish(&path, &[env, many]);
    let reader = reader(&path);
    let by = |f: BenchFilter| benches(&reader.table(&f).unwrap().runs);

    assert_eq!(
        by(BenchFilter {
            env: vec![("MALLOC_ARENA_MAX".into(), "2".into())],
            ..filter()
        }),
        vec![("cat".into(), 1_000)]
    );
    // A collection matches in bound order, and an empty one as "".
    let param = |name: &str, value: &str| BenchFilter {
        params: vec![(name.into(), value.into())],
        ..filter()
    };
    assert_eq!(by(param("osc", "12,10")).len(), 1);
    assert!(by(param("osc", "10,12")).is_empty());
    assert_eq!(by(param("none", "")).len(), 1);
    assert_eq!(by(param("dataset", "denmark")).len(), 2);

    let grep = |terms: &[&str], not: &[&str]| {
        by(BenchFilter {
            grep: terms.iter().map(|t| (*t).to_owned()).collect(),
            grep_v: not.iter().map(|t| (*t).to_owned()).collect(),
            ..filter()
        })
    };
    // brokkr's argv, a child's argv, a child's env and the captured env.
    assert_eq!(grep(&["merge"], &[]).len(), 1);
    assert_eq!(grep(&["/data/denmark.osm.pbf"], &[]).len(), 2);
    assert_eq!(grep(&["RAYON_NUM_THREADS=4"], &[]).len(), 2);
    assert_eq!(
        grep(&["MALLOC_ARENA_MAX=2"], &[]),
        vec![("cat".into(), 1_000)]
    );
    assert_eq!(grep(&[], &["MALLOC_ARENA"]), vec![("merge".into(), 2_000)]);
}

#[test]
fn a_uuid_prefix_counts_its_candidates_and_names_another_projects_run() {
    let path = scratch("lookup");
    let mut other = run(7, "cat", 'a', 9_000, 100);
    other.project = "dellingr".into();
    other.uuid = "f".repeat(32);
    publish(
        &path,
        &[
            run(1, "cat", 'a', 1_000, 100),
            run(2, "cat", 'a', 2_000, 100),
            other,
        ],
    );
    let reader = reader(&path);
    let found = reader
        .lookup(
            "0000",
            &BenchFilter {
                limit: 1,
                ..filter()
            },
        )
        .unwrap();
    assert_eq!(
        (found.runs.len(), found.omitted, found.candidates),
        (1, 1, 2)
    );
    assert!(found.runs[0].children_loaded);

    let elsewhere = reader.lookup("FFFF", &filter()).unwrap();
    assert!(elsewhere.runs.is_empty());
    assert_eq!(
        elsewhere.elsewhere,
        vec![("f".repeat(32), "dellingr".to_owned())]
    );
    assert!(reader.lookup("", &filter()).is_err());
}

#[test]
fn the_single_view_shows_verdicts_children_records_and_the_hotpath_report() {
    let path = scratch("single");
    let mut doc = run(1, "cat", 'a', 1_762_000_000_000, 2_000);
    doc.iterations_requested = 2;
    doc.iterations = vec![
        iteration(3_000, Some(""), &serde_json::json!({"nodes": 10})),
        iteration(
            2_000,
            Some(""),
            &serde_json::json!({"nodes": 10, "hotpath": {"functions_timing": {"data": [
                {"name": "decode", "calls": 12, "avg": "2 ms", "total": "24 ms",
                 "percent_total": "40%", "p50": "1.8 ms", "p95": "3.0 ms", "p99": "3.8 ms"}
            ]}}}),
        ),
    ];
    let mut failed = run(2, "cat", 'a', 1_000, 100);
    failed.outcome = RunOutcome::Failed {
        reason: "exit 2".into(),
    };
    failed.build.artifact = None;
    failed.iterations[0].outcome = IterationOutcome::Failed {
        reason: "exit 2".into(),
    };
    failed.iterations[0].record = Some(
        serde_json::json!({"hotpath": {"nope": 1}})
            .as_object()
            .unwrap()
            .clone(),
    );
    publish(&path, &[doc.clone(), failed.clone()]);
    let reader = reader(&path);

    let shown = single(&reader, &doc.uuid);
    assert!(shown.contains("2025-11-01 12:26:40"), "{shown}");
    assert!(shown.contains("verdict     \"\""), "{shown}");
    assert!(
        shown.contains("2.000 ms (iteration 1, best of 2: 3.000 ms / 2.000 ms)"),
        "{shown}"
    );
    assert!(shown.contains("iteration 1 (result)"), "{shown}");
    assert!(
        shown.contains("exit 0  pbfhogg cat /data/denmark.osm.pbf"),
        "{shown}"
    );
    assert!(shown.contains("output truncated"), "{shown}");
    assert!(shown.contains("record      nodes = 10"), "{shown}");
    assert!(shown.contains("decode"), "{shown}");

    let shown = single(&reader, &failed.uuid);
    assert!(shown.contains("outcome     failed: exit 2"), "{shown}");
    assert!(
        shown.contains("hotpath = {\"nope\":1} (not a hotpath report brokkr can read)"),
        "{shown}"
    );

    let listing = reader.table(&filter()).unwrap();
    let table = format_table(&listing.runs, &listing.marks);
    assert!(table.contains("failed"), "{table}");
    assert!(!table.contains("hidden"), "{table}");
    assert!(table.contains("2.000 ms"), "{table}");
    assert!(table.contains("verdict"), "{table}");
}

#[test]
fn compare_pairs_the_latest_result_per_key_and_names_what_the_key_leaves_out() {
    let path = scratch("compare");
    // A: an older run, then the latest, published out of order.
    let a_old = run(1, "cat", 'a', 1_000, 1_000);
    let mut a_new = run(2, "cat", 'a', 2_000, 1_000);
    a_new.iterations[0] = iteration(1_000, None, &serde_json::json!({"nodes": 10}));
    let mut b = run(3, "cat", 'b', 3_000, 900);
    b.config = "[datasets.denmark]\nbbox = \"1,2,3,5\"\n".into();
    b.hostname = "other".into();
    b.iterations[0] = iteration(900, Some(""), &serde_json::json!({"nodes": 9}));
    // Never compared: a dirty run and a failed one at B.
    let mut dirty = run(4, "cat", 'b', 9_000, 1);
    dirty.dirty = true;
    let mut failed = run(5, "cat", 'b', 9_500, 1);
    failed.outcome = RunOutcome::Failed { reason: "x".into() };
    // One-sided: the same bench under a collection param.
    let mut scalar_vs_many = run(6, "inspect", 'a', 1_500, 100);
    scalar_vs_many.params = BTreeMap::from([("dataset".into(), param(true, &[("denmark", None)]))]);
    let inspect_b = run(7, "inspect", 'b', 1_600, 100);
    publish(
        &path,
        &[a_new, a_old, b, dirty, failed, scalar_vs_many, inspect_b],
    );
    let reader = reader(&path);
    let c = reader.compare_sides("aaaa", "bbbb", &filter()).unwrap();
    assert_eq!(
        c.b.len(),
        2,
        "dirty and failed runs never stand for a commit"
    );
    let out = compare(&c, "aaaa", "bbbb", 20);
    assert!(out.contains("1.000 ms  0.900 ms  -10.0%"), "{out}");
    assert!(
        out.contains("config differs: datasets.denmark.bbox \"1,2,3,4\" -> \"1,2,3,5\""),
        "{out}"
    );
    assert!(out.contains("host: host bygg vs other"), "{out}");
    assert!(out.contains("verdict: nil -> \"\""), "{out}");
    assert!(out.contains("record: nodes 10 -> 9"), "{out}");
    assert!(
        out.contains("latest inspect run on B, 00000000: dataset collection vs scalar"),
        "{out}"
    );
    assert!(
        out.contains("latest inspect run on A, 00000000: dataset scalar vs collection"),
        "{out}"
    );

    let capped = compare(&c, "aaaa", "bbbb", 1);
    assert!(
        capped.contains("(2 more pairs not shown - raise --limit)"),
        "{capped}"
    );
    assert!(reader.compare_sides("", "bbbb", &filter()).is_err());
}

/// `run` as a `--hotpath` run whose one child captured `decode` with one
/// function of `calls` calls, and a second stage on A only.
fn profiled(uuid_tail: u8, commit: char, started: i64, calls: u64, extra: bool) -> RunDocument {
    let mut doc = run(uuid_tail, "cat", commit, started, 1_000);
    doc.measurement = Measurement::Hotpath;
    let report = |calls: u64| CaptureOutcome::Captured {
        report: serde_json::json!({"functions_timing": {"data": [
            {"name": "decode", "calls": calls, "avg": "2 ms", "total": "24 ms",
             "percent_total": "40%", "p50": "1.8 ms", "p95": "3.0 ms", "p99": "3.8 ms"}
        ]}}),
    };
    let base = doc.iterations[0].children[0].clone();
    let mut children = vec![Child {
        hotpath: Some(HotpathCapture {
            stage: "decode".into(),
            occurrence: 1,
            outcome: report(calls),
        }),
        ..base.clone()
    }];
    if extra {
        children.push(Child {
            hotpath: Some(HotpathCapture {
                stage: "write".into(),
                occurrence: 1,
                outcome: report(1),
            }),
            ..base
        });
    }
    doc.iterations[0].children = children;
    doc
}

/// A capture is shown on its child and rendered under its stage; a compare
/// of two profiled runs diffs the stage both hold and names the one only
/// one side holds; profiled and timing runs never pair.
#[test]
fn captures_render_per_stage_and_compare_by_stage() {
    let path = scratch("captures");
    let a = profiled(1, 'a', 1_000, 12, true);
    let b = profiled(2, 'b', 2_000, 20, false);
    let timing_b = run(3, "cat", 'b', 3_000, 900);
    publish(&path, &[a.clone(), b, timing_b]);
    let reader = reader(&path);

    let shown = single(&reader, &a.uuid);
    assert!(shown.contains("measured    hotpath (--hotpath)"), "{shown}");
    assert!(shown.contains("hotpath decode #1: captured"), "{shown}");
    assert!(shown.contains("hotpath write #1 (iteration 0)"), "{shown}");

    let c = reader.compare_sides("a", "b", &filter()).unwrap();
    assert!(
        c.a[0].children_loaded,
        "a profiled run is read with its children"
    );
    assert!(
        c.b.iter().any(|r| !r.children_loaded),
        "a timing run is not"
    );
    let out = compare(&c, "a", "b", 20);
    assert!(out.contains("hotpath decode #1"), "{out}");
    assert!(
        out.contains("hotpath captures unpaired: write #1 on A only"),
        "{out}"
    );
    assert!(out.contains("measured timing vs hotpath"), "{out}");
}

#[test]
fn a_collapsed_arm_is_disclosed_and_an_equal_context_pair_pointed_at() {
    let path = scratch("arms");
    let with = |mut doc: RunDocument, value: &str| {
        doc.conditions
            .captured_env
            .insert("FEATURE".into(), value.into());
        doc
    };
    publish(
        &path,
        &[
            with(run(1, "cat", 'a', 1_000, 1_000), "0"),
            with(run(2, "cat", 'a', 2_000, 1_100), "1"),
            with(run(3, "cat", 'b', 1_500, 1_200), "1"),
            with(run(4, "cat", 'b', 2_500, 900), "0"),
        ],
    );
    let reader = reader(&path);
    let out = compare(
        &reader.compare_sides("a", "b", &filter()).unwrap(),
        "a",
        "b",
        20,
    );
    assert!(out.contains("env: FEATURE=1 vs 0"), "{out}");
    assert!(
        out.contains("other runs under this key with a different env or build: A 1 (newest"),
        "{out}"
    );
    // The pair whose older run is newest among equal-context pairs: A run 1
    // (1000) with B run 4 (2500) against A run 2 (2000) with B run 3 (1500).
    let a2 = format!("{:032x}", 2u128 | (0xabcd << 64));
    let b3 = format!("{:032x}", 3u128 | (0xabcd << 64));
    assert!(
        out.contains(&format!(
            "older pair with equal captured env and requested build: {} vs {}",
            &a2[..8],
            &b3[..8]
        )),
        "{out}"
    );

    // Within the query: --env FEATURE=0 leaves only the off arm on both sides.
    let filtered = reader
        .compare_sides(
            "a",
            "b",
            &BenchFilter {
                env: vec![("FEATURE".into(), "0".into())],
                ..filter()
            },
        )
        .unwrap();
    let out = compare(&filtered, "a", "b", 20);
    assert!(!out.contains("older pair"), "{out}");
    assert!(!out.contains("other runs"), "{out}");
}

/// Runs stored under an earlier name list, filter, pair and compare as the
/// chain's end, each view disclosing the name it was stored as.
#[test]
fn a_renamed_bench_lists_filters_and_pairs_as_its_new_name() {
    use super::super::store::{Mark, MarkRequest};
    // Renaming onto a name with runs is refused, so `concat`'s run is
    // published after the rename, joining the renamed history.
    let path = scratch("renamed");
    let old = run(1, "cat", 'a', 1_000, 100);
    publish(&path, std::slice::from_ref(&old));
    evidence::open_store(&path)
        .unwrap()
        .mark(&MarkRequest {
            uuid: "e".repeat(32),
            project: "pbfhogg".into(),
            at_unix_ms: 1_762_000_000_000,
            hostname: "bygg".into(),
            mark: Mark::Rename {
                from: "cat".into(),
                to: "concat".into(),
            },
        })
        .unwrap();
    publish(&path, &[run(2, "concat", 'b', 2_000, 90)]);
    let reader = reader(&path);

    let listing = reader
        .table(&BenchFilter {
            bench: Some("concat".into()),
            ..filter()
        })
        .unwrap();
    assert_eq!(listing.runs.len(), 2);
    let table = format_table(&listing.runs, &listing.marks);
    assert!(table.contains("concat (as cat)"), "{table}");

    let shown = single(&reader, &old.uuid);
    assert!(
        shown.contains("concat (stored as cat; project pbfhogg)"),
        "{shown}"
    );

    let out = compare(
        &reader.compare_sides("a", "b", &filter()).unwrap(),
        "a",
        "b",
        20,
    );
    assert_eq!(
        out.lines().filter(|l| l.starts_with("concat")).count(),
        1,
        "{out}"
    );
    assert!(out.contains("stored as: A cat, B concat"), "{out}");
    assert!(out.contains("-10.0%"), "{out}");
}

/// A retired bench's runs leave the table and the comparison, come back
/// marked under `--hidden`, and a uuid lookup shows one marked regardless.
#[test]
fn a_retired_bench_is_hidden_until_asked_for_and_marked_when_shown() {
    use super::super::store::{Mark, MarkRequest};
    let path = scratch("retired");
    let old = run(1, "old", 'a', 1_000, 100);
    publish(
        &path,
        &[
            old.clone(),
            run(2, "old", 'b', 2_000, 90),
            run(3, "cat", 'a', 3_000, 100),
        ],
    );
    evidence::open_store(&path)
        .unwrap()
        .mark(&MarkRequest {
            uuid: "e".repeat(32),
            project: "pbfhogg".into(),
            at_unix_ms: 1_762_000_000_000,
            hostname: "bygg".into(),
            mark: Mark::Retire {
                name: "old".into(),
                reason: "replaced by cat".into(),
            },
        })
        .unwrap();
    let reader = reader(&path);
    let hidden = BenchFilter {
        hidden: true,
        ..filter()
    };

    let listing = reader.table(&filter()).unwrap();
    assert_eq!(benches(&listing.runs), vec![("cat".into(), 3_000)]);
    let listing = reader.table(&hidden).unwrap();
    assert_eq!(listing.runs.len(), 3);
    let table = format_table(&listing.runs, &listing.marks);
    assert!(table.lines().next().unwrap().ends_with("hidden"), "{table}");
    assert!(table.contains("retired"), "{table}");

    let c = reader.compare_sides("a", "b", &filter()).unwrap();
    assert!(c.a.iter().chain(&c.b).all(|r| r.doc.bench == "cat"));
    let out = compare(
        &reader.compare_sides("a", "b", &hidden).unwrap(),
        "a",
        "b",
        20,
    );
    assert!(
        out.contains("hidden on A, 00000000: retired at 2025-11-01 12:26:40 on bygg: replaced by cat (mark eeeeeeee)"),
        "{out}"
    );
    assert!(out.contains("hidden on B"), "{out}");

    let shown = single(&reader, &old.uuid);
    assert!(
        shown.contains("hidden      retired at 2025-11-01 12:26:40 on bygg: replaced by cat"),
        "{shown}"
    );
}
