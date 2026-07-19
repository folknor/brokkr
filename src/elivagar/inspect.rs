//! Inspect elivagar PMTiles output.
//!
//! Builds the release binary and runs `elivagar inspect <pmtiles-file>`,
//! streaming its output directly (header, tile stats, section layout, and
//! metadata) for the caller to read or pipe.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::output;

pub fn run(pmtiles_path: &Path, build_root: &Path) -> Result<(), DevError> {
    let binary = build::cargo_build(&build::BuildConfig::release(None), build_root)?;
    let binary_str = binary.display().to_string();
    let pmtiles_str = pmtiles_path.display().to_string();

    let captured = output::run_captured(&binary_str, &["inspect", &pmtiles_str], build_root)?;

    let stdout = String::from_utf8_lossy(&captured.stdout);
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    captured.check_success(&binary_str)?;
    Ok(())
}
