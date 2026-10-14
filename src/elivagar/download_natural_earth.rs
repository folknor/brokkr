//! Download Natural Earth shapefiles for low-zoom layers (z0-5).
//!
//! Downloads pinned Natural Earth archives into `data/natural_earth/`.
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
    /// Immutable S3 object version for this archive.
    s3_version_id: &'static str,
    /// XXH128 of the pinned archive bytes.
    xxh128: &'static str,
}

// The Natural Earth CDN paths are mutable. These immutable S3 object versions
// identify the same published ZIP artifacts, and each XXH128 is computed over
// that exact object. Updating Natural Earth inputs requires changing both
// pins explicitly.
const LAYERS: &[NaturalEarthLayer] = &[
    // Ocean
    NaturalEarthLayer {
        name: "ne_110m_ocean",
        category: "physical",
        scale: "110m",
        s3_version_id: "MUObtTI2xWOdF_zuXwU_9GhzB.dVLbAN",
        xxh128: "621fcb903ae7a02ac72d060fa73af1d1",
    },
    NaturalEarthLayer {
        name: "ne_50m_ocean",
        category: "physical",
        scale: "50m",
        s3_version_id: "C73uPtV4GdJNU4k6oxytAblPD2fi6N92",
        xxh128: "d4d8afefc83a09ea223d4a0a8f5157fe",
    },
    NaturalEarthLayer {
        name: "ne_10m_ocean",
        category: "physical",
        scale: "10m",
        s3_version_id: "Fvr3vd_8GtWZ6sQoo.Je3C2rTj5Fotg1",
        xxh128: "a69a1d94db8cb6cc26710ead34585c23",
    },
    // Lakes
    NaturalEarthLayer {
        name: "ne_110m_lakes",
        category: "physical",
        scale: "110m",
        s3_version_id: "iU13Po9dZlJsf3dCtNOxZ0fre23Nf8ip",
        xxh128: "885a3b8c5d42030862ab63828302d8af",
    },
    NaturalEarthLayer {
        name: "ne_50m_lakes",
        category: "physical",
        scale: "50m",
        s3_version_id: "usgwJFifnRrM3VHtX13Dyz5X4fdBik.u",
        xxh128: "145a541d09d4add0c4a82e0cc0dd1533",
    },
    NaturalEarthLayer {
        name: "ne_10m_lakes",
        category: "physical",
        scale: "10m",
        s3_version_id: "I8f12xUKxLVqYQlDbPelAGtLPA7QLH8D",
        xxh128: "329dadaae94e6333080669874ffa8f0b",
    },
    // Glaciated areas
    NaturalEarthLayer {
        name: "ne_110m_glaciated_areas",
        category: "physical",
        scale: "110m",
        s3_version_id: "WyVugfTA0mpKpwIM600XZkK2f3xggU3W",
        xxh128: "02f25c11e15523f721552fc77fb8f19c",
    },
    NaturalEarthLayer {
        name: "ne_50m_glaciated_areas",
        category: "physical",
        scale: "50m",
        s3_version_id: "Y6gFJ0DNvm8.4LAdU2jwUkG8FoYjssrB",
        xxh128: "741c69f7dc465bdd9cdb52c20516859c",
    },
    NaturalEarthLayer {
        name: "ne_10m_glaciated_areas",
        category: "physical",
        scale: "10m",
        s3_version_id: "K5303JYT9Dync5kIWdeIoVw0LIikEgWz",
        xxh128: "2b47229d575a1d042b254ad21f480fea",
    },
    // Country boundaries
    NaturalEarthLayer {
        name: "ne_110m_admin_0_boundary_lines_land",
        category: "cultural",
        scale: "110m",
        s3_version_id: "6BXaI1J83h8twySMFcRVuyhMKloNnUFv",
        xxh128: "49792e504a8a6d40c812767aca5f1d59",
    },
    NaturalEarthLayer {
        name: "ne_50m_admin_0_boundary_lines_land",
        category: "cultural",
        scale: "50m",
        s3_version_id: "INgS9a.w9e940deDqwMgSNuQCvuCIhhu",
        xxh128: "1c16ac540ad660d49f8b5330bd120ccf",
    },
    NaturalEarthLayer {
        name: "ne_10m_admin_0_boundary_lines_land",
        category: "cultural",
        scale: "10m",
        s3_version_id: "bM5CgEPdE0hz.WZC4Zpu478UCD0ISGUH",
        xxh128: "85cc9ef5aae081f28853bbad7c27923a",
    },
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
            "https://naturalearth.s3.amazonaws.com/{}_{}/{}.zip?versionId={}",
            layer.scale, layer.category, layer.name, layer.s3_version_id,
        );
        let zip_name = format!("{}.zip", layer.name);
        let zip_path = ne_dir.join(&zip_name);

        output::download_msg(&format!("downloading {}", layer.name));
        tools::download_file_verified(&url, &zip_path, layer.xxh128)?;

        output::download_msg(&format!("extracting {}", layer.name));
        let ne_str = ne_dir.display().to_string();
        let zip_str = zip_path.display().to_string();

        let captured =
            output::run_captured("unzip", &["-o", "-j", &zip_str, "-d", &ne_str], &ne_dir)?;

        captured.check_success("unzip")?;

        match std::fs::remove_file(&zip_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

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
    match crate::tools::find_executable("unzip") {
        Some(_) => Ok(()),
        None => Err(DevError::Preflight(vec![
            "'unzip' not found in PATH (required for Natural Earth shapefile extraction)".into(),
        ])),
    }
}
