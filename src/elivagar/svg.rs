//! Render one PMTiles tile to SVG.
//!
//! Builds the release binary and runs `elivagar svg <pmtiles-file> -z Z -x X
//! -y Y [-W width] [-H height] [-l layers] [-o output]`, streaming its
//! output directly for the caller to read or pipe.

use std::path::Path;

use crate::build;
use crate::error::DevError;
use crate::output;

#[allow(clippy::too_many_arguments)]
pub fn run(
    pmtiles_path: &Path,
    project_root: &Path,
    z: u8,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    layers: Option<&str>,
    output_path: Option<&Path>,
) -> Result<(), DevError> {
    let binary = build::cargo_build(&build::BuildConfig::release(None), project_root)?;
    let binary_str = binary.display().to_string();
    let pmtiles_str = pmtiles_path.display().to_string();
    let z_str = z.to_string();
    let x_str = x.to_string();
    let y_str = y.to_string();
    let width_str = width.to_string();
    let height_str = height.to_string();
    let output_str = output_path.map(|p| p.display().to_string());

    let mut args: Vec<&str> = vec![
        "svg", &pmtiles_str, "-z", &z_str, "-x", &x_str, "-y", &y_str, "-W", &width_str, "-H",
        &height_str,
    ];
    if let Some(l) = layers {
        args.push("-l");
        args.push(l);
    }
    if let Some(o) = output_str.as_deref() {
        args.push("-o");
        args.push(o);
    }

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
