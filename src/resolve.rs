use std::path::{Path, PathBuf};

use crate::config;
use crate::error::DevError;
use crate::preflight;

/// Resolve the PBF path from --pbf or --dataset.
pub(crate) fn resolve_pbf_path(
    pbf: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let (path, hash, origin) = match pbf {
        Some(p) => (PathBuf::from(p), None, None),
        None => {
            let ds = paths.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let pbf_file = ds.pbf.as_ref().ok_or_else(|| {
                DevError::Config(format!("dataset '{dataset}' has no pbf configured"))
            })?;
            (
                paths.data_dir.join(pbf_file),
                ds.sha256_pbf.as_deref(),
                ds.origin.as_deref(),
            )
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "PBF file not found: {}",
            path.display()
        )));
    }

    if let Some(expected) = hash {
        preflight::verify_file_hash(&path, expected, project_root, origin)?;
    }

    Ok(path)
}

/// Resolve the OSC path from --osc or --dataset.
pub(crate) fn resolve_osc_path(
    osc: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let (path, hash, origin) = match osc {
        Some(p) => (PathBuf::from(p), None, None),
        None => {
            let ds = paths.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let osc_file = ds.osc.as_ref().ok_or_else(|| {
                DevError::Config(format!(
                    "dataset '{dataset}' has no osc file configured"
                ))
            })?;
            (
                paths.data_dir.join(osc_file),
                ds.sha256_osc.as_deref(),
                ds.origin.as_deref(),
            )
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "OSC file not found: {}",
            path.display()
        )));
    }

    if let Some(expected) = hash {
        preflight::verify_file_hash(&path, expected, project_root, origin)?;
    }

    Ok(path)
}

/// Resolve the bbox from --bbox or dataset config.
pub(crate) fn resolve_bbox(
    bbox: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<String, DevError> {
    if let Some(b) = bbox {
        return Ok(b.to_owned());
    }

    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;

    ds.bbox.clone().ok_or_else(|| {
        DevError::Config(format!(
            "dataset '{dataset}' has no bbox configured (use --bbox)"
        ))
    })
}

/// Resolve the non-indexed (raw) PBF path from --pbf-raw or dataset config.
pub(crate) fn resolve_raw_pbf_path(
    pbf_raw: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = match pbf_raw {
        Some(p) => PathBuf::from(p),
        None => {
            let ds = paths.datasets.get(dataset).ok_or_else(|| {
                DevError::Config(format!("unknown dataset: {dataset}"))
            })?;
            let raw_file = ds.pbf_raw.as_ref().ok_or_else(|| {
                DevError::Config(format!(
                    "dataset '{dataset}' has no pbf_raw configured (use --pbf-raw)"
                ))
            })?;
            paths.data_dir.join(raw_file)
        }
    };

    if !path.exists() {
        return Err(DevError::Config(format!(
            "raw PBF file not found: {}",
            path.display()
        )));
    }

    Ok(path)
}

/// Get file size in MB (decimal, consistent with bench scripts).
pub(crate) fn file_size_mb(path: &Path) -> Result<f64, DevError> {
    let meta = std::fs::metadata(path)?;
    Ok(meta.len() as f64 / 1_000_000.0)
}

/// Resolve PBF path and its size in one call.
pub(crate) fn resolve_pbf_with_size(
    pbf: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, f64), DevError> {
    let path = resolve_pbf_path(pbf, dataset, paths, project_root)?;
    let mb = file_size_mb(&path)?;
    Ok((path, mb))
}

/// Resolve the nidhogg dataset data directory (required).
pub(crate) fn resolve_nidhogg_data_dir(
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;
    let dir_name = ds.data_dir.as_ref().ok_or_else(|| {
        DevError::Config(format!("dataset '{dataset}' has no data_dir configured"))
    })?;
    Ok(paths.data_dir.join(dir_name))
}

/// Path to the results database for the current project.
pub(crate) fn results_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("results.db")
}
