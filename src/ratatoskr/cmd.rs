//! Top-level `[ratatoskr]` brokkr commands.

use std::path::Path;

use crate::error::DevError;
use crate::output;
use crate::ratatoskr::discover::{self, ScriptInfo, SCRIPT_DIR};

/// Skeleton implementation of `brokkr service-test <SCRIPT>`.
///
/// Validates the script path, then errors with a "harness pending"
/// message. The full implementation (build the ratatoskr binary against
/// a `[[check]]` sweep, spawn `app --test-harness <script>` with an
/// artefact-dir env var, capture exit, preserve the artefact dir on
/// failure, record the run in brokkr's history DB) lands once the
/// brokkr/ratatoskr split is wired through. See
/// `notes/ratatoskr-service-harness.md`.
pub fn service_test(
    _project_root: &Path,
    script: &str,
    _keep_artefacts: bool,
) -> Result<(), DevError> {
    let script_path = Path::new(script);
    if !script_path.exists() {
        return Err(DevError::Config(format!(
            "service-test: script not found: {script}"
        )));
    }
    if !script_path.is_file() {
        return Err(DevError::Config(format!(
            "service-test: script path is not a regular file: {script}"
        )));
    }
    Err(DevError::Config(format!(
        "service-test harness not yet implemented (script {script} validated). \
         See notes/ratatoskr-service-harness.md for the plan."
    )))
}

/// `brokkr service-list` - print every discovered script with its
/// description and expected outcome. Empty-state message points at the
/// expected location so a fresh checkout (no harness module yet) still
/// gets a useful response.
pub fn service_list(project_root: &Path) -> Result<(), DevError> {
    let scripts = discover::discover(project_root)?;
    if scripts.is_empty() {
        output::ratatoskr_msg(&format!(
            "no service-test scripts found under {SCRIPT_DIR}/"
        ));
        output::ratatoskr_msg(
            "  (the harness module has not landed in ratatoskr yet, or no scripts have been added)",
        );
        return Ok(());
    }

    output::ratatoskr_msg(&format!(
        "  {:<40} {:<10} {}",
        "Name", "Expected", "Description",
    ));
    output::ratatoskr_msg(&format!("  {}", "\u{2500}".repeat(78)));
    for ScriptInfo {
        name,
        description,
        expected,
        ..
    } in &scripts
    {
        output::ratatoskr_msg(&format!(
            "  {:<40} {:<10} {}",
            name,
            expected.as_str(),
            description.as_deref().unwrap_or("\u{2014}"),
        ));
    }
    Ok(())
}
