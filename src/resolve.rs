use std::path::{Path, PathBuf};

use crate::config;
use crate::error::DevError;
use crate::preflight;

/// Resolve the PBF path from --dataset + --variant.
pub(crate) fn resolve_pbf_path(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;
    let entry = ds.pbf.get(variant).ok_or_else(|| {
        DevError::Config(format!(
            "dataset '{dataset}' has no pbf variant '{variant}'"
        ))
    })?;
    let path = paths.data_dir.join(&entry.file);
    let hash = entry.sha256.as_deref();
    let origin = ds.origin.as_deref();

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

/// Resolve the OSC path from --dataset + --osc-seq.
pub(crate) fn resolve_osc_path(
    dataset: &str,
    seq: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;
    let entry = ds.osc.get(seq).ok_or_else(|| {
        let available: Vec<&str> = ds.osc.keys().map(String::as_str).collect();
        DevError::Config(format!(
            "dataset '{dataset}' has no osc seq '{seq}' (available: {})",
            if available.is_empty() { "none".to_string() } else { available.join(", ") }
        ))
    })?;
    let path = paths.data_dir.join(&entry.file);
    let hash = entry.sha256.as_deref();
    let origin = ds.origin.as_deref();

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

/// Resolve the default OSC path when no --osc-seq is specified.
///
/// If exactly one OSC is configured, returns it. If multiple exist, errors
/// with a message listing available sequence numbers.
pub(crate) fn resolve_default_osc_path(
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;

    if ds.osc.is_empty() {
        return Err(DevError::Config(format!(
            "dataset '{dataset}' has no osc files configured"
        )));
    }

    if ds.osc.len() > 1 {
        let mut seqs: Vec<&str> = ds.osc.keys().map(String::as_str).collect();
        seqs.sort();
        return Err(DevError::Config(format!(
            "dataset '{dataset}' has multiple osc files — use --osc-seq to select (available: {})",
            seqs.join(", ")
        )));
    }

    let (seq, _) = ds.osc.iter().next().unwrap();
    resolve_osc_path(dataset, seq, paths, project_root)
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

/// Get file size in MB (decimal, consistent with bench scripts).
pub(crate) fn file_size_mb(path: &Path) -> Result<f64, DevError> {
    let meta = std::fs::metadata(path)?;
    Ok(meta.len() as f64 / 1_000_000.0)
}

/// Resolve PBF path and its size in one call.
pub(crate) fn resolve_pbf_with_size(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, f64), DevError> {
    let path = resolve_pbf_path(dataset, variant, paths, project_root)?;
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

/// Get a PBF entry reference for direct field access (e.g. checking if a variant exists).
pub(crate) fn get_pbf_entry<'a>(
    dataset: &str,
    variant: &str,
    paths: &'a config::ResolvedPaths,
) -> Option<&'a config::PbfEntry> {
    paths.datasets.get(dataset)?.pbf.get(variant)
}

/// Get the first available OSC entry for optional lookups (e.g. bench-all).
pub(crate) fn get_default_osc_entry<'a>(
    dataset: &str,
    paths: &'a config::ResolvedPaths,
) -> Option<&'a config::OscEntry> {
    let ds = paths.datasets.get(dataset)?;
    if ds.osc.len() == 1 {
        ds.osc.values().next()
    } else {
        None
    }
}

/// Path to the results database for the current project.
pub(crate) fn results_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("results.db")
}
