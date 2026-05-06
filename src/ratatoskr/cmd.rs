//! Top-level `[ratatoskr]` brokkr commands.

use std::path::Path;

use crate::config::DevConfig;
use crate::error::DevError;
use crate::output;
use crate::ratatoskr::build;
use crate::ratatoskr::discover::{self, ScriptInfo, SCRIPT_DIR};

/// Skeleton implementation of `brokkr service-test <SCRIPT>`.
///
/// Validates the script path, builds the configured `[[check]]` sweep
/// via `[ratatoskr.harness]`, then errors with a "harness pending"
/// message. The build step is the live half of the contract: the same
/// feature flags `brokkr check` enforces are applied here, and the
/// resulting binary path is the one the harness will eventually spawn.
/// The actual spawn (and Lua VM, ServiceClient bindings, wait
/// combinator, artefact-dir population, history-DB recording) lands
/// once the brokkr/ratatoskr architecture decision is settled. See
/// `notes/ratatoskr-service-harness.md`.
pub fn service_test(
    project_root: &Path,
    dev_config: &DevConfig,
    script: &str,
    _keep_artefacts: bool,
    debug: bool,
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

    let harness_cfg = dev_config
        .ratatoskr
        .as_ref()
        .and_then(|r| r.harness.as_ref())
        .ok_or_else(|| {
            DevError::Config(
                "service-test: no [ratatoskr.harness] section in brokkr.toml. \
                 Add a [[check]] entry naming the harness sweep, then \
                 [ratatoskr.harness] sweep = \"<name>\", binary = \"<package>\". \
                 See notes/ratatoskr-service-harness.md."
                    .into(),
            )
        })?;

    let built = build::build_for_harness(project_root, &dev_config.check, harness_cfg, debug)?;
    output::ratatoskr_msg(&format!(
        "harness build ok (sweep={}, binary={})",
        built.sweep_label,
        built.binary.display(),
    ));

    Err(DevError::Config(format!(
        "service-test harness not yet implemented (script {script} validated, \
         binary built at {}). See notes/ratatoskr-service-harness.md for the plan.",
        built.binary.display()
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
