//! The wire contract a results server and its clients share: one request
//! per HTTP exchange, its answer, and the failure envelope. No HTTP here (a
//! status code is the server's choice and only advisory); the server is
//! transport around [`serve`], and a client decodes the same types.
//!
//! Requests and answers are strict in both directions, and compatibility is
//! by one protocol version, upgraded in step: any change to how a request,
//! an answer or a type inside them encodes or decodes is a new
//! [`PROTOCOL_VERSION`]. The failure envelope is the one tolerant shape, and
//! its meaning is stable across versions, since a client meeting a server
//! of another version must still understand the refusal.

use serde::{Deserialize, Serialize};

use super::document::RunDocument;
use super::list::{RunFilter, StoredRun};
use super::{Published, Store, StoreError};

/// The protocol both sides speak. A request naming another is refused.
pub const PROTOCOL_VERSION: u32 = 3;

/// The header carrying [`PROTOCOL_VERSION`], on every request and on every
/// answer the server itself writes.
pub const PROTOCOL_HEADER: &str = "brokkr-bench-protocol";

/// The one route: `POST` a [`Request`], or `GET` the server's identity.
pub const PATH: &str = "/bench";

/// The largest request body the server reads. A larger document is
/// publishable to a local store but refused remotely as `too_large`.
pub const MAX_REQUEST_BYTES: u64 = 256 << 20;

/// How long the server keeps one connection, head to the last byte of its
/// answer. A client attempt waits longer than this, so a server that is not
/// stuck always answers or closes before the client gives up on it.
pub const CONNECTION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// One request. Externally tagged, so a large document is decoded in place
/// rather than buffered to find its tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Request {
    /// Publish one run; its uuid is the request id, and a retry is answered
    /// from the stored run.
    Publish { document: Box<RunDocument> },
    /// The uuids a filter selects, in the store's total order.
    Select { filter: RunFilter },
    /// One run with what the store derived, children on request.
    Run { uuid: String, children: bool },
    /// Whether a project has any run.
    HasRuns { project: String },
    /// Runs of other projects whose uuid starts with `prefix`.
    Elsewhere { project: String, prefix: String },
}

/// A successful answer, one variant per request kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Answer {
    Published(Published),
    Runs(Vec<String>),
    /// `children_loaded` says whether the iterations' children were read; a
    /// run without them is a summary, never a document to republish.
    Run(Option<Box<StoredRun>>),
    HasRuns(bool),
    /// `(uuid, project)`, newest first.
    Elsewhere(Vec<(String, String)>),
}

/// Run one decoded request against the store.
pub fn execute(store: &mut Store, request: Request) -> Result<Answer, StoreError> {
    Ok(match request {
        Request::Publish { document } => Answer::Published(store.publish(&document)?),
        Request::Select { filter } => Answer::Runs(store.select(&filter)?),
        Request::Run { uuid, children } => Answer::Run(store.run(&uuid, children)?.map(Box::new)),
        Request::HasRuns { project } => Answer::HasRuns(store.has_runs(&project)?),
        Request::Elsewhere { project, prefix } => {
            Answer::Elsewhere(store.elsewhere(&project, &prefix)?)
        }
    })
}

/// Why a request failed, as the server classifies it. Each kind fixes what
/// its envelope says: whether this attempt is proved unapplied, and whether a
/// retry may succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The store refused the request (an invalid document). Final.
    Refused,
    /// The run uuid is stored with different content. Final.
    Conflict,
    /// The store failed before any commit. Retryable.
    Failed,
    /// The store's commit errored; the write may have landed. Retryable, and
    /// a republication settles it.
    CommitUncertain,
    /// The request is not one this server reads: malformed, an unknown kind,
    /// another protocol, no body length. Final.
    BadRequest,
    /// The body is over [`MAX_REQUEST_BYTES`]. Final.
    TooLarge,
    /// The server could not take the request (at capacity, or its store
    /// worker gone), and it never ran. Retryable.
    Busy,
    /// The body did not arrive in time. Retryable.
    ReadTimeout,
    /// The server stopped waiting for the store. Unapplied only when the
    /// request was withdrawn before it started. Retryable.
    WaitExpired { withdrawn: bool },
    /// The server failed while running or answering the request, after it
    /// started. Retryable.
    Internal,
}

impl Kind {
    /// The wire spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
            Self::CommitUncertain => "commit_uncertain",
            Self::BadRequest => "bad_request",
            Self::TooLarge => "too_large",
            Self::Busy => "busy",
            Self::ReadTimeout => "read_timeout",
            Self::WaitExpired { .. } => "wait_expired",
            Self::Internal => "internal",
        }
    }

    /// Whether this attempt is proved not to have applied.
    pub fn unapplied(self) -> bool {
        match self {
            Self::CommitUncertain | Self::Internal => false,
            Self::WaitExpired { withdrawn } => withdrawn,
            Self::Refused
            | Self::Conflict
            | Self::Failed
            | Self::BadRequest
            | Self::TooLarge
            | Self::Busy
            | Self::ReadTimeout => true,
        }
    }

    /// Whether repeating the same request may succeed.
    pub fn retry(self) -> bool {
        !matches!(
            self,
            Self::Refused | Self::Conflict | Self::BadRequest | Self::TooLarge
        )
    }

    /// The kind a store error is.
    pub fn of(error: &StoreError) -> Self {
        match error {
            StoreError::Refused(_) => Self::Refused,
            StoreError::Conflict { .. } => Self::Conflict,
            StoreError::Failed(_) => Self::Failed,
            StoreError::CommitUncertain(_) => Self::CommitUncertain,
        }
    }
}

/// The failure envelope, `{"error": {...}}`. `applied` is present, and
/// `false`, only when this attempt is proved not to have applied; it speaks
/// for this attempt alone, never for earlier ones. Extra fields and kinds a
/// client does not know are tolerated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<bool>,
    pub retry: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Envelope {
    error: Failure,
}

impl Failure {
    /// The envelope for `kind`.
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind: kind.name().to_owned(),
            message: message.into(),
            applied: kind.unapplied().then_some(false),
            retry: kind.retry(),
        }
    }

    /// The envelope as the body of an answer.
    pub fn encode(&self) -> Vec<u8> {
        // Strings and booleans only: encoding cannot fail, and an empty body
        // would still classify as no envelope at all.
        serde_json::to_vec(&Envelope {
            error: self.clone(),
        })
        .unwrap_or_default()
    }

    /// The envelope in an answer's body, if it holds a well-formed one. A
    /// body without one (a proxy's page, a truncated answer) is `None`, and
    /// the answer is then classified by its status alone.
    pub fn decode(body: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Envelope>(body)
            .ok()
            .map(|envelope| envelope.error)
    }

    /// What a client may conclude from this envelope. `applied: true` is no
    /// value the protocol uses and proves nothing, so it is read as
    /// uncertain.
    pub fn classify(&self, published_uuid: Option<&str>) -> Classification {
        let consistent = self.consistent();
        let unapplied = self.applied == Some(false) && consistent;
        Classification {
            unapplied,
            retry: self.retry,
            store: consistent
                .then(|| self.store_error(unapplied, published_uuid))
                .flatten(),
        }
    }

    /// Whether a kind this code knows carries the `applied` and `retry` that
    /// kind always has; an unknown kind is read by those fields alone. An
    /// envelope that contradicts its own kind proves nothing.
    fn consistent(&self) -> bool {
        let kinds = [
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
        ];
        let named: Vec<Kind> = kinds
            .into_iter()
            .filter(|kind| kind.name() == self.kind)
            .collect();
        named.is_empty()
            || named.iter().any(|kind| {
                kind.unapplied() == (self.applied == Some(false))
                    && self.applied != Some(true)
                    && kind.retry() == self.retry
            })
    }

    /// The store error this envelope reports, only when its kind is a store
    /// kind and its `applied` and `retry` are exactly that kind's: a
    /// contradiction never reaches the caller as a store error whose class
    /// claims more than the envelope proves.
    fn store_error(&self, unapplied: bool, published_uuid: Option<&str>) -> Option<StoreError> {
        let error = match self.kind.as_str() {
            "refused" => StoreError::Refused(self.message.clone()),
            "conflict" => StoreError::Conflict {
                uuid: published_uuid?.to_owned(),
            },
            "failed" => StoreError::Failed(self.message.clone()),
            "commit_uncertain" => StoreError::CommitUncertain(self.message.clone()),
            _ => return None,
        };
        let kind = Kind::of(&error);
        let exact = kind.unapplied() == unapplied
            && self.applied != Some(true)
            && kind.retry() == self.retry;
        exact.then_some(error)
    }
}

/// A client's reading of a failure envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// This attempt is proved not to have applied. False is uncertain, not
    /// applied.
    pub unapplied: bool,
    /// Repeating the request may succeed.
    pub retry: bool,
    /// The store error, when the envelope is exactly one.
    pub store: Option<StoreError>,
}

/// The server's reply to one request body: the failure kind, if any, and
/// the bytes to send. Success bytes exist before a success is reported, so
/// an answer that cannot be encoded is a failure, never a success without a
/// body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub failure: Option<Kind>,
    pub body: Vec<u8>,
}

impl Reply {
    /// A failure reply.
    pub fn failure(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            failure: Some(kind),
            body: Failure::new(kind, message).encode(),
        }
    }
}

/// Decode a request body, run it and encode the answer: everything a server
/// does with a body once it holds the store.
pub fn serve(store: &mut Store, body: &[u8]) -> Reply {
    let request: Request = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => {
            return Reply::failure(
                Kind::BadRequest,
                format!("the request does not decode as protocol {PROTOCOL_VERSION}: {error}"),
            );
        }
    };
    match execute(store, request) {
        Ok(answer) => match serde_json::to_vec(&answer) {
            Ok(body) => Reply {
                failure: None,
                body,
            },
            Err(error) => Reply::failure(
                Kind::Internal,
                format!("the answer could not be encoded: {error}"),
            ),
        },
        Err(error) => Reply::failure(Kind::of(&error), error.to_string()),
    }
}

/// Each shipped protocol's fingerprint, indexed by version - 1: the xxh3-128
/// of the canonical encoding of the frozen wire fixtures. A shipped entry
/// never changes; a change to the encoding is a new entry and a new
/// [`PROTOCOL_VERSION`].
#[cfg(test)]
pub(crate) const SHIPPED_PROTOCOLS: &[&str] = &[
    "79350d76b49bcb1a44e2d467a7cc838c",
    "84fb92d68285c0f19cfad661fa755ac3",
    "4ab725018fac45420ed6d582e73d0a89",
];
