//! Ratatoskr-specific brokkr commands.
//!
//! The eventual harness lives partly here and partly in ratatoskr's own
//! source tree (the side that holds `ServiceClient`). This module owns
//! brokkr's orchestration responsibilities: project gating, direct harness
//! builds from `[ratatoskr.harness]`, lockfile, artefact-dir
//! lifecycle, history recording. The Lua VM and the `ServiceClient`
//! Lua bindings live in ratatoskr (in the `app` crate, behind the
//! `test-helpers` feature). The cross-cutting design lives at
//! `<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`.

use crate::config::HarnessConfig;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::error::DevError;

pub(crate) fn require_harness_config<'a>(
    cfg: &'a RatatoskrConfig,
    command: &str,
) -> Result<&'a HarnessConfig, DevError> {
    cfg.harness.as_ref().ok_or_else(|| {
        DevError::Config(format!(
            "{command}: no [ratatoskr.harness] section in brokkr.toml. \
             Declare it with `package = \"<crate>\"` (and optional \
             `binary`, `features`, `debug`)."
        ))
    })
}

pub mod build;
pub mod cmd;
pub mod discover;
pub mod gate;
pub(crate) mod gate_store;
pub mod saehrimnir;
pub mod service;
pub mod sync;
pub mod timing;
