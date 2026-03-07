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
        let mut available: Vec<&str> = ds.pbf.keys().map(String::as_str).collect();
        available.sort();
        DevError::Config(format!(
            "dataset '{dataset}' has no pbf variant '{variant}' (available: {})",
            if available.is_empty() { "none".to_string() } else { available.join(", ") }
        ))
    })?;
    let path = paths.data_dir.join(&entry.file);
    let hash = entry.xxhash.as_deref();
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
        let mut available: Vec<&str> = ds.osc.keys().map(String::as_str).collect();
        available.sort();
        DevError::Config(format!(
            "dataset '{dataset}' has no osc seq '{seq}' (available: {})",
            if available.is_empty() { "none".to_string() } else { available.join(", ") }
        ))
    })?;
    let path = paths.data_dir.join(&entry.file);
    let hash = entry.xxhash.as_deref();
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

    // SAFETY: checked len == 1 above.
    let Some((seq, _)) = ds.osc.iter().next() else {
        return Err(DevError::Config(format!(
            "dataset '{dataset}' has no osc files configured"
        )));
    };
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

/// Resolve the PMTiles path from --dataset + --tiles variant.
pub(crate) fn resolve_pmtiles_path(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;
    let entry = ds.pmtiles.get(variant).ok_or_else(|| {
        let mut available: Vec<&str> = ds.pmtiles.keys().map(String::as_str).collect();
        available.sort();
        DevError::Config(format!(
            "dataset '{dataset}' has no pmtiles variant '{variant}' (available: {})",
            if available.is_empty() { "none".to_string() } else { available.join(", ") }
        ))
    })?;
    let path = paths.data_dir.join(&entry.file);
    let hash = entry.xxhash.as_deref();
    let origin = ds.origin.as_deref();

    if !path.exists() {
        return Err(DevError::Config(format!(
            "PMTiles file not found: {}",
            path.display()
        )));
    }

    if let Some(expected) = hash {
        preflight::verify_file_hash(&path, expected, project_root, origin)?;
    }

    Ok(path)
}

/// Resolve the default PMTiles path when no variant is specified.
///
/// If exactly one PMTiles entry is configured, returns it. If multiple exist,
/// errors with a message listing available variants.
pub(crate) fn resolve_default_pmtiles_path(
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = paths.datasets.get(dataset).ok_or_else(|| {
        DevError::Config(format!("unknown dataset: {dataset}"))
    })?;

    if ds.pmtiles.is_empty() {
        return Err(DevError::Config(format!(
            "dataset '{dataset}' has no pmtiles configured"
        )));
    }

    if ds.pmtiles.len() > 1 {
        let mut variants: Vec<&str> = ds.pmtiles.keys().map(String::as_str).collect();
        variants.sort();
        return Err(DevError::Config(format!(
            "dataset '{dataset}' has multiple pmtiles variants — use --tiles to select (available: {})",
            variants.join(", ")
        )));
    }

    // SAFETY: checked len == 1 above.
    let Some((variant, _)) = ds.pmtiles.iter().next() else {
        return Err(DevError::Config(format!(
            "dataset '{dataset}' has no pmtiles configured"
        )));
    };
    resolve_pmtiles_path(dataset, variant, paths, project_root)
}

/// Resolve PMTiles path and its size in one call.
pub(crate) fn resolve_pmtiles_with_size(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, f64), DevError> {
    let path = resolve_pmtiles_path(dataset, variant, paths, project_root)?;
    let mb = file_size_mb(&path)?;
    Ok((path, mb))
}

/// Resolve default PMTiles path and its size in one call.
pub(crate) fn resolve_default_pmtiles_with_size(
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, f64), DevError> {
    let path = resolve_default_pmtiles_path(dataset, paths, project_root)?;
    let mb = file_size_mb(&path)?;
    Ok((path, mb))
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::{Dataset, OscEntry, PbfEntry, PmtilesEntry, ResolvedPaths};

    use super::*;

    fn unique_test_dir(name: &str) -> PathBuf {
        let cwd = std::env::current_dir().expect("cwd");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        cwd.join(".brokkr")
            .join("test-artifacts")
            .join(format!("resolve-{name}-{}-{stamp}", std::process::id()))
    }

    fn mk_paths(data_dir: &Path, datasets: HashMap<String, Dataset>) -> ResolvedPaths {
        ResolvedPaths {
            hostname: String::from("test-host"),
            data_dir: data_dir.to_path_buf(),
            scratch_dir: data_dir.join("scratch"),
            target_dir: data_dir.join("target"),
            drives: None,
            preview: None,
            datasets,
        }
    }

    fn empty_dataset() -> Dataset {
        Dataset {
            origin: None,
            download_date: None,
            bbox: None,
            data_dir: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
            pmtiles: HashMap::new(),
        }
    }

    #[test]
    fn resolve_default_osc_path_errors_when_multiple_variants_exist() {
        let mut ds = empty_dataset();
        ds.osc.insert(String::from("4706"), OscEntry { file: String::from("b.osc.gz"), xxhash: None });
        ds.osc.insert(String::from("4705"), OscEntry { file: String::from("a.osc.gz"), xxhash: None });
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);

        let paths = mk_paths(Path::new("/irrelevant"), datasets);
        let err = resolve_default_osc_path("denmark", &paths, Path::new(".")).unwrap_err().to_string();
        assert!(err.contains("multiple osc files"));
        assert!(err.contains("4705, 4706"));
    }

    #[test]
    fn resolve_default_osc_path_uses_single_entry() {
        let dir = unique_test_dir("single-osc");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let osc = dir.join("one.osc.gz");
        std::fs::write(&osc, "x").expect("write");

        let mut ds = empty_dataset();
        ds.osc.insert(String::from("4705"), OscEntry { file: String::from("one.osc.gz"), xxhash: None });
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_default_osc_path("denmark", &paths, Path::new(".")).expect("resolve");
        assert_eq!(resolved, osc);

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_default_pmtiles_path_errors_when_multiple_variants_exist() {
        let mut ds = empty_dataset();
        ds.pmtiles.insert(String::from("z"), PmtilesEntry { file: String::from("z.pmtiles"), xxhash: None });
        ds.pmtiles.insert(String::from("a"), PmtilesEntry { file: String::from("a.pmtiles"), xxhash: None });
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);

        let paths = mk_paths(Path::new("/irrelevant"), datasets);
        let err = resolve_default_pmtiles_path("denmark", &paths, Path::new(".")).unwrap_err().to_string();
        assert!(err.contains("multiple pmtiles variants"));
        assert!(err.contains("a, z"));
    }

    #[test]
    fn resolve_bbox_prefers_arg_then_dataset() {
        let mut ds = empty_dataset();
        ds.bbox = Some(String::from("1,2,3,4"));
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let explicit = resolve_bbox(Some("9,9,9,9"), "denmark", &paths).expect("bbox");
        assert_eq!(explicit, "9,9,9,9");

        let from_dataset = resolve_bbox(None, "denmark", &paths).expect("bbox");
        assert_eq!(from_dataset, "1,2,3,4");
    }

    #[test]
    fn resolve_nidhogg_data_dir_requires_configured_data_dir() {
        let mut ds = empty_dataset();
        ds.pbf.insert(
            String::from("raw"),
            PbfEntry { file: String::from("raw.osm.pbf"), xxhash: None, seq: None },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(Path::new("/data-root"), datasets);

        let err = resolve_nidhogg_data_dir("denmark", &paths).unwrap_err().to_string();
        assert!(err.contains("has no data_dir configured"));
    }
}
