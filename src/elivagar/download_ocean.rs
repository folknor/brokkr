//! Download ocean polygons shapefile (EPSG:3857).
//!
//! Replaces `download_ocean.sh`. Downloads and extracts
//! `water-polygons-split-3857.zip` from osmdata.openstreetmap.de.
//! Idempotent: skips if the shapefile already exists.

use std::path::Path;

use crate::error::DevError;
use crate::output;
use crate::tools;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const OCEAN_URL: &str = "https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip";
const OCEAN_DIR: &str = "water-polygons-split-3857";
const OCEAN_SHP: &str = "water-polygons-split-3857/water_polygons.shp";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(data_dir: &Path) -> Result<(), DevError> {
    let shp_path = data_dir.join(OCEAN_SHP);

    if shp_path.exists() {
        output::download_msg(&format!(
            "ocean shapefile already exists: {}",
            shp_path.display()
        ));
        return Ok(());
    }

    tools::check_curl()?;
    check_unzip()?;

    std::fs::create_dir_all(data_dir)?;

    let zip_path = data_dir.join("water-polygons-split-3857.zip");

    output::download_msg("downloading ocean polygons (~765 MB)");
    tools::download_file(OCEAN_URL, &zip_path)?;

    output::download_msg("extracting...");
    let zip_str = zip_path.display().to_string();
    let data_str = data_dir.display().to_string();

    let captured = output::run_captured(
        "unzip",
        &["-o", &zip_str, "-d", &data_str],
        data_dir,
    )?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Subprocess {
            program: "unzip".into(),
            code: captured.status.code(),
            stderr: stderr.into_owned(),
        });
    }

    // Clean up zip file.
    let _ = std::fs::remove_file(&zip_path);

    let final_path = data_dir.join(OCEAN_DIR).join("water_polygons.shp");
    output::download_msg(&format!("done: {}", final_path.display()));

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn check_unzip() -> Result<(), DevError> {
    let result = std::process::Command::new("which")
        .arg("unzip")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => Ok(()),
        _ => Err(DevError::Preflight(vec![
            "'unzip' not found in PATH (required for ocean shapefile extraction)".into(),
        ])),
    }
}
