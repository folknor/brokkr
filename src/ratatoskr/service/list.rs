//! Service script discovery output.

use std::path::Path;

use crate::error::DevError;
use crate::output;
use crate::ratatoskr::discover::{self, SCRIPT_DIR, ScriptInfo};

/// Bare `brokkr service` - print every discovered script with its
/// description and expected outcome. Empty-state message points at the
/// expected location so a fresh checkout (no harness module yet) still
/// gets a useful response.
pub fn service_list(project_root: &Path) -> Result<(), DevError> {
    let scripts = discover::discover(project_root)?;
    if scripts.is_empty() {
        output::ratatoskr_msg(&format!(
            "no service-harness scripts found under {SCRIPT_DIR}/"
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
        selection,
        ..
    } in &scripts
    {
        output::ratatoskr_msg(&format!(
            "  {:<40} {:<10} {}",
            name,
            selection.as_str(),
            description.as_deref().unwrap_or("\u{2014}"),
        ));
    }
    Ok(())
}
