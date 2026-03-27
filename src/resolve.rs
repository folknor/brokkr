use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::DevError;
use crate::preflight;

// ---------------------------------------------------------------------------
// FileEntry trait — unifies PBF, OSC, and PMTiles entry types
// ---------------------------------------------------------------------------

pub(crate) trait FileEntry {
    fn file(&self) -> &str;
    fn xxhash(&self) -> Option<&str>;
}

impl FileEntry for config::PbfEntry {
    fn file(&self) -> &str {
        &self.file
    }
    fn xxhash(&self) -> Option<&str> {
        self.xxhash.as_deref()
    }
}

impl FileEntry for config::OscEntry {
    fn file(&self) -> &str {
        &self.file
    }
    fn xxhash(&self) -> Option<&str> {
        self.xxhash.as_deref()
    }
}

impl FileEntry for config::PmtilesEntry {
    fn file(&self) -> &str {
        &self.file
    }
    fn xxhash(&self) -> Option<&str> {
        self.xxhash.as_deref()
    }
}

/// Generic file resolver: lookup entry in map → join path → check exists → verify hash.
fn resolve_entry_path<E: FileEntry>(
    entries: &HashMap<String, E>,
    key: &str,
    dataset: &str,
    kind: &str,
    data_dir: &Path,
    origin: Option<&str>,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let entry = entries.get(key).ok_or_else(|| {
        let mut available: Vec<&str> = entries.keys().map(String::as_str).collect();
        available.sort();
        DevError::Config(format!(
            "dataset '{dataset}' has no {kind} '{key}' (available: {})",
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        ))
    })?;
    let path = data_dir.join(entry.file());

    if !path.exists() {
        return Err(DevError::Config(format!(
            "{} file not found: {}",
            kind.to_uppercase(),
            path.display()
        )));
    }

    if let Some(expected) = entry.xxhash() {
        preflight::verify_file_hash(&path, expected, project_root, origin)?;
    }

    Ok(path)
}

/// Generic default resolver: auto-select if exactly one entry, error if zero or multiple.
fn resolve_default_entry_path<E: FileEntry>(
    entries: &HashMap<String, E>,
    dataset: &str,
    kind: &str,
    flag: &str,
    data_dir: &Path,
    origin: Option<&str>,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    if entries.is_empty() {
        return Err(DevError::Config(format!(
            "dataset '{dataset}' has no {kind} configured"
        )));
    }

    if entries.len() > 1 {
        let mut keys: Vec<&str> = entries.keys().map(String::as_str).collect();
        keys.sort();
        return Err(DevError::Config(format!(
            "dataset '{dataset}' has multiple {kind} entries — use {flag} to select (available: {})",
            keys.join(", ")
        )));
    }

    if let Some(key) = entries.keys().next() {
        resolve_entry_path(entries, key, dataset, kind, data_dir, origin, project_root)
    } else {
        unreachable!("entries is non-empty (checked above)")
    }
}

/// Look up a dataset by name, returning it and its origin.
fn get_dataset<'a>(
    dataset: &str,
    paths: &'a config::ResolvedPaths,
) -> Result<&'a config::Dataset, DevError> {
    paths
        .datasets
        .get(dataset)
        .ok_or_else(|| DevError::Config(format!("unknown dataset: {dataset}")))
}

/// Resolve the PBF path from --dataset + --variant.
pub(crate) fn resolve_pbf_path(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = get_dataset(dataset, paths)?;
    resolve_entry_path(
        &ds.pbf,
        variant,
        dataset,
        "pbf variant",
        &paths.data_dir,
        ds.origin.as_deref(),
        project_root,
    )
}

/// Resolve the OSC path from --dataset + --osc-seq.
pub(crate) fn resolve_osc_path(
    dataset: &str,
    seq: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = get_dataset(dataset, paths)?;
    resolve_entry_path(
        &ds.osc,
        seq,
        dataset,
        "osc seq",
        &paths.data_dir,
        ds.origin.as_deref(),
        project_root,
    )
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
    let ds = get_dataset(dataset, paths)?;
    resolve_default_entry_path(
        &ds.osc,
        dataset,
        "osc",
        "--osc-seq",
        &paths.data_dir,
        ds.origin.as_deref(),
        project_root,
    )
}

/// Resolve the bbox from --bbox or dataset config.
///
/// Validates that the bbox has exactly 4 comma-separated floats.
pub(crate) fn resolve_bbox(
    bbox: Option<&str>,
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<String, DevError> {
    let value = if let Some(b) = bbox {
        b.to_owned()
    } else {
        let ds = paths
            .datasets
            .get(dataset)
            .ok_or_else(|| DevError::Config(format!("unknown dataset: {dataset}")))?;
        ds.bbox.clone().ok_or_else(|| {
            DevError::Config(format!(
                "dataset '{dataset}' has no bbox configured (use --bbox)"
            ))
        })?
    };

    validate_bbox(&value)?;
    Ok(value)
}

/// Check that a bbox string has exactly 4 comma-separated floats.
fn validate_bbox(bbox: &str) -> Result<(), DevError> {
    let parts: Vec<&str> = bbox.split(',').collect();
    if parts.len() != 4 {
        return Err(DevError::Config(format!(
            "bbox must have 4 comma-separated values, got {}: '{bbox}'",
            parts.len()
        )));
    }
    for (i, part) in parts.iter().enumerate() {
        if part.trim().parse::<f64>().is_err() {
            return Err(DevError::Config(format!(
                "bbox component {i} is not a number: '{}'",
                part.trim()
            )));
        }
    }
    Ok(())
}

/// Resolve the PMTiles path from --dataset + --tiles variant.
pub(crate) fn resolve_pmtiles_path(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = get_dataset(dataset, paths)?;
    resolve_entry_path(
        &ds.pmtiles,
        variant,
        dataset,
        "pmtiles variant",
        &paths.data_dir,
        ds.origin.as_deref(),
        project_root,
    )
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
    let ds = get_dataset(dataset, paths)?;
    resolve_default_entry_path(
        &ds.pmtiles,
        dataset,
        "pmtiles",
        "--tiles",
        &paths.data_dir,
        ds.origin.as_deref(),
        project_root,
    )
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
    let ds = paths
        .datasets
        .get(dataset)
        .ok_or_else(|| DevError::Config(format!("unknown dataset: {dataset}")))?;
    let dir_name = ds.data_dir.as_ref().ok_or_else(|| {
        DevError::Config(format!("dataset '{dataset}' has no data_dir configured"))
    })?;
    let path = paths.data_dir.join(dir_name);
    if !path.exists() {
        return Err(DevError::Config(format!(
            "data directory not found: {}",
            path.display()
        )));
    }
    Ok(path)
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
            features: Vec::new(),
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
        ds.osc.insert(
            String::from("4706"),
            OscEntry {
                file: String::from("b.osc.gz"),
                xxhash: None,
            },
        );
        ds.osc.insert(
            String::from("4705"),
            OscEntry {
                file: String::from("a.osc.gz"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);

        let paths = mk_paths(Path::new("/irrelevant"), datasets);
        let err = resolve_default_osc_path("denmark", &paths, Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple osc entries"));
        assert!(err.contains("4705, 4706"));
    }

    #[test]
    fn resolve_default_osc_path_uses_single_entry() {
        let dir = unique_test_dir("single-osc");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let osc = dir.join("one.osc.gz");
        std::fs::write(&osc, "x").expect("write");

        let mut ds = empty_dataset();
        ds.osc.insert(
            String::from("4705"),
            OscEntry {
                file: String::from("one.osc.gz"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved =
            resolve_default_osc_path("denmark", &paths, Path::new(".")).expect("resolve");
        assert_eq!(resolved, osc);

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_default_pmtiles_path_errors_when_multiple_variants_exist() {
        let mut ds = empty_dataset();
        ds.pmtiles.insert(
            String::from("z"),
            PmtilesEntry {
                file: String::from("z.pmtiles"),
                xxhash: None,
            },
        );
        ds.pmtiles.insert(
            String::from("a"),
            PmtilesEntry {
                file: String::from("a.pmtiles"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);

        let paths = mk_paths(Path::new("/irrelevant"), datasets);
        let err = resolve_default_pmtiles_path("denmark", &paths, Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(err.contains("multiple pmtiles entries"));
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
            PbfEntry {
                file: String::from("raw.osm.pbf"),
                xxhash: None,
                seq: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(Path::new("/data-root"), datasets);

        let err = resolve_nidhogg_data_dir("denmark", &paths)
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no data_dir configured"));
    }
}
