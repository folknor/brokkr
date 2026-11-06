//! The server against its contract: the store's wire answered over real
//! HTTP on a loopback port, the refusals made from a request's head, the
//! cancellation races between a handler and the worker, and ownership of the
//! file.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::Limits;
use crate::store::wire::{self, Failure, Kind};
use crate::store::{
    Build, Conditions, DOCUMENT_FORMAT, Measurement, RunDocument, RunOutcome, Store, StoreError,
};
use crate::worker::{self, Job, RUNNING, WITHDRAWN};

/// A fresh directory under the crate's gitignored `target/test-tmp/`; each
/// test names its own.
pub(crate) fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-tmp/results-server")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn document(uuid: &str, project: &str) -> RunDocument {
    RunDocument {
        format: DOCUMENT_FORMAT,
        uuid: uuid.into(),
        project: project.into(),
        bench: "cat".into(),
        checkout: "/src/demo".into(),
        hostname: "host".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        dirty: false,
        subject: None,
        started_unix_ms: 1_762_000_000_000,
        brokkr_version: "0.1.0".into(),
        argv: vec!["brokkr".into(), "bench".into(), "cat".into()],
        conditions: Conditions {
            kernel: None,
            cpu_governor: None,
            avail_memory_mb: None,
            captured_env: BTreeMap::new(),
        },
        build: Build {
            target: None,
            profile: "release".into(),
            features: Vec::new(),
            default_features: true,
            resolved: None,
            artifact: None,
        },
        measurement: Measurement::Timing,
        params: BTreeMap::new(),
        config: "project = \"demo\"\n".into(),
        iterations_requested: 1,
        outcome: RunOutcome::Interrupted,
        iterations: Vec::new(),
    }
}

const UUID: &str = "0123456789abcdef0123456789abcdef";

/// The executable digest a test server reports.
pub(crate) const BUILD: &str = "00112233445566778899aabbccddeeff";

/// A running server on a loopback port; dropping it stops the runtime.
pub(crate) struct Server {
    pub(crate) addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

pub(crate) fn server(name: &str, limits: Limits) -> Server {
    let dir = scratch(name);
    let owned = crate::owner::own(&dir.join("bench.db")).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (queue, gone) = worker::start(owned.store, limits.queue).unwrap();
    let shared = Arc::new(crate::http::Shared::new(limits, queue, Some(BUILD.into())));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let lock = owned.lock;
    runtime.spawn(async move {
        let ended = crate::http::serve(listener, shared, gone, async {
            drop(stopped.await);
        })
        .await;
        drop(lock);
        ended
    });
    Server {
        addr,
        stop: Some(stop),
        runtime: Some(runtime),
    }
}

struct Answer {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn failure(&self) -> Failure {
        Failure::decode(&self.body).unwrap()
    }

    fn has_protocol(&self) -> bool {
        self.headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&protocol_header()))
    }
}

/// The protocol header line this build sends and answers with.
fn protocol_header() -> String {
    format!("brokkr-bench-protocol: {}", wire::PROTOCOL_VERSION)
}

/// One raw HTTP/1.1 exchange; the server closes the connection after its
/// answer.
fn exchange(addr: SocketAddr, head: &str, body: &[u8]) -> Answer {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(&raw)));
    let headers = String::from_utf8(raw[..split].to_vec()).unwrap();
    let status = headers.split(' ').nth(1).unwrap().parse().unwrap();
    Answer {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

fn post(addr: SocketAddr, request: &wire::Request) -> Answer {
    let body = serde_json::to_vec(request).unwrap();
    let head = format!(
        "POST /bench HTTP/1.1\r\nHost: test\r\n{}\r\nContent-Length: {}\r\n\r\n",
        protocol_header(),
        body.len()
    );
    exchange(addr, &head, &body)
}

fn publish(document: &RunDocument) -> wire::Request {
    wire::Request::Publish {
        document: Box::new(document.clone()),
    }
}

#[test]
fn publication_replay_conflict_and_reads_answer_over_http() {
    let server = server("roundtrip", Limits::DEFAULT);
    let doc = document(UUID, "demo");

    let first = post(server.addr, &publish(&doc));
    assert_eq!(first.status, 200);
    assert!(first.has_protocol(), "{}", first.headers);
    assert_eq!(first.json(), serde_json::json!({"published": "stored"}));
    let replay = post(server.addr, &publish(&doc));
    assert_eq!(
        replay.json(),
        serde_json::json!({"published": "already_stored"})
    );

    let mut changed = doc.clone();
    changed.bench = "inspect".into();
    let conflict = post(server.addr, &publish(&changed));
    assert_eq!(conflict.status, 409);
    assert!(conflict.has_protocol());
    let classified = conflict.failure().classify(Some(UUID));
    assert!(classified.unapplied && !classified.retry);
    assert_eq!(
        classified.store,
        Some(StoreError::Conflict { uuid: UUID.into() })
    );

    let mut other = document("ffffffffffffffffffffffffffffffff", "other");
    other.started_unix_ms += 1;
    assert_eq!(post(server.addr, &publish(&other)).status, 200);

    let select = post(
        server.addr,
        &wire::Request::Select {
            filter: crate::store::RunFilter {
                project: "demo".into(),
                ..crate::store::RunFilter::default()
            },
        },
    );
    assert_eq!(select.json(), serde_json::json!({"runs": [UUID]}));

    let run = post(
        server.addr,
        &wire::Request::Run {
            uuid: UUID.into(),
            children: false,
        },
    );
    let answer: wire::Answer = serde_json::from_slice(&run.body).unwrap();
    let wire::Answer::Run(Some(stored)) = answer else {
        panic!("not a run: {answer:?}");
    };
    assert_eq!(stored.doc, doc);
    assert!(!stored.children_loaded);

    let missing = post(
        server.addr,
        &wire::Request::Run {
            uuid: "1".repeat(32),
            children: true,
        },
    );
    assert_eq!(missing.json(), serde_json::json!({"run": null}));

    let has = post(
        server.addr,
        &wire::Request::HasRuns {
            project: "nobody".into(),
        },
    );
    assert_eq!(has.json(), serde_json::json!({"has_runs": false}));

    let elsewhere = post(
        server.addr,
        &wire::Request::Elsewhere {
            project: "demo".into(),
            prefix: "FF".into(),
        },
    );
    assert_eq!(
        elsewhere.json(),
        serde_json::json!({"elsewhere": [["ffffffffffffffffffffffffffffffff", "other"]]})
    );
}

#[test]
fn a_request_is_refused_from_its_head_before_its_body_is_read() {
    let limits = Limits {
        max_request_bytes: 64,
        ..Limits::DEFAULT
    };
    let server = server("head", limits);
    let addr = server.addr;

    let identity = exchange(addr, "GET /bench HTTP/1.1\r\nHost: t\r\n\r\n", b"");
    assert_eq!(identity.status, 200);
    assert_eq!(identity.json()["protocol"], wire::PROTOCOL_VERSION);
    assert_eq!(identity.json()["build"], BUILD);
    assert!(identity.json().get("probe").is_none());
    assert!(
        identity
            .headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("cache-control: no-store")),
        "{}",
        identity.headers
    );

    let probed = exchange(
        addr,
        "GET /bench?probe=0a1b2c3d HTTP/1.1\r\nHost: t\r\n\r\n",
        b"",
    );
    assert_eq!(probed.status, 200);
    assert_eq!(probed.json()["probe"], "0a1b2c3d");
    for bad in ["probe=", "probe=XYZ", "probe=ab&probe=cd"] {
        let refused = exchange(
            addr,
            &format!("GET /bench?{bad} HTTP/1.1\r\nHost: t\r\n\r\n"),
            b"",
        );
        assert_eq!(refused.status, 400, "{bad}");
        assert_eq!(refused.failure().kind, "bad_request");
    }

    let route = exchange(addr, "GET /other HTTP/1.1\r\nHost: t\r\n\r\n", b"");
    assert_eq!(route.status, 404);
    assert_eq!(route.failure().kind, "bad_request");

    let no_protocol = exchange(
        addr,
        "POST /bench HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\n\r\n",
        b"{}",
    );
    assert_eq!(no_protocol.status, 400);
    let failure = no_protocol.failure();
    assert_eq!(failure.kind, "bad_request");
    assert_eq!(failure.applied, Some(false));
    assert!(!failure.retry);

    let other_protocol = exchange(
        addr,
        &format!(
            "POST /bench HTTP/1.1\r\nHost: t\r\nbrokkr-bench-protocol: {}\r\nContent-Length: 2\r\n\r\n",
            wire::PROTOCOL_VERSION + 1
        ),
        b"{}",
    );
    assert!(
        other_protocol
            .failure()
            .message
            .contains(&format!("protocol {}", wire::PROTOCOL_VERSION)),
        "{:?}",
        other_protocol.failure()
    );

    let header = protocol_header();
    let chunked = exchange(
        addr,
        &format!(
            "POST /bench HTTP/1.1\r\nHost: t\r\n{header}\r\nTransfer-Encoding: chunked\r\n\r\n"
        ),
        b"2\r\n{}\r\n0\r\n\r\n",
    );
    assert_eq!(chunked.status, 411);

    let large = exchange(
        addr,
        &format!("POST /bench HTTP/1.1\r\nHost: t\r\n{header}\r\nContent-Length: 65\r\n\r\n"),
        b"",
    );
    assert_eq!(large.status, 413);
    assert_eq!(large.failure().kind, "too_large");

    let malformed = exchange(
        addr,
        &format!("POST /bench HTTP/1.1\r\nHost: t\r\n{header}\r\nContent-Length: 12\r\n\r\n"),
        b"{\"drop\":{}}x",
    );
    assert_eq!(malformed.status, 400);
    assert_eq!(malformed.failure().kind, "bad_request");
}

#[test]
fn an_invalid_document_is_refused_by_the_store_and_capacity_answers_busy() {
    let server = server("refused", Limits::DEFAULT);
    let refused = post(server.addr, &publish(&document("NOT-A-UUID", "demo")));
    assert_eq!(refused.status, 400);
    let classified = refused.failure().classify(None);
    assert!(classified.unapplied && !classified.retry);
    assert!(matches!(classified.store, Some(StoreError::Refused(_))));

    let full = server_with_no_slots();
    let busy = post(full.addr, &publish(&document(UUID, "demo")));
    assert_eq!(busy.status, 503);
    assert!(busy.has_protocol());
    let failure = busy.failure();
    assert_eq!(failure.kind, "busy");
    let classified = failure.classify(Some(UUID));
    assert!(classified.unapplied && classified.retry);
    assert_eq!(classified.store, None);
}

fn server_with_no_slots() -> Server {
    server(
        "busy",
        Limits {
            in_flight: 0,
            ..Limits::DEFAULT
        },
    )
}

fn store(name: &str) -> Store {
    Store::open(&scratch(name).join("bench.db")).unwrap()
}

/// A submission on the real handler path against a queue the test drains by
/// hand, so it decides when the worker reaches the job. The handler runs on a
/// paused clock, so its wait expires only when the test blocks on it, never
/// while the test is claiming: no scheduling window decides a race.
struct Pending {
    runtime: tokio::runtime::Runtime,
    shared: Arc<crate::http::Shared>,
    jobs: std::sync::mpsc::Receiver<Job>,
}

impl Pending {
    fn new(wait: Duration) -> Self {
        let (queue, jobs) = std::sync::mpsc::sync_channel(4);
        let limits = Limits {
            wait,
            in_flight: 2,
            ..Limits::DEFAULT
        };
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap(),
            shared: Arc::new(crate::http::Shared::new(limits, queue, None)),
            jobs,
        }
    }

    /// Submit a publication holding one in-flight slot, as `handle` does,
    /// and take its job off the queue.
    fn submit(&self) -> (tokio::task::JoinHandle<wire::Reply>, Job) {
        let slot = Arc::clone(&self.shared.in_flight)
            .try_acquire_owned()
            .unwrap();
        let body = serde_json::to_vec(&publish(&document(UUID, "demo"))).unwrap();
        let shared = Arc::clone(&self.shared);
        let handler = self
            .runtime
            .spawn(async move { crate::http::submit(&shared, body.into(), vec![slot]).await });
        // Let the handler run until it has queued its job and waits.
        for _ in 0..100 {
            if let Ok(job) = self.jobs.try_recv() {
                return (handler, job);
            }
            self.runtime.block_on(tokio::task::yield_now());
        }
        panic!("the handler never queued its job");
    }

    fn free_slots(&self) -> usize {
        self.shared.in_flight.available_permits()
    }
}

fn state(job: &Job) -> u8 {
    job.state.load(std::sync::atomic::Ordering::Acquire)
}

/// A handler dropped (its connection gone) before the worker reached its
/// job: the job is withdrawn, keeps its admission while queued, never runs,
/// and releases the admission when the worker drops it.
#[test]
fn a_job_whose_handler_is_dropped_before_its_claim_never_runs() {
    let mut store = store("withdrawn");
    let pending = Pending::new(Duration::from_secs(60));
    let (handler, job) = pending.submit();
    assert_eq!(pending.free_slots(), 1);
    handler.abort();
    assert!(pending.runtime.block_on(handler).is_err());
    assert_eq!(state(&job), WITHDRAWN);
    assert_eq!(pending.free_slots(), 1, "a queued job keeps its admission");
    assert!(worker::process(&mut store, job));
    assert!(!store.has_runs("demo").unwrap());
    assert_eq!(pending.free_slots(), 2);
}

/// The handler's wait expires before the worker reaches the job: withdrawn
/// and proved unapplied, and the worker then drops it unrun.
#[test]
fn a_wait_expiring_before_the_claim_is_proved_unapplied() {
    let mut store = store("expired");
    let pending = Pending::new(Duration::from_secs(60));
    let (handler, job) = pending.submit();
    let reply = pending.runtime.block_on(handler).unwrap();
    assert_eq!(reply.failure, Some(Kind::WaitExpired { withdrawn: true }));
    assert_eq!(Failure::decode(&reply.body).unwrap().applied, Some(false));
    assert!(worker::process(&mut store, job));
    assert!(!store.has_runs("demo").unwrap());
    assert_eq!(pending.free_slots(), 2);
}

/// The worker claims the job, then the handler's wait expires: the handler
/// cannot withdraw it and answers uncertain, and the job still runs to its
/// end on the worker's path and lands.
#[test]
fn a_wait_expiring_after_the_claim_is_uncertain_and_the_job_still_lands() {
    let mut store = store("started");
    let pending = Pending::new(Duration::from_secs(60));
    let (handler, job) = pending.submit();
    assert!(worker::claim(&job.state, job.deadline));
    assert_eq!(state(&job), RUNNING);
    let reply = pending.runtime.block_on(handler).unwrap();
    assert_eq!(reply.failure, Some(Kind::WaitExpired { withdrawn: false }));
    let failure = Failure::decode(&reply.body).unwrap();
    assert_eq!(failure.applied, None);
    assert!(failure.retry);
    assert_eq!(pending.free_slots(), 1, "a running job keeps its admission");
    assert!(worker::run_claimed(&mut store, job));
    assert!(store.has_runs("demo").unwrap());
    assert!(!store.in_transaction());
    assert_eq!(pending.free_slots(), 2);
}

#[test]
fn one_file_has_one_server_whatever_it_is_called() {
    let dir = scratch("owner");
    let db = dir.join("bench.db");
    let held = crate::owner::own(&db).unwrap();
    let again = crate::owner::own(&db).err().unwrap();
    assert!(again.contains("another server holds"), "{again}");

    std::os::unix::fs::symlink(&db, dir.join("alias.db")).unwrap();
    let alias = crate::owner::own(&dir.join("alias.db")).err().unwrap();
    assert!(alias.contains("another server holds"), "{alias}");
    drop(held);

    std::fs::hard_link(&db, dir.join("second.db")).unwrap();
    let linked = crate::owner::own(&db).err().unwrap();
    assert!(linked.contains("hard links"), "{linked}");

    std::os::unix::fs::symlink(dir.join("nothing.db"), dir.join("dangling.db")).unwrap();
    assert!(crate::owner::own(&dir.join("dangling.db")).is_err());
    assert!(crate::owner::own(Path::new("relative.db")).is_err());
}
