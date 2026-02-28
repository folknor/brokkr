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

const FULL_RES_4326: OceanVariant = OceanVariant {
    url: "https://osmdata.openstreetmap.de/download/water-polygons-split-4326.zip",
    zip_name: "water-polygons-split-4326.zip",
    dir_name: "water-polygons-split-4326",
    shp_name: "water_polygons.shp",
    label: "full-resolution ocean polygons (4326)",
    size_hint: "~700 MB",
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

fn check_ogr2ogr() -> Result<(), DevError> {
    let result = std::process::Command::new("which")
        .arg("ogr2ogr")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    match result {
        Ok(status) if status.success() => Ok(()),
        _ => Err(DevError::Preflight(vec![
            "'ogr2ogr' not found in PATH (required for EPSG:4326 reprojection)".into(),
        ])),
    }
}

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

// ---------------------------------------------------------------------------
// EPSG:4326 ocean shapefiles
// ---------------------------------------------------------------------------

/// Ensure EPSG:4326 ocean shapefiles exist.
///
/// Downloads full-resolution 4326 polygons directly, then reprojects the
/// simplified 3857 polygons to 4326 via `ogr2ogr`. Idempotent — skips
/// downloads and reprojection if the output shapefiles already exist.
pub fn ensure_ocean_4326(data_dir: &Path) -> Result<(), DevError> {
    tools::check_curl()?;
    check_unzip()?;
    std::fs::create_dir_all(data_dir)?;

    // Full-resolution 4326 — direct download.
    download_variant(data_dir, &FULL_RES_4326)?;

    // Simplified 4326 — reprojected from 3857 source via ogr2ogr.
    let simplified_dir = data_dir.join("simplified-water-polygons-split-4326");
    let simplified = simplified_dir.join("simplified_water_polygons.shp");

    if simplified.exists() {
        output::download_msg(&format!(
            "simplified ocean polygons (4326) already exists: {}",
            simplified.display()
        ));
    } else {
        // Ensure 3857 simplified source exists.
        download_variant(data_dir, &SIMPLIFIED)?;
        check_ogr2ogr()?;

        std::fs::create_dir_all(&simplified_dir)?;

        let src = data_dir
            .join(SIMPLIFIED.dir_name)
            .join(SIMPLIFIED.shp_name);
        let dst_str = simplified.display().to_string();
        let src_str = src.display().to_string();

        output::download_msg("reprojecting simplified ocean polygons to EPSG:4326...");

        let captured = output::run_captured(
            "ogr2ogr",
            &[
                "-f",
                "ESRI Shapefile",
                &dst_str,
                &src_str,
                "-t_srs",
                "EPSG:4326",
                "-lco",
                "ENCODING=utf8",
            ],
            data_dir,
        )?;

        captured.check_success("ogr2ogr")?;

        output::download_msg(&format!("done: {}", simplified.display()));
    }

    Ok(())
}
