//! Download Natural Earth shapefiles for low-zoom layers (z0-5).
//!
//! Downloads individual shapefiles from naciscdn.org into `data/natural_earth/`.
//! Elivagar uses these for pre-generalized water, glacier, and boundary features
//! at z0-5, with OSM taking over at z6.
//!
//! Idempotent: skips layers whose .shp already exists.

use std::path::Path;

use crate::error::DevError;
use crate::output;
use crate::tools;

// ---------------------------------------------------------------------------
// Layer registry
// ---------------------------------------------------------------------------

struct NaturalEarthLayer {
    /// Layer name, e.g. `ne_110m_ocean`.
    name: &'static str,
    /// URL category: `physical` or `cultural`.
    category: &'static str,
    /// Scale: `110m`, `50m`, or `10m`.
    scale: &'static str,
}

const LAYERS: &[NaturalEarthLayer] = &[
    // Ocean
    NaturalEarthLayer { name: "ne_110m_ocean", category: "physical", scale: "110m" },
    NaturalEarthLayer { name: "ne_50m_ocean", category: "physical", scale: "50m" },
    NaturalEarthLayer { name: "ne_10m_ocean", category: "physical", scale: "10m" },
    // Lakes
    NaturalEarthLayer { name: "ne_110m_lakes", category: "physical", scale: "110m" },
    NaturalEarthLayer { name: "ne_50m_lakes", category: "physical", scale: "50m" },
    NaturalEarthLayer { name: "ne_10m_lakes", category: "physical", scale: "10m" },
    // Glaciated areas
    NaturalEarthLayer { name: "ne_110m_glaciated_areas", category: "physical", scale: "110m" },
    NaturalEarthLayer { name: "ne_50m_glaciated_areas", category: "physical", scale: "50m" },
    NaturalEarthLayer { name: "ne_10m_glaciated_areas", category: "physical", scale: "10m" },
    // Country boundaries
    NaturalEarthLayer { name: "ne_110m_admin_0_boundary_lines_land", category: "cultural", scale: "110m" },
    NaturalEarthLayer { name: "ne_50m_admin_0_boundary_lines_land", category: "cultural", scale: "50m" },
    NaturalEarthLayer { name: "ne_10m_admin_0_boundary_lines_land", category: "cultural", scale: "10m" },
];

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(data_dir: &Path) -> Result<(), DevError> {
    tools::check_curl()?;
    check_unzip()?;

    let ne_dir = data_dir.join("natural_earth");
    std::fs::create_dir_all(&ne_dir)?;

    let mut downloaded = 0usize;
    let mut skipped = 0usize;

    for layer in LAYERS {
        let shp_path = ne_dir.join(format!("{}.shp", layer.name));

        if shp_path.exists() {
            skipped += 1;
            continue;
        }

        let url = format!(
            "https://naciscdn.org/naturalearth/{}/{}/{}.zip",
            layer.scale, layer.category, layer.name,
        );
        let zip_name = format!("{}.zip", layer.name);
        let zip_path = ne_dir.join(&zip_name);

        output::download_msg(&format!("downloading {}", layer.name));
        tools::download_file(&url, &zip_path)?;

        output::download_msg(&format!("extracting {}", layer.name));
        let ne_str = ne_dir.display().to_string();
        let zip_str = zip_path.display().to_string();

        let captured = output::run_captured(
            "unzip",
            &["-o", "-j", &zip_str, "-d", &ne_str],
            &ne_dir,
        )?;

        captured.check_success("unzip")?;

        std::fs::remove_file(&zip_path).ok();

        if !shp_path.exists() {
            return Err(DevError::Verify(format!(
                "expected {} after extraction but not found",
                shp_path.display()
            )));
        }

        downloaded += 1;
    }

    if skipped == LAYERS.len() {
        output::download_msg(&format!(
            "all {} Natural Earth layers already present in {}",
            LAYERS.len(),
            ne_dir.display()
        ));
    } else {
        output::download_msg(&format!(
            "done: {downloaded} downloaded, {skipped} already present"
        ));
    }

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
            "'unzip' not found in PATH (required for Natural Earth shapefile extraction)".into(),
        ])),
    }
}
