//! Typed observations emitted by libtest streams.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// THE OBSERVATION CONTRACT. The runner reports what it saw, typed, as it sees
// it - one `Observation` per libtest record, stream end and kill - to a sink
// the caller supplies. It never interprets them: the caller resolves a stream
// against whatever plan it holds (`check_cmd::accounting`), so nothing here
// knows a sweep, a policy or an expected test. Delivered live rather than
// returned, because the interesting runs are the ones that never return
// normally - an interrupt, a watchdog kill - and a report assembled at the end
// is exactly what those paths used to drop.

/// Where an observed stream came from, as far as the runner knows.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum StreamSource {
    /// The launched process's own stdout: a directly executed test binary's,
    /// or cargo's when no harness is isolated.
    Process,
    /// A harness the shim accepted, named by the executable cargo asked the
    /// runner to run - the shim's handshake carries it before the exec.
    Harness { executable: String },
    /// The rustdoc entry point the shim accepted.
    Rustdoc,
}

/// How a stream stopped. Three different facts: the writer closed it, a read
/// failed, or brokkr stopped reading (the drain grace ran out with something
/// still holding the pipe) - in which case records may have been lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StreamEnd {
    Eof,
    ReadError,
    Cancelled,
}

/// A test's terminal record. libtest states the first three; the engine lane
/// also reports a timed-out test and one its own cancellation killed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TestResult {
    Ok,
    Failed,
    Ignored,
    TimedOut,
    Interrupted,
}

/// Why the runner killed (or stopped waiting for) a process. Structured, so a
/// consumer never has to read a cause back out of a signal status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KillCause {
    /// A watched test crossed the per-test cap; `test` names it.
    PerTest,
    /// Execution under way and nothing completed within the cap.
    NoCompletion,
    /// Nothing in flight for the idle window.
    Idle,
    /// The run's own wall backstop.
    Wall,
    /// A sibling run's failure cancelled this one (the parallel lane's abort).
    Cancelled,
    /// A cooperative shutdown: an interrupt or the phase watchdog.
    Stopped,
}

/// One thing the runner saw on one stream.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum ObsEvent {
    SuiteStarted {
        test_count: u64,
    },
    SuiteFinished {
        passed: u64,
        failed: u64,
        ignored: u64,
    },
    Started {
        name: String,
    },
    Finished {
        name: String,
        result: TestResult,
    },
    /// libtest's `test/timeout`: a test past libtest's own 60s warning. Not a
    /// terminal record - the test is still running.
    SlowWarning {
        name: String,
    },
    StreamEnded {
        end: StreamEnd,
    },
    /// A directly launched process's exit status.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Killed {
        cause: KillCause,
        test: Option<String>,
    },
    /// The shim could not authenticate or attribute a harness.
    AttributionError {
        detail: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Observation {
    /// Unique within this brokkr process; one per stream the runner reads.
    pub(crate) stream: u64,
    pub(crate) source: StreamSource,
    pub(crate) event: ObsEvent,
}

pub(crate) type ObservationSink = Arc<dyn Fn(Observation) + Send + Sync>;

static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

/// A fresh stream id, for a caller that reports observations of its own (the
/// engine lane, whose events come from the engine rather than a pipe).
pub(crate) fn next_stream_id() -> u64 {
    NEXT_STREAM.fetch_add(1, Ordering::Relaxed)
}

/// One stream's handle on the sink.
#[derive(Clone)]
pub(crate) struct StreamTap {
    sink: ObservationSink,
    stream: u64,
    source: StreamSource,
}

impl StreamTap {
    pub(crate) fn new(sink: &ObservationSink, source: StreamSource) -> Self {
        Self {
            sink: Arc::clone(sink),
            stream: next_stream_id(),
            source,
        }
    }

    pub(crate) fn emit(&self, event: ObsEvent) {
        (self.sink)(Observation {
            stream: self.stream,
            source: self.source.clone(),
            event,
        });
    }
}
