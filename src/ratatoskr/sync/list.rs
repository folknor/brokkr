//! Sync script discovery and the bare index command.

use std::path::{Path, PathBuf};

use crate::config::project_config::ratatoskr::RatatoskrConfig;
use crate::error::DevError;
use crate::output;
use crate::ratatoskr::discover::{self, ScriptInfo};

/// Default location for sync-test scripts inside ratatoskr's tree.
pub const DEFAULT_SYNC_SCRIPT_DIR: &str = "crates/app/tests/sync-harness";

/// Bare `brokkr sync` - discover sync-test scripts under the configured
/// directory and print a sorted table. Empty-state message names the
/// expected directory so a fresh checkout (no harness scripts yet) gets
/// a useful response.
pub fn run_sync_list(project_root: &Path, ratatoskr: &RatatoskrConfig) -> Result<(), DevError> {
    let dir = sync_script_dir(project_root, Some(ratatoskr));
    let scripts = discover::discover_at(&dir)?;
    let display_dir = dir.display();

    if scripts.is_empty() {
        output::ratatoskr_msg(&format!("no sync-test scripts found under {display_dir}"));
        output::ratatoskr_msg(
            "  (the sync-harness module / cohort has not landed in ratatoskr yet, or no scripts have been added)",
        );
        return Ok(());
    }

    output::ratatoskr_msg(&format!(
        "  {:<32} {:<10} {:<14} {:<10} {}",
        "Name", "Expected", "Fixture", "Protocol", "Description",
    ));
    output::ratatoskr_msg(&format!("  {}", "\u{2500}".repeat(78)));
    for ScriptInfo {
        name,
        description,
        expected,
        fixture,
        protocol,
        ..
    } in &scripts
    {
        output::ratatoskr_msg(&format!(
            "  {:<32} {:<10} {:<14} {:<10} {}",
            name,
            expected.as_str(),
            fixture.as_deref().unwrap_or("\u{2014}"),
            protocol.as_deref().unwrap_or("\u{2014}"),
            description.as_deref().unwrap_or("\u{2014}"),
        ));
    }
    Ok(())
}

/// Resolve the sync-script directory: explicit `[ratatoskr]
/// sync_script_dir` if set (relative paths join against the project
/// root), else the [`DEFAULT_SYNC_SCRIPT_DIR`] convention.
pub(super) fn sync_script_dir(project_root: &Path, cfg: Option<&RatatoskrConfig>) -> PathBuf {
    let configured = cfg.and_then(|c| c.sync_script_dir.as_ref());
    match configured {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => project_root.join(p),
        None => project_root.join(DEFAULT_SYNC_SCRIPT_DIR),
    }
}
