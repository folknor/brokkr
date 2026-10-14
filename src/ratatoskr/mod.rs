//! Ratatoskr-specific brokkr commands.
//!
//! The eventual harness lives partly here and partly in ratatoskr's own
//! source tree (the side that holds `ServiceClient`). This module owns
//! brokkr's orchestration responsibilities: project gating, direct harness
//! builds from `[ratatoskr.harness]`, lockfile, artefact-dir
//! lifecycle, and measurement-history recording for sync benches. The Lua VM
//! and the `ServiceClient` Lua bindings live in ratatoskr (in the `app`
//! crate, behind the `test-helpers` feature). Cross-cutting design lives at
//! `<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`.

use crate::config::HarnessConfig;
use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::error::DevError;
use std::path::{Path, PathBuf};

/// Argument that switches the configured harness binary into script mode.
pub(crate) const HARNESS_MODE_FLAG: &str = "--test-harness";

/// Resolve a path from ratatoskr configuration against the project root.
/// Absolute values remain absolute; every relative path follows one rule.
pub(crate) fn resolve_config_path(project_root: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        project_root.join(configured)
    }
}

/// Common environment passed to every ratatoskr harness invocation.
pub(crate) struct HarnessEnv {
    artefact_dir: String,
    bin_dir: String,
}

impl HarnessEnv {
    pub(crate) fn as_pairs(&self) -> [(&'static str, &str); 2] {
        [
            ("BROKKR_HARNESS_ARTEFACT_DIR", &self.artefact_dir),
            ("BROKKR_TEST_BIN_DIR", &self.bin_dir),
        ]
    }
}

/// Construct the shared environment for a ratatoskr harness process.
pub(crate) fn harness_env(artefact_dir: &Path, bin_dir: &Path) -> HarnessEnv {
    HarnessEnv {
        artefact_dir: artefact_dir.display().to_string(),
        bin_dir: bin_dir.display().to_string(),
    }
}

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
