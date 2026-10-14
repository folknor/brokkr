//! Process-sensitive services supplied to the test runner by its caller.

use std::sync::Arc;

/// A cooperative stop source. Production callers bridge this to brokkr's
/// shutdown protocol; runner tests can supply an isolated signal.
#[derive(Clone)]
pub(crate) struct StopSignal(Arc<dyn Fn() -> bool + Send + Sync>);

impl StopSignal {
    pub(crate) fn new(is_requested: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(is_requested))
    }

    pub(crate) fn process() -> Self {
        Self::new(crate::shutdown::is_shutdown_requested)
    }

    pub(crate) fn is_requested(&self) -> bool {
        (self.0)()
    }
}

/// The process-sensitive services used by the runner and its background
/// drains: a stop source and the warning sink.
#[derive(Clone)]
pub(crate) struct RunContext {
    pub(crate) stop: StopSignal,
    warn: Arc<dyn Fn(&str) + Send + Sync>,
}

impl RunContext {
    pub(crate) fn new(stop: StopSignal, warn: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self {
            stop,
            warn: Arc::new(warn),
        }
    }

    pub(crate) fn process() -> Self {
        Self::new(StopSignal::process(), crate::output::warn)
    }

    pub(crate) fn warn(&self, message: &str) {
        (self.warn)(message);
    }
}
