//! Download Natural Earth shapefiles for low-zoom layers (z0-5).
//!
//! Downloads pinned Natural Earth archives into `data/natural_earth/`.
//! Elivagar uses these for pre-generalized water, glacier, and boundary features
//! at z0-5, with OSM taking over at z6.
//!
//! Idempotent: skips layers only when all required shapefile components exist.

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

const REQUIRED_COMPONENTS: &[&str] = &["shp", "shx", "dbf", "prj"];

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
        if layer_is_complete(&ne_dir, layer) {
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
        let staging = LayerStaging::create(&ne_dir, layer.name)?;
        let zip_str = zip_path.display().to_string();
        let staging_str = staging.path().display().to_string();

        let captured = output::run_captured(
            "unzip",
            &["-o", "-j", &zip_str, "-d", &staging_str],
            &ne_dir,
        )?;

        captured.check_success("unzip")?;

        for extension in REQUIRED_COMPONENTS {
            let staged_component = staging.path().join(format!("{}.{}", layer.name, extension));
            if !staged_component.is_file() {
                return Err(DevError::Verify(format!(
                    "expected {} after extraction but not found",
                    staged_component.display()
                )));
            }
        }

        promote_staged(&staging, &ne_dir, layer.name)?;

        if !layer_is_complete(&ne_dir, layer) {
            return Err(DevError::Verify(format!(
                "Natural Earth layer {} is missing one or more required components after extraction",
                layer.name
            )));
        }

        match std::fs::remove_file(&zip_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
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

fn promote_staged(
    staging: &LayerStaging,
    ne_dir: &Path,
    layer_name: &str,
) -> Result<(), DevError> {
    {
        let required_paths: Vec<_> = REQUIRED_COMPONENTS
            .iter()
            .map(|extension| staging.path().join(format!("{layer_name}.{extension}")))
            .collect();
        let mut extracted_files = Vec::new();
        for entry in std::fs::read_dir(staging.path())? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                extracted_files.push(entry.path());
            }
        }
        extracted_files.sort();

        // Clear any old partial set before installing the staged files. The
        // required components are moved last, so the final rename is the
        // first point at which the layer can pass the completeness check.
        for extension in REQUIRED_COMPONENTS {
            let destination = ne_dir.join(format!("{layer_name}.{extension}"));
            match std::fs::remove_file(&destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        // Keep any additional archive files (such as .cpg) and replace them
        // before the required set, so they cannot make a partial layer appear
        // complete during promotion.
        for extracted in extracted_files
            .iter()
            .filter(|path| !required_paths.contains(*path))
        {
            let file_name = extracted.file_name().ok_or_else(|| {
                DevError::Verify(format!(
                    "extracted path has no file name: {}",
                    extracted.display()
                ))
            })?;
            std::fs::rename(extracted, ne_dir.join(file_name))?;
        }
        for extracted in &required_paths {
            let file_name = extracted.file_name().ok_or_else(|| {
                DevError::Verify(format!(
                    "extracted path has no file name: {}",
                    extracted.display()
                ))
            })?;
            std::fs::rename(extracted, ne_dir.join(file_name))?;
        }

    }
    Ok(())
}

fn layer_is_complete(ne_dir: &Path, layer: &NaturalEarthLayer) -> bool {
    REQUIRED_COMPONENTS.iter().all(|extension| {
        ne_dir
            .join(format!("{}.{}", layer.name, extension))
            .is_file()
    })
}

struct LayerStaging {
    path: std::path::PathBuf,
}

impl LayerStaging {
    fn create(ne_dir: &Path, layer_name: &str) -> Result<Self, DevError> {
        let path = ne_dir.join(format!(".{layer_name}.extracting"));
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&path)?,
            Ok(_) => std::fs::remove_file(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::create_dir(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LayerStaging {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.path));
    }
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
