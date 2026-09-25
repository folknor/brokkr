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

/// Marker written into an extracted shapefile directory once the extraction
/// (or reprojection) that produced it has finished. The `.shp` alone is not
/// evidence of a complete set: an interrupted `unzip` can leave it present -
/// even truncated - beside a missing `.dbf`/`.shx`, and every later check would
/// call that "already exists".
///
/// A tree extracted before the marker existed is adopted (marker written) when
/// its whole component set is present, rather than re-fetched: these files feed
/// tilegen's ocean inputs, whose digests the corpus contract and the ocean
/// artifact key pin, and osmdata regenerates them daily - a silent re-download
/// would change every archive's ocean. The adoption cannot see a truncated
/// `.shp`; only extractions this code performed carry that guarantee.
///
/// There is no content hash to verify against: osmdata.openstreetmap.de
/// regenerates these archives daily under fixed URLs, so any pinned digest
/// would be stale within a day. Completeness is what can be checked here.
const COMPLETE_MARKER: &str = ".brokkr-complete";

/// Whether the shapefile at `shp` is complete: marked by a finished
/// extraction, or (legacy, pre-marker trees) carrying every component file, in
/// which case the marker is written now.
fn shapefile_complete(shp: &Path) -> bool {
    let Some(dir) = shp.parent() else {
        return false;
    };
    let marker = dir.join(COMPLETE_MARKER);
    if marker.exists() {
        return shp.exists();
    }
    let legacy_complete = ["shp", "shx", "dbf", "prj"]
        .iter()
        .all(|ext| shp.with_extension(ext).exists());
    legacy_complete && std::fs::write(&marker, b"").is_ok()
}

fn download_variant(data_dir: &Path, variant: &OceanVariant) -> Result<(), DevError> {
    let shp_path = data_dir.join(variant.dir_name).join(variant.shp_name);
    let marker = data_dir.join(variant.dir_name).join(COMPLETE_MARKER);

    if shapefile_complete(&shp_path) {
        output::download_msg(&format!(
            "{} already exists: {}",
            variant.label,
            shp_path.display()
        ));
        return Ok(());
    }

    let zip_path = data_dir.join(variant.zip_name);

    output::download_msg(&format!(
        "downloading {} ({})",
        variant.label, variant.size_hint
    ));
    tools::download_file(variant.url, &zip_path)?;

    output::download_msg("extracting...");
    let zip_str = zip_path.display().to_string();
    let data_str = data_dir.display().to_string();

    let captured = output::run_captured("unzip", &["-o", &zip_str, "-d", &data_str], data_dir)?;

    captured.check_success("unzip")?;
    if !shp_path.exists() {
        return Err(DevError::Config(format!(
            "unzip succeeded but {} is missing - archive layout changed?",
            shp_path.display()
        )));
    }
    std::fs::write(&marker, b"")?;

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
/// simplified 3857 polygons to 4326 via `ogr2ogr`. Idempotent - skips
/// downloads and reprojection if the output shapefiles already exist.
pub fn ensure_ocean_4326(data_dir: &Path) -> Result<(), DevError> {
    tools::check_curl()?;
    check_unzip()?;
    std::fs::create_dir_all(data_dir)?;

    // Full-resolution 4326 - direct download.
    download_variant(data_dir, &FULL_RES_4326)?;

    // Simplified 4326 - reprojected from 3857 source via ogr2ogr.
    let simplified_dir = data_dir.join("simplified-water-polygons-split-4326");
    let simplified = simplified_dir.join("simplified_water_polygons.shp");
    let simplified_marker = simplified_dir.join(COMPLETE_MARKER);

    if shapefile_complete(&simplified) {
        output::download_msg(&format!(
            "simplified ocean polygons (4326) already exists: {}",
            simplified.display()
        ));
    } else {
        // Ensure 3857 simplified source exists.
        download_variant(data_dir, &SIMPLIFIED)?;
        check_ogr2ogr()?;

        std::fs::create_dir_all(&simplified_dir)?;

        let src = data_dir.join(SIMPLIFIED.dir_name).join(SIMPLIFIED.shp_name);
        let dst_str = simplified.display().to_string();
        let src_str = src.display().to_string();

        output::download_msg("reprojecting simplified ocean polygons to EPSG:4326...");

        let captured = output::run_captured(
            "ogr2ogr",
            &[
                // A partial reprojection may have left output behind.
                "-overwrite",
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
        std::fs::write(&simplified_marker, b"")?;

        output::download_msg(&format!("done: {}", simplified.display()));
    }

    Ok(())
}
