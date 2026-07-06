//! Diagnose one PMTiles tile's ring winding/area.
//!
//! Builds the release binary and runs `elivagar diag <pmtiles-file> -z Z -x X
//! -y Y`, streaming its output directly for the caller to read or pipe.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::output;

pub fn run(pmtiles_path: &Path, project_root: &Path, z: u8, x: u32, y: u32) -> Result<(), DevError> {
    let binary = build::cargo_build(&build::BuildConfig::release(None), project_root)?;
    let binary_str = binary.display().to_string();
    let pmtiles_str = pmtiles_path.display().to_string();
    let z_str = z.to_string();
    let x_str = x.to_string();
    let y_str = y.to_string();

    let args = ["diag", &pmtiles_str, "-z", &z_str, "-x", &x_str, "-y", &y_str];
    let captured = output::run_captured(&binary_str, &args, project_root)?;

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
