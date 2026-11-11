//! A script's `validate(params, keys)`: run for every arm as soon as the
//! params bind, before anything else the invocation does, on every run and
//! under `--dry-run`, so a value the bench cannot run is refused before
//! anything is built and nothing is stored.
//!
//! It is pure: the Lua seam refuses every launch, handle and `since()` in the
//! validate phase, and this host refuses them again, so a gap in either is
//! not a way out. What it has is the params as path strings, `config()`,
//! `json`, `fail()` and `print`.

use std::sync::Arc;
use std::time::Instant;

use super::host::{stop_pending, stopping};
use crate::bench::lua::{self, Abort, CallOptions, CallOutcome, HandleRef, Word};

/// The builtins' side for one arm's `validate()`.
pub(super) struct ValidateHost {
    pub(super) config: Arc<lua::Value>,
}

fn refused(name: &str) -> Abort {
    Abort::Failed(format!(
        "{name}() is not available inside validate(), which only judges the params"
    ))
}

impl lua::Host for ValidateHost {
    fn stop(&mut self) -> Result<(), Abort> {
        stopping()
    }

    fn stop_requested() -> bool {
        stop_pending()
    }

    fn begin_block(&mut self, _at: Instant) {}

    fn binary(&mut self, _: Vec<Word>, _: CallOptions) -> Result<CallOutcome, Abort> {
        Err(refused("binary"))
    }

    fn since(&mut self, _: &str) -> Result<bool, Abort> {
        Err(refused("since"))
    }

    fn output(&mut self, _: &str) -> Result<HandleRef, Abort> {
        Err(refused("output"))
    }

    fn scratch(&mut self, _: &str) -> Result<HandleRef, Abort> {
        Err(refused("scratch"))
    }

    fn config(&mut self) -> Result<Arc<lua::Value>, Abort> {
        stopping()?;
        Ok(Arc::clone(&self.config))
    }

    fn read(&mut self, _: usize) -> Result<Vec<u8>, Abort> {
        Err(refused("read"))
    }

    fn write(&mut self, _: usize, _: &[u8]) -> Result<(), Abort> {
        Err(refused("write"))
    }

    fn filesize(&mut self, _: usize) -> Result<u64, Abort> {
        Err(refused("filesize"))
    }

    fn page_cache(&mut self, _: &str, _: lua::CacheMode) -> Result<Option<String>, Abort> {
        Err(refused("page_cache"))
    }
}
