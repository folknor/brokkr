use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::DevError;
use crate::preflight;

// ---------------------------------------------------------------------------
// FileEntry trait - unifies PBF, OSC, and PMTiles entry types
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
    finalize_entry_path(entry, data_dir, kind, origin, project_root)
}

/// Tail of `resolve_entry_path`: build the on-disk path, check existence,
/// verify hash. Extracted so callers that want a custom "missing key" error
/// (e.g. the snapshot pbf path) can reuse the file-checking logic.
fn finalize_entry_path<E: FileEntry>(
    entry: &E,
    data_dir: &Path,
    kind: &str,
    origin: Option<&str>,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
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
            "dataset '{dataset}' has multiple {kind} entries - use {flag} to select (available: {})",
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

// ---------------------------------------------------------------------------
// Snapshot resolution
// ---------------------------------------------------------------------------

/// A reference to a dataset snapshot used by `diff-snapshots` and any future
/// command that takes snapshot pairs as input.
///
/// `Base` refers to the dataset's legacy top-level `pbf`/`osc` data - the
/// "primary" snapshot. `Named(key)` refers to a snapshot registered under
/// `[dataset.snapshot.<key>]` in `brokkr.toml`. The CLI string `"base"` parses
/// to `Base`; any other string parses to `Named` (after key validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotRef {
    Base,
    Named(String),
}

impl SnapshotRef {
    /// Parse a `--from` / `--to` CLI argument into a `SnapshotRef`.
    /// `"base"` (case-sensitive) → `Base`. Anything else is validated as a
    /// snapshot key (`[a-zA-Z0-9_-]+`) and wrapped in `Named`.
    pub(crate) fn parse(s: &str) -> Result<Self, DevError> {
        if s == "base" {
            return Ok(Self::Base);
        }
        crate::config::validate_snapshot_key(s).map_err(DevError::Config)?;
        Ok(Self::Named(s.to_owned()))
    }
}

/// Get the OSC HashMap for a snapshot ref. `Base` returns the dataset's
/// top-level `osc` table; `Named(key)` returns the snapshot's `osc` table.
/// Errors if the snapshot key isn't registered.
fn snapshot_osc_map<'a>(
    dataset: &str,
    snapshot: &SnapshotRef,
    paths: &'a config::ResolvedPaths,
) -> Result<&'a HashMap<String, config::OscEntry>, DevError> {
    let ds = get_dataset(dataset, paths)?;
    match snapshot {
        SnapshotRef::Base => Ok(&ds.osc),
        SnapshotRef::Named(key) => {
            let snap = ds.snapshot.get(key).ok_or_else(|| {
                let mut available: Vec<&str> = std::iter::once("base")
                    .chain(ds.snapshot.keys().map(String::as_str))
                    .collect();
                available.sort();
                DevError::Config(format!(
                    "dataset '{dataset}' has no snapshot '{key}' (available: {})",
                    available.join(", ")
                ))
            })?;
            Ok(&snap.osc)
        }
    }
}

/// Get the dataset's `data_dir` (data files always live in the dataset's
/// data dir; snapshots don't have their own data dir).
fn dataset_data_dir(paths: &config::ResolvedPaths) -> &Path {
    &paths.data_dir
}

/// Resolve a snapshot's PBF path for the given variant.
///
/// `Base` dispatches to the legacy `resolve_pbf_path` (top-level `pbf.<variant>`).
/// `Named(key)` looks up `dataset.snapshot.<key>.pbf.<variant>`.
pub(crate) fn resolve_snapshot_pbf_path(
    dataset: &str,
    snapshot: &SnapshotRef,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    match snapshot {
        SnapshotRef::Base => resolve_pbf_path(dataset, variant, paths, project_root),
        SnapshotRef::Named(key) => {
            let ds = get_dataset(dataset, paths)?;
            let snap = ds.snapshot.get(key).ok_or_else(|| {
                let mut available: Vec<&str> = std::iter::once("base")
                    .chain(ds.snapshot.keys().map(String::as_str))
                    .collect();
                available.sort();
                DevError::Config(format!(
                    "dataset '{dataset}' has no snapshot '{key}' (available: {})",
                    available.join(", ")
                ))
            })?;
            // Custom missing-variant error: name the available variants and
            // suggest both a one-shot workaround (--variant <X>) and the
            // proper fix (re-download to auto-generate). This is the
            // first-time-user papercut from TODO #5 - closing it inline
            // instead of adding the per-side --variant-from / --variant-to
            // flags, since no concrete asymmetric use case has surfaced yet.
            let entry = snap.pbf.get(variant).ok_or_else(|| {
                let mut available: Vec<&str> = snap.pbf.keys().map(String::as_str).collect();
                available.sort();
                let available_str = if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                };
                let suggestion_variant = available.first().copied().unwrap_or("raw");
                DevError::Config(format!(
                    "dataset '{dataset}' has no snapshot.{key}.pbf variant '{variant}'\n  \
                     └─ available variants on this snapshot: {available_str}\n  \
                     └─ pass `--variant {suggestion_variant}` to resolve both sides with the available variant,\n     \
                     or register the {variant} variant via `brokkr download {dataset} --as-snapshot {key}`\n     \
                     (which auto-generates pbf.indexed from pbf.raw)."
                ))
            })?;
            finalize_entry_path(
                entry,
                &paths.data_dir,
                &format!("snapshot.{key}.pbf variant"),
                ds.origin.as_deref(),
                project_root,
            )
        }
    }
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

/// Resolve a single OSC path along with the seq key it came from.
///
/// If `osc_seq` is `Some(seq)`, returns the path for that seq plus the seq
/// itself. If `None`, auto-selects the only configured OSC entry (errors if
/// zero or multiple), returning both its path and its key. The seq key is
/// useful when constructing cache keys that need to disambiguate by OSC
/// (e.g. the diff/diff-osc merged-PBF cache).
pub(crate) fn resolve_single_osc(
    dataset: &str,
    snapshot: &SnapshotRef,
    osc_seq: Option<&str>,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, String), DevError> {
    let osc_map = snapshot_osc_map(dataset, snapshot, paths)?;
    let ds = get_dataset(dataset, paths)?;
    let origin = ds.origin.as_deref();
    let data_dir = dataset_data_dir(paths);
    let kind_label = match snapshot {
        SnapshotRef::Base => "osc seq".to_string(),
        SnapshotRef::Named(k) => format!("snapshot.{k}.osc seq"),
    };

    if let Some(seq) = osc_seq {
        let path = resolve_entry_path(
            osc_map,
            seq,
            dataset,
            &kind_label,
            data_dir,
            origin,
            project_root,
        )?;
        return Ok((path, seq.to_string()));
    }

    // Auto-select: requires exactly one configured OSC under the snapshot.
    if osc_map.is_empty() {
        let where_clause = match snapshot {
            SnapshotRef::Base => format!("dataset '{dataset}'"),
            SnapshotRef::Named(k) => format!("dataset '{dataset}' snapshot '{k}'"),
        };
        return Err(DevError::Config(format!(
            "{where_clause} has no osc configured"
        )));
    }
    if osc_map.len() > 1 {
        let mut keys: Vec<&str> = osc_map.keys().map(String::as_str).collect();
        keys.sort();
        let where_clause = match snapshot {
            SnapshotRef::Base => format!("dataset '{dataset}'"),
            SnapshotRef::Named(k) => format!("dataset '{dataset}' snapshot '{k}'"),
        };
        return Err(DevError::Config(format!(
            "{where_clause} has multiple osc entries - use --osc-seq to select (available: {})",
            keys.join(", ")
        )));
    }
    let key = osc_map.keys().next().expect("len == 1").clone();
    let path = resolve_entry_path(
        osc_map,
        &key,
        dataset,
        &kind_label,
        data_dir,
        origin,
        project_root,
    )?;
    Ok((path, key))
}

/// Resolve a range of OSC paths from `--osc-range LO..HI`.
///
/// Every integer seq in `[LO, HI]` must be present in the dataset's osc map;
/// a missing seq fails fast with `missing osc.X`. Returns paths in ascending
/// seq order. The range string must be pre-validated in `LO..HI` form.
pub(crate) fn resolve_osc_range(
    dataset: &str,
    snapshot: &SnapshotRef,
    range: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<Vec<PathBuf>, DevError> {
    let (lo_s, hi_s) = range
        .split_once("..")
        .ok_or_else(|| DevError::Config(format!("invalid osc range '{range}': expected LO..HI")))?;
    let lo: u64 = lo_s
        .parse()
        .map_err(|e| DevError::Config(format!("invalid osc range LO '{lo_s}': {e}")))?;
    let hi: u64 = hi_s
        .parse()
        .map_err(|e| DevError::Config(format!("invalid osc range HI '{hi_s}': {e}")))?;
    if lo > hi {
        return Err(DevError::Config(format!(
            "osc range LO ({lo}) must be <= HI ({hi})"
        )));
    }

    let osc_map = snapshot_osc_map(dataset, snapshot, paths)?;
    let ds = get_dataset(dataset, paths)?;
    let origin = ds.origin.as_deref();
    let data_dir = dataset_data_dir(paths);
    let kind_label = match snapshot {
        SnapshotRef::Base => "osc seq".to_string(),
        SnapshotRef::Named(k) => format!("snapshot.{k}.osc seq"),
    };
    let where_clause = match snapshot {
        SnapshotRef::Base => format!("dataset '{dataset}'"),
        SnapshotRef::Named(k) => format!("dataset '{dataset}' snapshot '{k}'"),
    };

    #[allow(clippy::cast_possible_truncation)]
    let mut resolved = Vec::with_capacity((hi - lo + 1) as usize);
    for seq in lo..=hi {
        let key = seq.to_string();
        if !osc_map.contains_key(&key) {
            return Err(DevError::Config(format!(
                "{where_clause} missing osc.{seq} (required by --osc-range {range})"
            )));
        }
        let path = resolve_entry_path(
            osc_map,
            &key,
            dataset,
            &kind_label,
            data_dir,
            origin,
            project_root,
        )?;
        resolved.push(path);
    }
    Ok(resolved)
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

/// Path to the sidecar profile database (gitignored, local-only).
pub(crate) fn sidecar_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("sidecar.db")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::panic,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::too_many_arguments,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::approx_constant,
        clippy::needless_pass_by_value,
        clippy::let_underscore_must_use,
        clippy::useless_vec
    )]
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
            snapshot: HashMap::new(),
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
    fn snapshot_ref_parses_base_sentinel() {
        assert!(matches!(SnapshotRef::parse("base").unwrap(), SnapshotRef::Base));
    }

    #[test]
    fn snapshot_ref_parses_named_keys() {
        let parsed = SnapshotRef::parse("20260411").unwrap();
        assert!(matches!(parsed, SnapshotRef::Named(ref s) if s == "20260411"));
    }

    #[test]
    fn snapshot_ref_rejects_invalid_chars() {
        let err = SnapshotRef::parse("not a key").unwrap_err().to_string();
        assert!(err.contains("[a-zA-Z0-9_-]+"), "got: {err}");
    }

    #[test]
    fn snapshot_ref_rejects_empty() {
        let err = SnapshotRef::parse("").unwrap_err().to_string();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn resolve_snapshot_pbf_path_base_uses_legacy_table() {
        let dir = unique_test_dir("snap-base");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-base.osm.pbf"), "x").expect("write");

        let mut ds = empty_dataset();
        ds.pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "planet-base.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_snapshot_pbf_path(
            "planet",
            &SnapshotRef::Base,
            "indexed",
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(resolved.ends_with("planet-base.osm.pbf"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_snapshot_pbf_path_named_uses_snapshot_table() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-named");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-20260411.osm.pbf"), "x").expect("write");

        let mut snap = Snapshot {
            download_date: Some("2026-04-11".into()),
            seq: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
        };
        snap.pbf.insert(
            "raw".into(),
            PbfEntry {
                file: "planet-20260411.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_snapshot_pbf_path(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            "raw",
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(resolved.ends_with("planet-20260411.osm.pbf"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_snapshot_pbf_path_missing_variant_emits_friendly_error() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-missing-variant");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-20260411.osm.pbf"), "x").expect("write");

        // Snapshot has only `raw` - user asks for `indexed`. The error should
        // name `raw` as available and suggest both --variant raw and the
        // re-download path.
        let mut snap = Snapshot {
            download_date: Some("2026-04-11".into()),
            seq: None,
            pbf: HashMap::new(),
            osc: HashMap::new(),
        };
        snap.pbf.insert(
            "raw".into(),
            PbfEntry {
                file: "planet-20260411.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let err = resolve_snapshot_pbf_path(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            "indexed",
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();

        // Names the missing variant and the available one.
        assert!(
            err.contains("snapshot.20260411.pbf variant 'indexed'"),
            "got: {err}"
        );
        assert!(
            err.contains("available variants on this snapshot: raw"),
            "got: {err}"
        );
        // Names the workaround flag with the actual available variant.
        assert!(err.contains("--variant raw"), "got: {err}");
        // Names the re-download path as the proper fix.
        assert!(
            err.contains("brokkr download planet --as-snapshot 20260411"),
            "got: {err}"
        );

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_snapshot_pbf_path_errors_on_unknown_named_key() {
        let mut ds = empty_dataset();
        ds.pbf.insert(
            "indexed".into(),
            PbfEntry {
                file: "planet-base.osm.pbf".into(),
                xxhash: None,
                seq: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let err = resolve_snapshot_pbf_path(
            "planet",
            &SnapshotRef::Named("missing-snap".into()),
            "indexed",
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no snapshot 'missing-snap'"), "got: {err}");
        assert!(err.contains("base"), "available list should mention base: {err}");
    }

    #[test]
    fn resolve_single_osc_returns_explicit_seq() {
        let dir = unique_test_dir("single-osc-explicit");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-4914.osc.gz"), "x").expect("write");

        let mut ds = empty_dataset();
        ds.osc.insert(
            String::from("4914"),
            OscEntry {
                file: String::from("planet-4914.osc.gz"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("planet"), ds);
        let paths = mk_paths(&dir, datasets);

        let (path, seq) = resolve_single_osc(
            "planet",
            &SnapshotRef::Base,
            Some("4914"),
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(path.ends_with("planet-4914.osc.gz"));
        assert_eq!(seq, "4914");

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_single_osc_auto_selects_when_one_configured() {
        let dir = unique_test_dir("single-osc-auto");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("denmark-4705.osc.gz"), "x").expect("write");

        let mut ds = empty_dataset();
        ds.osc.insert(
            String::from("4705"),
            OscEntry {
                file: String::from("denmark-4705.osc.gz"),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert(String::from("denmark"), ds);
        let paths = mk_paths(&dir, datasets);

        let (path, seq) = resolve_single_osc(
            "denmark",
            &SnapshotRef::Base,
            None,
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(path.ends_with("denmark-4705.osc.gz"));
        assert_eq!(seq, "4705");

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_single_osc_errors_when_multiple_and_no_seq() {
        let mut ds = empty_dataset();
        for n in [4913u64, 4914, 4915] {
            ds.osc.insert(
                n.to_string(),
                OscEntry {
                    file: format!("planet-{n}.osc.gz"),
                    xxhash: None,
                },
            );
        }
        let mut datasets = HashMap::new();
        datasets.insert(String::from("planet"), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let err = resolve_single_osc(
            "planet",
            &SnapshotRef::Base,
            None,
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("multiple osc entries"), "got: {err}");
        assert!(err.contains("4913, 4914, 4915"), "got: {err}");
    }

    #[test]
    fn resolve_single_osc_named_snapshot_uses_snapshot_table() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-osc-named");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-20260411-seq4969.osc.gz"), "x").expect("write");

        let mut snap = Snapshot {
            download_date: Some("2026-04-11".into()),
            seq: Some(4969),
            pbf: HashMap::new(),
            osc: HashMap::new(),
        };
        snap.osc.insert(
            "4969".into(),
            OscEntry {
                file: "planet-20260411-seq4969.osc.gz".into(),
                xxhash: None,
            },
        );
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let (path, seq) = resolve_single_osc(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            Some("4969"),
            &paths,
            Path::new("."),
        )
        .expect("resolve");
        assert!(path.ends_with("planet-20260411-seq4969.osc.gz"));
        assert_eq!(seq, "4969");

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_osc_range_named_snapshot_uses_snapshot_table() {
        use crate::config::Snapshot;

        let dir = unique_test_dir("snap-osc-range");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for n in 4969..=4971 {
            std::fs::write(dir.join(format!("planet-20260411-seq{n}.osc.gz")), "x")
                .expect("write");
        }

        let mut snap = Snapshot {
            download_date: Some("2026-04-11".into()),
            seq: Some(4969),
            pbf: HashMap::new(),
            osc: HashMap::new(),
        };
        for n in 4969..=4971u64 {
            snap.osc.insert(
                n.to_string(),
                OscEntry {
                    file: format!("planet-20260411-seq{n}.osc.gz"),
                    xxhash: None,
                },
            );
        }
        let mut ds = empty_dataset();
        ds.snapshot.insert("20260411".into(), snap);
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_osc_range(
            "planet",
            &SnapshotRef::Named("20260411".into()),
            "4969..4971",
            &paths,
            Path::new("."),
        )
        .expect("range");
        assert_eq!(resolved.len(), 3);
        assert!(resolved[0].ends_with("planet-20260411-seq4969.osc.gz"));
        assert!(resolved[2].ends_with("planet-20260411-seq4971.osc.gz"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_single_osc_named_snapshot_unknown_key_errors() {
        let mut ds = empty_dataset();
        ds.osc.insert(
            "4913".into(),
            OscEntry {
                file: "planet-4913.osc.gz".into(),
                xxhash: None,
            },
        );
        let mut datasets = HashMap::new();
        datasets.insert("planet".into(), ds);
        let paths = mk_paths(Path::new("/irrelevant"), datasets);

        let err = resolve_single_osc(
            "planet",
            &SnapshotRef::Named("missing".into()),
            None,
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no snapshot 'missing'"), "got: {err}");
    }

    #[test]
    fn resolve_osc_range_returns_paths_in_seq_order() {
        let dir = unique_test_dir("osc-range-ok");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for n in 4914..=4916 {
            std::fs::write(dir.join(format!("planet-{n}.osc.gz")), "x").expect("write");
        }

        let mut ds = empty_dataset();
        for n in 4914..=4916u64 {
            ds.osc.insert(
                n.to_string(),
                OscEntry {
                    file: format!("planet-{n}.osc.gz"),
                    xxhash: None,
                },
            );
        }
        let mut datasets = HashMap::new();
        datasets.insert(String::from("planet"), ds);
        let paths = mk_paths(&dir, datasets);

        let resolved = resolve_osc_range(
            "planet",
            &SnapshotRef::Base,
            "4914..4916",
            &paths,
            Path::new("."),
        )
        .expect("range");
        assert_eq!(resolved.len(), 3);
        assert!(resolved[0].ends_with("planet-4914.osc.gz"));
        assert!(resolved[1].ends_with("planet-4915.osc.gz"));
        assert!(resolved[2].ends_with("planet-4916.osc.gz"));

        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn resolve_osc_range_fails_fast_on_missing_seq() {
        let dir = unique_test_dir("osc-range-missing");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("planet-4914.osc.gz"), "x").expect("write");
        std::fs::write(dir.join("planet-4916.osc.gz"), "x").expect("write");

        let mut ds = empty_dataset();
        for n in [4914u64, 4916] {
            ds.osc.insert(
                n.to_string(),
                OscEntry {
                    file: format!("planet-{n}.osc.gz"),
                    xxhash: None,
                },
            );
        }
        let mut datasets = HashMap::new();
        datasets.insert(String::from("planet"), ds);
        let paths = mk_paths(&dir, datasets);

        let err = resolve_osc_range(
            "planet",
            &SnapshotRef::Base,
            "4914..4916",
            &paths,
            Path::new("."),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("missing osc.4915"), "got: {err}");

        drop(std::fs::remove_dir_all(&dir));
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
