//! The results server client against a scripted HTTP/1.1 server on a
//! loopback port: what each answer classifies as, the publication latch over
//! the wire, and reads through `brokkr results`' reader against a real store
//! behind `wire::serve`.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::publication::Policy;
use super::super::results::BenchReader;
use super::super::results::tests::{filter, run};
use super::super::store::wire;
use super::{Client, refusal};
use crate::config::ResultsServer;

/// Retries without pauses, so a test never waits on a backoff.
const QUICK: Policy = Policy {
    window: Duration::from_secs(30),
    attempts: 4,
    first_delay: Duration::ZERO,
    max_delay: Duration::ZERO,
};

/// One request the scripted server read.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    protocol: Option<String>,
    body: Vec<u8>,
}

/// What the scripted server does with a request.
enum Reply {
    Answer(u16, Vec<u8>),
    /// Close the connection without answering, after reading the request.
    Hang,
    /// Announce `declared` body bytes, send `body`, then close.
    Short(u16, Vec<u8>, usize),
}

/// A loopback server answering every connection with `respond`, recording
/// what it read. Its thread outlives the test, idle on `accept`.
fn serve(
    respond: impl FnMut(&Seen) -> Reply + Send + 'static,
) -> (ResultsServer, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let mut respond = respond;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            log.lock().unwrap().push(request.clone());
            let reply = match respond(&request) {
                Reply::Answer(status, body) => {
                    let declared = body.len();
                    Some((status, body, declared))
                }
                Reply::Short(status, body, declared) => Some((status, body, declared)),
                Reply::Hang => None,
            };
            if let Some((status, body, declared)) = reply {
                let head = format!(
                    "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\ncontent-length: {declared}\r\nconnection: close\r\n\r\n"
                );
                let written = stream
                    .write_all(head.as_bytes())
                    .and_then(|()| stream.write_all(&body));
                drop(written);
            }
        }
    });
    let server = ResultsServer::parse(&format!("http://{address}")).unwrap();
    (server, seen)
}

fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    while !buffer.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        buffer.push(byte[0]);
    }
    let head = String::from_utf8(buffer).ok()?;
    let mut lines = head.split("\r\n");
    let mut start = lines.next()?.split(' ');
    let method = start.next()?.to_owned();
    let path = start.next()?.to_owned();
    let mut length = 0usize;
    let mut protocol = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().ok()?,
            "brokkr-bench-protocol" => protocol = Some(value.to_owned()),
            _ => {}
        }
    }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        path,
        protocol,
        body,
    })
}

fn identity(protocol: u32) -> Reply {
    Reply::Answer(
        200,
        serde_json::json!({"server": "brokkr-results-server", "version": "0", "protocol": protocol})
            .to_string()
            .into_bytes(),
    )
}

fn envelope(status: u16, kind: wire::Kind) -> Reply {
    Reply::Answer(status, wire::Failure::new(kind, "scripted").encode())
}

fn client(server: &ResultsServer) -> Client {
    Client::with_policies(server, QUICK, QUICK)
}

/// A real store behind the wire, as the server runs it.
fn backed_by(name: &str) -> impl FnMut(&Seen) -> Reply + Send + 'static {
    let path = crate::test_scratch::scratch("bench_remote", name).join("bench.db");
    let mut store = super::super::evidence::open_store(&path).unwrap();
    move |seen| {
        if seen.method == "GET" {
            return identity(wire::PROTOCOL_VERSION);
        }
        let reply = wire::serve(&mut store, &seen.body);
        Reply::Answer(if reply.failure.is_none() { 200 } else { 400 }, reply.body)
    }
}

#[test]
fn a_run_publishes_and_reads_back_through_the_wire_with_the_protocol_on_every_request() {
    let (server, seen) = serve(backed_by("round_trip"));
    let client = client(&server);
    client.probe().unwrap();
    let documents = [
        run(1, "cat", 'a', 1_000, 500),
        run(2, "cat", 'b', 2_000, 400),
    ];
    for document in &documents {
        client.publish(document).unwrap();
    }
    // A replay is answered, not applied twice.
    client.publish(&documents[0]).unwrap();

    let reader = BenchReader::remote(Client::with_policies(&server, QUICK, QUICK), "pbfhogg");
    assert!(reader.has_runs().unwrap());
    let table = reader.table(&filter()).unwrap();
    assert_eq!(
        table.iter().map(|r| r.doc.uuid.clone()).collect::<Vec<_>>(),
        [documents[1].uuid.clone(), documents[0].uuid.clone()]
    );
    assert!(table.iter().all(|r| !r.children_loaded));
    let found = reader.lookup(&documents[0].uuid[..], &filter()).unwrap();
    assert_eq!(found.runs.len(), 1);
    assert!(found.runs[0].children_loaded);
    assert_eq!(found.runs[0].doc, documents[0]);
    let (a, b) = reader.compare_sides("a", "b", &filter()).unwrap();
    assert_eq!((a.len(), b.len()), (1, 1));
    let elsewhere = BenchReader::remote(client, "dellingr")
        .lookup(&documents[0].uuid[..], &filter())
        .unwrap();
    assert_eq!(elsewhere.elsewhere.len(), 1);

    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|s| s.path == "/bench"), "{seen:?}");
    assert!(
        seen.iter().all(|s| s.protocol.as_deref() == Some("1")),
        "{seen:?}"
    );
}

#[test]
fn an_uncertain_publication_stays_uncertain_across_a_later_refusal() {
    let mut replies = vec![Reply::Hang, envelope(409, wire::Kind::Conflict)].into_iter();
    let (server, _) = serve(move |_| replies.next().unwrap());
    let error = client(&server)
        .publish(&run(3, "cat", 'a', 1_000, 500))
        .unwrap_err()
        .to_string();
    assert!(error.contains("may or may not have been stored"), "{error}");
    assert!(error.contains("perhaps after the request left"), "{error}");
}

#[test]
fn a_200_acknowledges_a_publication_whatever_its_body() {
    let mut replies = vec![
        Reply::Answer(502, b"<html>bad gateway</html>".to_vec()),
        Reply::Answer(200, b"not json".to_vec()),
    ]
    .into_iter();
    let (server, seen) = serve(move |_| replies.next().unwrap());
    client(&server)
        .publish(&run(4, "cat", 'a', 1_000, 500))
        .unwrap();
    // The same bytes went out both times.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, seen[1].body);
    drop(seen);

    // A 200 whose body is cut short cannot be read at all, and still
    // acknowledges: no second request is made.
    let (server, seen) = serve(|_| Reply::Short(200, b"{\"published\"".to_vec(), 4096));
    client(&server)
        .publish(&run(8, "cat", 'a', 1_000, 500))
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn a_refusal_before_any_uncertainty_is_final_and_not_stored() {
    let (server, seen) = serve(|_| envelope(400, wire::Kind::Refused));
    let error = client(&server)
        .publish(&run(5, "cat", 'a', 1_000, 500))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("was not stored") && error.contains("refused (HTTP 400)"),
        "{error}"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[test]
fn an_unreachable_server_fails_reads_probes_and_publications_without_uncertainty() {
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let server = ResultsServer::parse(&format!("http://{address}")).unwrap();
    let client = client(&server);
    let error = client.probe().unwrap_err().to_string();
    assert!(
        error.contains(&server.shown) && error.contains("cannot reach it"),
        "{error}"
    );
    let error = client
        .publish(&run(6, "cat", 'a', 1_000, 500))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("was not stored") && error.contains("4 attempts"),
        "{error}"
    );
    let error = BenchReader::remote(client, "pbfhogg")
        .has_runs()
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot reach it"), "{error}");
}

#[test]
fn the_probe_refuses_another_protocol_and_a_run_answer_must_load_what_was_asked() {
    let (server, _) = serve(|_| identity(wire::PROTOCOL_VERSION + 1));
    let error = client(&server).probe().unwrap_err().to_string();
    assert!(error.contains("upgrade them together"), "{error}");

    // A server answering a summary where the whole run was asked for.
    let mut stored = None;
    let (server, _) = serve(move |seen| {
        let request: wire::Request = serde_json::from_slice(&seen.body).unwrap();
        match request {
            wire::Request::Select { .. } => Reply::Answer(
                200,
                serde_json::to_vec(&wire::Answer::Runs(vec![run(7, "cat", 'a', 1, 2).uuid]))
                    .unwrap(),
            ),
            wire::Request::Run { .. } => {
                let run = stored.get_or_insert_with(|| {
                    let path =
                        crate::test_scratch::scratch("bench_remote", "summary").join("bench.db");
                    let mut store = super::super::evidence::open_store(&path).unwrap();
                    store.publish(&run(7, "cat", 'a', 1, 2)).unwrap();
                    store
                        .run(&run(7, "cat", 'a', 1, 2).uuid, false)
                        .unwrap()
                        .unwrap()
                });
                Reply::Answer(
                    200,
                    serde_json::to_vec(&wire::Answer::Run(Some(Box::new(run.clone())))).unwrap(),
                )
            }
            _ => panic!("unexpected request"),
        }
    });
    let reader = BenchReader::remote(client(&server), "pbfhogg");
    let Err(error) = reader.lookup(&run(7, "cat", 'a', 1, 2).uuid[..], &filter()) else {
        panic!("a summary answering a whole-run read must be refused");
    };
    let error = error.to_string();
    assert!(
        error.contains("children_loaded false, asked true"),
        "{error}"
    );
}

#[test]
fn an_answer_without_an_envelope_is_classified_by_its_status_alone() {
    // A bare 4xx: refused before any brokkr server ran it; 408 and 429 retry.
    for (status, retry) in [
        (400, false),
        (404, false),
        (413, false),
        (408, true),
        (429, true),
    ] {
        let failure = refusal(status, b"<html/>", None);
        assert!(
            failure.unapplied && failure.retry == retry,
            "{status}: {failure:?}"
        );
    }
    // Anything else may have reached the server, a proxy's 502 and 503 too.
    for status in [500, 502, 503, 504, 301, 204] {
        let failure = refusal(status, b"", None);
        assert!(!failure.unapplied && failure.retry, "{status}: {failure:?}");
    }
    // An envelope decides over its status.
    let busy = wire::Failure::new(wire::Kind::Busy, "full").encode();
    let failure = refusal(503, &busy, None);
    assert!(failure.unapplied && failure.retry, "{failure:?}");
    let uncertain = wire::Failure::new(wire::Kind::CommitUncertain, "disk").encode();
    assert!(!refusal(400, &uncertain, None).unapplied);
}
