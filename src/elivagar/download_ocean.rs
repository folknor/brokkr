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

struct OceanVariant {
    url: &'static str,
    zip_name: &'static str,
    dir_name: &'static str,
    shp_name: &'static str,
    label: &'static str,
    size_hint: &'static str,
}

const FULL_RES: OceanVariant = OceanVariant {
    url: "https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip",
    zip_name: "water-polygons-split-3857.zip",
    dir_name: "water-polygons-split-3857",
    shp_name: "water_polygons.shp",
    label: "full-resolution ocean polygons",
    size_hint: "~765 MB",
};

const SIMPLIFIED: OceanVariant = OceanVariant {
    url: "https://osmdata.openstreetmap.de/download/simplified-water-polygons-split-3857.zip",
    zip_name: "simplified-water-polygons-split-3857.zip",
    dir_name: "simplified-water-polygons-split-3857",
    shp_name: "simplified_water_polygons.shp",
    label: "simplified ocean polygons",
    size_hint: "~13 MB",
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(data_dir: &Path) -> Result<(), DevError> {
    tools::check_curl()?;
    check_unzip()?;
    std::fs::create_dir_all(data_dir)?;

    download_variant(data_dir, &FULL_RES)?;
    download_variant(data_dir, &SIMPLIFIED)?;

    Ok(())
}

fn download_variant(data_dir: &Path, variant: &OceanVariant) -> Result<(), DevError> {
    let shp_path = data_dir.join(variant.dir_name).join(variant.shp_name);

    if shp_path.exists() {
        output::download_msg(&format!(
            "{} already exists: {}",
            variant.label,
            shp_path.display()
        ));
        return Ok(());
    }

    let zip_path = data_dir.join(variant.zip_name);

    output::download_msg(&format!("downloading {} ({})", variant.label, variant.size_hint));
    tools::download_file(variant.url, &zip_path)?;

    output::download_msg("extracting...");
    let zip_str = zip_path.display().to_string();
    let data_str = data_dir.display().to_string();

    let captured = output::run_captured(
        "unzip",
        &["-o", &zip_str, "-d", &data_str],
        data_dir,
    )?;

    captured.check_success("unzip")?;

    std::fs::remove_file(&zip_path).ok();

    output::download_msg(&format!("done: {}", shp_path.display()));

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
