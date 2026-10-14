//! Run one unmeasured sync script.

use std::path::Path;

use crate::artefacts;
use crate::config::DevConfig;
use crate::error::DevError;
use crate::lockfile::{self, LockContext};
use crate::output;
use crate::ratatoskr::build;

use super::runner::{SyncRunner, resolve_single, validate_sync_config};

// ---------------------------------------------------------------------------
// sync <SCRIPT> - run one
// ---------------------------------------------------------------------------

/// CLI inputs for `brokkr sync <SCRIPT>`. Pulled out so the orchestration
/// body can be smoke-tested with synthetic paths if needed.
pub struct SyncSmokeRequest<'a> {
    pub project_root: &'a Path,
    pub dev_config: &'a DevConfig,
    pub script: &'a str,
    pub keep_artefacts: bool,
    pub profile_override: Option<bool>,
}

/// Drive the unmeasured `brokkr sync <SCRIPT>` end-to-end:
///
/// 1. Validate config: `[ratatoskr.harness]`, `mock_server_binary`, and
///    `fixtures_dir` are all required. Endpoint env-var names are
///    optional - protocols without a configured spelling just don't
///    get an env var.
/// 2. Parse the script's frontmatter; require a `fixture: <NAME>` and
///    resolve it - both before anything is built, so a bad script fails
///    fast.
/// 3. Acquire the global lockfile.
/// 4. Build the harness binary per `[ratatoskr.harness]`.
/// 5. Hand off to [`SyncRunner::run_resolved`]: allocate
///    `.brokkr/ratatoskr/sync/<test>/run-N/`, spawn sæhrimnir and the
///    harness binary, tear down, finalize the artefact dir.
/// 6. PASS/FAIL on the harness binary's exit code; sæhrimnir's outcome
///    is logged but not gating (a script may legitimately tear it down
///    early in scenarios).
pub fn run_sync_smoke(req: &SyncSmokeRequest<'_>) -> Result<(), DevError> {
    let (cfg, harness_cfg, mock_binary, fixtures_dir) =
        validate_sync_config(req.project_root, req.dev_config)?;
    let resolved = resolve_single(req.script, &fixtures_dir)?;

    let project_root_str = req.project_root.display().to_string();
    let _lock = lockfile::acquire(&LockContext {
        project: "ratatoskr",
        command: "sync",
        project_root: &project_root_str,
    })?;
    // Cooperative SIGTERM for `brokkr kill`. Installed right after the
    // lock so every captured subprocess from here on - cargo build,
    // sæhrimnir spawn (no flag-poll, but Drop will hard-kill on unwind),
    // the harness binary - sees the flag-poll path in
    // `output::run_captured_with_env_and_deadline`. Drops at function
    // end, before `_lock`.
    let _sigterm = crate::shutdown::SigtermGuard::install();

    let debug = req
        .profile_override
        .unwrap_or_else(|| harness_cfg.debug.unwrap_or(false));
    let built = build::build_for_harness(
        req.project_root,
        harness_cfg,
        debug,
        Some(&|pid| _lock.set_child_pid(pid)),
        Some(&|| _lock.clear_child_pid()),
        true, // isolate_pg: SigtermGuard above bridges terminal signals
    )?;

    let runner = SyncRunner {
        project_root: req.project_root,
        cfg,
        built: &built,
        mock_binary: &mock_binary,
        keep_artefacts: req.keep_artefacts,
    };

    output::ratatoskr_msg(&format!(
        "running {} (fixture: {})",
        resolved.test_id, resolved.fixture_name
    ));

    let outcome = runner.run_resolved(&resolved, &_lock);
    match outcome.result {
        Ok(()) => {
            output::ratatoskr_msg(&format!("PASS{}", outcome.summary));
            Ok(())
        }
        Err(e) => {
            output::ratatoskr_msg(&format!("FAIL{}: {e}", outcome.summary));
            if let Some(path) = outcome.preserved {
                output::ratatoskr_msg(&format!("artefacts preserved at {}", path.display()));
                artefacts::emit_clean_hint(output::ratatoskr_msg);
            }
            Err(e)
        }
    }
}
