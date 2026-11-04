//! The HTTP layer: hyper's HTTP/1.1 server on tokio, one request per
//! connection, every bound from [`Limits`].
//!
//! A request is routed and checked from its head alone (path, method,
//! protocol, declared length), then admitted (an in-flight slot and a byte
//! reservation) before its body is polled, so an unadmitted body is never
//! read: hyper sends no `100 Continue` for it, and the refusal goes out at
//! once. Delivery of such an early answer is best effort; a peer still
//! sending may see the connection reset instead. An admitted body is read
//! under its own deadline and handed to the worker as a job; the handler
//! waits for the reply until the job's deadline.
//!
//! Every answer this layer writes carries the protocol header and, for a
//! failure, the wire envelope. hyper's own refusals of malformed HTTP carry
//! neither, and a client classifies them by status alone.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::mpsc::TrySendError;
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};

use crate::Limits;
use crate::store::wire::{self, Kind, Reply};
use crate::worker::{self, Job, Queue, Waiting};

/// A byte reservation's unit: the budget counts KiB, so a request cap fits a
/// semaphore's per-call count.
const BUDGET_UNIT: u64 = 1024;

/// What every connection shares.
pub(crate) struct Shared {
    pub(crate) limits: Limits,
    queue: Queue,
    connections: Arc<Semaphore>,
    pub(crate) in_flight: Arc<Semaphore>,
    budget: Arc<Semaphore>,
}

impl Shared {
    pub(crate) fn new(limits: Limits, queue: Queue) -> Self {
        let units = usize::try_from(limits.budget_bytes / BUDGET_UNIT).unwrap_or(usize::MAX);
        Self {
            limits,
            queue,
            connections: Arc::new(Semaphore::new(limits.connections)),
            in_flight: Arc::new(Semaphore::new(limits.in_flight)),
            budget: Arc::new(Semaphore::new(units.min(Semaphore::MAX_PERMITS))),
        }
    }
}

/// Accept connections until `stop` resolves or the worker stops; the latter
/// is an error, so the process exits non-zero and its manager restarts it.
pub(crate) async fn serve(
    listener: TcpListener,
    shared: Arc<Shared>,
    mut worker_gone: oneshot::Receiver<()>,
    stop: impl Future<Output = ()>,
) -> Result<(), String> {
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut worker_gone => return Err("the store worker stopped".to_owned()),
            () = &mut stop => return Ok(()),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let Ok(slot) = Arc::clone(&shared.connections).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        connection(stream, shared).await;
                        drop(slot);
                    });
                }
                Err(error) => {
                    // Out of descriptors, say: back off rather than spin.
                    crate::log(&format!("accept failed: {error}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
}

async fn connection(stream: TcpStream, shared: Arc<Shared>) {
    let limits = shared.limits;
    let service = service_fn(move |request| {
        let shared = Arc::clone(&shared);
        async move { Ok::<_, Infallible>(handle(request, &shared).await) }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.head)
        .keep_alive(false)
        .max_buf_size(64 * 1024);
    let served = builder.serve_connection(TokioIo::new(stream), service);
    // A connection past its deadline is cut; a client that stalled or went
    // away needs no log line.
    if let Ok(Err(_)) | Err(_) = tokio::time::timeout(limits.connection, served).await {}
}

/// The status a failure is answered with. Advisory: a client reads the
/// envelope.
fn status(kind: Kind) -> StatusCode {
    match kind {
        Kind::Refused | Kind::BadRequest => StatusCode::BAD_REQUEST,
        Kind::Conflict => StatusCode::CONFLICT,
        Kind::Failed | Kind::CommitUncertain | Kind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        Kind::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        Kind::Busy => StatusCode::SERVICE_UNAVAILABLE,
        Kind::ReadTimeout => StatusCode::REQUEST_TIMEOUT,
        Kind::WaitExpired { .. } => StatusCode::GATEWAY_TIMEOUT,
    }
}

fn respond(code: StatusCode, body: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(code)
        .header(CONTENT_TYPE, "application/json")
        .header(wire::PROTOCOL_HEADER, wire::PROTOCOL_VERSION.to_string())
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

fn reply(answer: Reply) -> Response<Full<Bytes>> {
    match answer.failure {
        None => respond(StatusCode::OK, answer.body),
        Some(kind) => {
            let code = status(kind);
            log_failure(code, kind);
            respond(code, answer.body)
        }
    }
}

fn refuse(code: StatusCode, kind: Kind, message: impl Into<String>) -> Response<Full<Bytes>> {
    log_failure(code, kind);
    respond(code, Reply::failure(kind, message).body)
}

/// One stderr line per failure answered, whichever layer answered it.
fn log_failure(code: StatusCode, kind: Kind) {
    crate::log(&format!("answered {} {}", code.as_u16(), kind.name()));
}

/// The server's identity, for a health check: no store access.
fn identity() -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "server": "brokkr-results-server",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": wire::PROTOCOL_VERSION,
    });
    respond(StatusCode::OK, body.to_string().into_bytes())
}

async fn handle(request: Request<Incoming>, shared: &Shared) -> Response<Full<Bytes>> {
    if request.uri().path() != wire::PATH {
        return refuse(StatusCode::NOT_FOUND, Kind::BadRequest, "no such route");
    }
    match *request.method() {
        Method::GET => return identity(),
        Method::POST => {}
        _ => {
            return refuse(
                StatusCode::METHOD_NOT_ALLOWED,
                Kind::BadRequest,
                "only GET and POST are served",
            );
        }
    }
    let protocol = request
        .headers()
        .get(wire::PROTOCOL_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    if protocol != Some(wire::PROTOCOL_VERSION.to_string().as_str()) {
        return refuse(
            StatusCode::BAD_REQUEST,
            Kind::BadRequest,
            format!(
                "this server speaks protocol {}; the request names {}",
                wire::PROTOCOL_VERSION,
                protocol.unwrap_or("none")
            ),
        );
    }
    let length = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let Some(length) = length else {
        return refuse(
            StatusCode::LENGTH_REQUIRED,
            Kind::BadRequest,
            "a request needs a Content-Length",
        );
    };
    if length > shared.limits.max_request_bytes {
        return refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            Kind::TooLarge,
            format!(
                "the request is {length} bytes; this server reads at most {}",
                shared.limits.max_request_bytes
            ),
        );
    }
    let busy = || {
        refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            Kind::Busy,
            "the server is at capacity",
        )
    };
    let Ok(slot) = Arc::clone(&shared.in_flight).try_acquire_owned() else {
        return busy();
    };
    let units = u32::try_from(length.div_ceil(BUDGET_UNIT).max(1)).unwrap_or(u32::MAX);
    let Ok(reservation) = Arc::clone(&shared.budget).try_acquire_many_owned(units) else {
        return busy();
    };
    let cap = usize::try_from(length).unwrap_or(usize::MAX);
    let read = Limited::new(request.into_body(), cap).collect();
    let body = match tokio::time::timeout(shared.limits.body, read).await {
        Err(_) => {
            return refuse(
                StatusCode::REQUEST_TIMEOUT,
                Kind::ReadTimeout,
                "the request body did not arrive in time",
            );
        }
        Ok(Err(error)) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                Kind::BadRequest,
                format!("the request body could not be read: {error}"),
            );
        }
        Ok(Ok(collected)) => collected.to_bytes(),
    };
    if u64::try_from(body.len()).ok() != Some(length) {
        return refuse(
            StatusCode::BAD_REQUEST,
            Kind::BadRequest,
            "the request body does not match its Content-Length",
        );
    }
    reply(submit(shared, body, vec![slot, reservation]).await)
}

/// Queue a body for the worker and wait for its reply until the job's
/// deadline. However this future ends, its `Waiting` withdraws a job that has
/// not started.
pub(crate) async fn submit(
    shared: &Shared,
    body: Bytes,
    held: Vec<tokio::sync::OwnedSemaphorePermit>,
) -> Reply {
    let state = Arc::new(AtomicU8::new(worker::QUEUED));
    let waiting = Waiting(Arc::clone(&state));
    let deadline = Instant::now() + shared.limits.wait;
    let (sender, receiver) = oneshot::channel();
    let job = Job {
        body,
        state: Arc::clone(&state),
        deadline,
        reply: sender,
        held,
    };
    match shared.queue.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            return Reply::failure(Kind::Busy, "the store's queue is full");
        }
        Err(TrySendError::Disconnected(_)) => {
            return Reply::failure(Kind::Busy, "the store worker has stopped");
        }
    }
    let answer = match tokio::time::timeout_at(deadline.into(), receiver).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => worker::unanswered(&state, false),
        Err(_) => worker::unanswered(&state, true),
    };
    drop(waiting);
    answer
}
