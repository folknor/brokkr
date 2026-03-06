//! Verify elivagar PMTiles output.
//!
//! Builds the release binary and runs `elivagar verify <pmtiles-file>`.
//! The verify command checks tile integrity, metadata, and geometry anomalies.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::output;

pub fn run(
    pmtiles_path: &Path,
    project_root: &Path,
) -> Result<(), DevError> {
    let binary = build::cargo_build(&build::BuildConfig::release(None), project_root)?;
    let binary_str = binary.display().to_string();
    let pmtiles_str = pmtiles_path.display().to_string();

    output::verify_msg(&format!("elivagar verify: {}", pmtiles_path.display()));

    let captured = output::run_captured(&binary_str, &["verify", &pmtiles_str], project_root)?;

    if captured.status.success() {
        let stdout = String::from_utf8_lossy(&captured.stdout);
        for line in stdout.lines() {
            output::verify_msg(&format!("  {line}"));
        }
        output::verify_msg("PASS");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        let stdout = String::from_utf8_lossy(&captured.stdout);
        for line in stdout.lines() {
            output::verify_msg(&format!("  {line}"));
        }
        for line in stderr.lines() {
            output::error(&format!("  {line}"));
        }
        Err(DevError::Verify(format!(
            "elivagar verify failed for {}",
            pmtiles_path.display()
        )))
    }
}
