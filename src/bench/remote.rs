//! brokkr's client of a results server: the store's wire contract
//! (`store::wire`, `reference/bench-store.md`, On the wire) over blocking
//! HTTP, for a user whose user-wide config names one (`results_server`).
//!
//! One attempt is one HTTP exchange. Its answer is classified by the failure
//! envelope when there is one (`wire::Failure::classify`), and by its status
//! alone when there is not: a bare 4xx is proved unapplied and final (408 and
//! 429 retryable), anything else uncertain and retryable. A transport error
//! is proved unapplied only when no byte of the request can have left.
//! Retries follow [`super::publication`]'s admission; an attempt is never cut
//! short of the server's own connection deadline, so a server that is not
//! stuck always answers or closes first.

use std::time::Duration;

use super::publication::{self, Admission, Attempt, Policy};
use super::store::wire;
use crate::config::ResultsServer;
use crate::error::DevError;

/// One attempt's whole wait, connecting to the last byte of the answer. It
/// outlasts the server's deadline by an allowance for a proxy in between.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(wire::CONNECTION_DEADLINE.as_secs() + 30);

/// Connecting alone, after which the attempt is known never sent.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

const _: () = assert!(ATTEMPT_TIMEOUT.as_secs() > wire::CONNECTION_DEADLINE.as_secs());
const _: () = assert!(CONNECT_TIMEOUT.as_secs() < ATTEMPT_TIMEOUT.as_secs());

/// What one exchange came to.
enum Exchange {
    /// A 200 whose body was read whole.
    Answered(Vec<u8>),
    /// A 200 whose body could not be read.
    Unreadable(String),
    Failed(Failure),
}

/// A failed exchange, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    unapplied: bool,
    retry: bool,
    message: String,
}

/// A configured results server.
pub(crate) struct Client {
    server: ResultsServer,
    agent: ureq::Agent,
    read: Policy,
    publish: Policy,
}

impl Client {
    pub(crate) fn new(server: &ResultsServer) -> Self {
        Self::with_policies(server, Policy::READ, Policy::PUBLISH)
    }

    pub(crate) fn with_policies(server: &ResultsServer, read: Policy, publish: Policy) -> Self {
        let config = ureq::Agent::config_builder()
            // Every status is an answer to classify, not an error.
            .http_status_as_error(false)
            // A request is never re-sent elsewhere, a publication least of all.
            .max_redirects(0)
            // The server takes one request per connection; every attempt
            // connects afresh, which is what lets a refused connection prove
            // an attempt unsent.
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(ATTEMPT_TIMEOUT))
            .build();
        Self {
            server: server.clone(),
            agent: ureq::Agent::new_with_config(config),
            read,
            publish,
        }
    }

    /// The server as messages name it.
    pub(crate) fn shown(&self) -> &str {
        &self.server.shown
    }

    fn named(&self, message: &str) -> DevError {
        DevError::Database(format!("results server {}: {message}", self.server.shown))
    }

    /// One `POST` of `body`.
    fn post(&self, body: &[u8], published: Option<&str>) -> Exchange {
        let sent = self
            .agent
            .post(&self.server.endpoint)
            .header(wire::PROTOCOL_HEADER, wire::PROTOCOL_VERSION.to_string())
            .header("content-type", "application/json")
            .send(body);
        answer(sent, published)
    }

    /// One `GET` of the server's identity.
    fn get(&self) -> Exchange {
        let sent = self
            .agent
            .get(&self.server.endpoint)
            .header(wire::PROTOCOL_HEADER, wire::PROTOCOL_VERSION.to_string())
            .call();
        answer(sent, None)
    }

    /// Retry `exchange` under the read policy until it answers, a failure is
    /// final, or admission ends it. Reads are idempotent, so nothing latches.
    fn retried(&self, mut exchange: impl FnMut() -> Exchange) -> Result<Vec<u8>, DevError> {
        let mut admission = Admission::new(self.read);
        loop {
            admission.attempt();
            let failure = match exchange() {
                Exchange::Answered(body) => return Ok(body),
                Exchange::Unreadable(why) => Failure {
                    unapplied: false,
                    retry: true,
                    message: format!("its answer could not be read: {why}"),
                },
                Exchange::Failed(failure) => failure,
            };
            if !failure.retry || !admission.another(false) {
                let attempts = crate::output::count(
                    usize::try_from(admission.made()).unwrap_or(usize::MAX),
                    "attempt",
                );
                return Err(self.named(&format!("{} ({attempts})", failure.message)));
            }
        }
    }

    /// The destination check before a run is measured: the server answers
    /// and speaks this brokkr's protocol. It proves reachability, not that a
    /// publication will be stored.
    pub(crate) fn probe(&self) -> Result<(), DevError> {
        let body = self.retried(|| self.get())?;
        let identity: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| self.named("its identity does not read as a brokkr results server's"))?;
        match identity.get("protocol").and_then(serde_json::Value::as_u64) {
            Some(protocol) if protocol == u64::from(wire::PROTOCOL_VERSION) => Ok(()),
            Some(protocol) => Err(self.named(&format!(
                "it speaks protocol {protocol}, this brokkr protocol {}: upgrade them together",
                wire::PROTOCOL_VERSION
            ))),
            None => Err(self.named("its identity names no protocol")),
        }
    }

    /// One read request, retried, and its answer.
    pub(crate) fn request(&self, request: &wire::Request) -> Result<wire::Answer, DevError> {
        let body = serde_json::to_vec(request)
            .map_err(|e| self.named(&format!("the request cannot be encoded: {e}")))?;
        let answer = self.retried(|| self.post(&body, None))?;
        serde_json::from_slice(&answer).map_err(|e| {
            self.named(&format!(
                "its answer does not decode as protocol {}: {e}",
                wire::PROTOCOL_VERSION
            ))
        })
    }

    /// Publish `document`, retried and latched ([`publication::publish`]).
    /// A 200 acknowledges the publication whatever its body says.
    pub(crate) fn publish(&self, document: &super::store::RunDocument) -> Result<(), DevError> {
        let request = wire::Request::Publish {
            document: Box::new(document.clone()),
        };
        // Encoded once: every retry sends the same bytes, under the same uuid.
        let body = serde_json::to_vec(&request).map_err(|e| {
            DevError::Database(format!("run {} cannot be encoded: {e}", document.uuid))
        })?;
        publication::publish(
            &document.uuid,
            &self.server.shown,
            self.publish,
            || match self.post(&body, Some(&document.uuid)) {
                Exchange::Answered(answer) => {
                    if !matches!(
                        serde_json::from_slice::<wire::Answer>(&answer),
                        Ok(wire::Answer::Published(_))
                    ) {
                        crate::output::warn(&format!(
                            "results server {} acknowledged run {} with an answer that does not read as a publication's",
                            self.server.shown, document.uuid
                        ));
                    }
                    Attempt::Settled
                }
                Exchange::Unreadable(why) => {
                    crate::output::warn(&format!(
                        "results server {} acknowledged run {}, but its answer could not be read: {why}",
                        self.server.shown, document.uuid
                    ));
                    Attempt::Settled
                }
                Exchange::Failed(failure) => Attempt::Failed {
                    unapplied: failure.unapplied,
                    retry: failure.retry,
                    message: failure.message,
                },
            },
        )
    }
}

/// Read what one exchange came to.
fn answer(
    sent: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    published: Option<&str>,
) -> Exchange {
    let mut response = match sent {
        Ok(response) => response,
        Err(error) => return Exchange::Failed(transport(&error)),
    };
    let status = response.status().as_u16();
    // Answers are not capped by the server, so neither is their reading.
    let body = response
        .body_mut()
        .with_config()
        .limit(u64::MAX)
        .read_to_vec();
    if status == 200 {
        return match body {
            Ok(body) => Exchange::Answered(body),
            Err(error) => Exchange::Unreadable(error.to_string()),
        };
    }
    Exchange::Failed(refusal(status, &body.unwrap_or_default(), published))
}

/// A non-200 answer: by its envelope, else by its status alone.
fn refusal(status: u16, body: &[u8], published: Option<&str>) -> Failure {
    if let Some(envelope) = wire::Failure::decode(body) {
        let class = envelope.classify(published);
        return Failure {
            unapplied: class.unapplied,
            retry: class.retry,
            message: format!("{} (HTTP {status}): {}", envelope.kind, envelope.message),
        };
    }
    let message = format!("HTTP {status} without an error envelope");
    if (400..500).contains(&status) {
        // Refused before any brokkr server ran it: by the proxy in front, or
        // by the HTTP layer itself on malformed HTTP.
        Failure {
            unapplied: true,
            retry: matches!(status, 408 | 429),
            message,
        }
    } else {
        // A proxy's 5xx (a 502 or 503 included), or anything else: the
        // request may have reached the server.
        Failure {
            unapplied: false,
            retry: true,
            message,
        }
    }
}

/// A transport error. Proved unsent only when the connection never existed:
/// the name did not resolve, connecting timed out, or it was refused. ureq
/// connects before it sends, makes one connection per attempt here (no
/// redirects, no idle pool) and retries nothing itself; and on Linux a TCP
/// socket reports `ECONNREFUSED` only from `SYN_SENT` (an established one
/// reports a reset as `ECONNRESET` or `EPIPE`), while ureq synthesizes
/// `ConnectionRefused` only in its connect loop. A change of transport (a
/// proxy protocol, another client) must re-establish this. Every other
/// error, the unreachable kinds included (an established socket can report
/// them too), may have followed a request that left.
fn transport(error: &ureq::Error) -> Failure {
    let unsent = match error {
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => true,
        ureq::Error::Timeout(timeout) => {
            matches!(timeout, ureq::Timeout::Resolve | ureq::Timeout::Connect)
        }
        ureq::Error::Io(io) => io.kind() == std::io::ErrorKind::ConnectionRefused,
        _ => false,
    };
    Failure {
        unapplied: unsent,
        retry: true,
        message: if unsent {
            format!("cannot reach it: {error}")
        } else {
            format!("the exchange failed, perhaps after the request left: {error}")
        },
    }
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
