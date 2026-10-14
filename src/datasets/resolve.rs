//! Resolve registered dataset entries and verify their input files.

use super::Bbox;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::error::ResolveError;
use crate::config;
use crate::preflight::{self, FileDigest};

// ---------------------------------------------------------------------------
// FileEntry trait - unifies PBF, OSC, and PMTiles entry types
// ---------------------------------------------------------------------------

pub(super) trait FileEntry {
    fn file(&self) -> &str;
    fn xxhash(&self) -> Option<&FileDigest>;
}

impl FileEntry for config::PbfEntry {
    fn file(&self) -> &str {
        &self.file
    }
    fn xxhash(&self) -> Option<&FileDigest> {
        self.xxhash.as_ref()
    }
}

impl FileEntry for config::OscEntry {
    fn file(&self) -> &str {
        &self.file
    }
    fn xxhash(&self) -> Option<&FileDigest> {
        self.xxhash.as_ref()
    }
}

impl FileEntry for config::PmtilesEntry {
    fn file(&self) -> &str {
        &self.file
    }
    fn xxhash(&self) -> Option<&FileDigest> {
        self.xxhash.as_ref()
    }
}

/// Generic file resolver: lookup entry in map -> join path -> check exists -> verify hash.
fn resolve_entry_path<E: FileEntry>(
    entries: &HashMap<String, E>,
    key: &str,
    dataset: &str,
    kind: &str,
    data_dir: &Path,
    origin: Option<&str>,
    project_root: &Path,
) -> Result<PathBuf, ResolveError> {
    let entry = get_entry(entries, key, dataset, kind)?;
    finalize_entry_path(entry, data_dir, kind, origin, project_root)
}

fn get_entry<'a, E>(
    entries: &'a HashMap<String, E>,
    key: &str,
    dataset: &str,
    kind: &str,
) -> Result<&'a E, ResolveError> {
    entries.get(key).ok_or_else(|| {
        let mut available: Vec<String> = entries.keys().cloned().collect();
        available.sort();
        ResolveError::UnknownEntry {
            dataset: dataset.to_owned(),
            kind: kind.to_owned(),
            key: key.to_owned(),
            available,
        }
    })
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
) -> Result<PathBuf, ResolveError> {
    let path = data_dir.join(entry.file());

    if !path.exists() {
        return Err(ResolveError::MissingFile {
            kind: kind.to_owned(),
            path,
        });
    }

    if let Some(expected) = entry.xxhash() {
        preflight::verify_file_digest(&path, expected, project_root, origin)?;
    }

    Ok(path)
}

/// Select the sole configured entry, with deterministic diagnostics for zero or many.
fn select_one<'a, E>(
    entries: &'a HashMap<String, E>,
    scope: &str,
    kind: &str,
    flag: &str,
) -> Result<(&'a String, &'a E), ResolveError> {
    if entries.is_empty() {
        return Err(ResolveError::EmptySelection {
            scope: scope.to_owned(),
            kind: kind.to_owned(),
        });
    }
    if entries.len() > 1 {
        let mut available: Vec<String> = entries.keys().cloned().collect();
        available.sort();
        return Err(ResolveError::AmbiguousSelection {
            scope: scope.to_owned(),
            kind: kind.to_owned(),
            flag: flag.to_owned(),
            available,
        });
    }
    Ok(entries.iter().next().expect("exactly one entry"))
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
) -> Result<PathBuf, ResolveError> {
    let (key, _) = select_one(entries, &format!("dataset '{dataset}'"), kind, flag)?;
    resolve_entry_path(entries, key, dataset, kind, data_dir, origin, project_root)
}

/// Look up a dataset by name, returning it and its origin.
///
/// This resolved table is authoritative for both paths and entry metadata.
/// Callers that need a selected entry's flags should carry that entry from
/// here instead of looking it up again through the raw project config.
fn get_dataset<'a>(
    dataset: &str,
    paths: &'a config::ResolvedPaths,
) -> Result<&'a config::Dataset, ResolveError> {
    paths
        .datasets
        .get(dataset)
        .ok_or_else(|| ResolveError::UnknownDataset(dataset.to_owned()))
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

/// The selected PBF config entry and its validated on-disk path.
///
/// Keep file-specific assertions attached to the same entry the resolver
/// checked, rather than looking them up again through the raw host config.
pub(crate) struct ResolvedPbf<'a> {
    pub(crate) path: PathBuf,
    pub(crate) entry: &'a config::PbfEntry,
}

impl SnapshotRef {
    /// Parse a `--from` / `--to` CLI argument into a `SnapshotRef`.
    /// `"base"` (case-sensitive) -> `Base`. Anything else is validated as a
    /// snapshot key (`[a-zA-Z0-9_-]+`) and wrapped in `Named`.
    pub(crate) fn parse(s: &str) -> Result<Self, ResolveError> {
        if s == "base" {
            return Ok(Self::Base);
        }
        crate::config::validate_snapshot_key(s).map_err(ResolveError::InvalidSnapshot)?;
        Ok(Self::Named(s.to_owned()))
    }

    /// Select an already-parsed CLI snapshot, defaulting to the primary data.
    pub(crate) fn from_opt(snapshot: Option<&Self>) -> Self {
        snapshot.cloned().unwrap_or(Self::Base)
    }
}

impl std::str::FromStr for SnapshotRef {
    type Err = ResolveError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl std::fmt::Display for SnapshotRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl SnapshotRef {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Base => "base",
            Self::Named(key) => key,
        }
    }
}

/// Get the OSC HashMap for a snapshot ref. `Base` returns the dataset's
/// top-level `osc` table; `Named(key)` returns the snapshot's `osc` table.
/// Errors if the snapshot key isn't registered.
fn snapshot_osc_map<'a>(
    dataset: &str,
    snapshot: &SnapshotRef,
    paths: &'a config::ResolvedPaths,
) -> Result<&'a HashMap<String, config::OscEntry>, ResolveError> {
    let ds = get_dataset(dataset, paths)?;
    match snapshot {
        SnapshotRef::Base => Ok(&ds.osc),
        SnapshotRef::Named(key) => {
            let snap = ds.snapshot.get(key).ok_or_else(|| {
                let mut available: Vec<&str> = std::iter::once("base")
                    .chain(ds.snapshot.keys().map(String::as_str))
                    .collect();
                available.sort();
                ResolveError::UnknownSnapshot {
                    dataset: dataset.to_owned(),
                    key: key.clone(),
                    available: available.into_iter().map(str::to_owned).collect(),
                }
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
/// `Base` selects the top-level `pbf.<variant>` entry. `Named(key)` selects
/// `dataset.snapshot.<key>.pbf.<variant>`.
pub(crate) fn resolve_snapshot_pbf_path(
    dataset: &str,
    snapshot: &SnapshotRef,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, ResolveError> {
    Ok(resolve_snapshot_pbf_entry(dataset, snapshot, variant, paths, project_root)?.path)
}

/// Resolve a snapshot PBF and retain the exact entry that supplied its path.
pub(crate) fn resolve_snapshot_pbf_entry<'a>(
    dataset: &str,
    snapshot: &SnapshotRef,
    variant: &str,
    paths: &'a config::ResolvedPaths,
    project_root: &Path,
) -> Result<ResolvedPbf<'a>, ResolveError> {
    let ds = get_dataset(dataset, paths)?;
    let (entry, kind) = match snapshot {
        SnapshotRef::Base => (
            get_entry(&ds.pbf, variant, dataset, "pbf variant")?,
            "pbf variant".to_owned(),
        ),
        SnapshotRef::Named(key) => {
            let snap = ds.snapshot.get(key).ok_or_else(|| {
                let mut available: Vec<&str> = std::iter::once("base")
                    .chain(ds.snapshot.keys().map(String::as_str))
                    .collect();
                available.sort();
                ResolveError::UnknownSnapshot {
                    dataset: dataset.to_owned(),
                    key: key.clone(),
                    available: available.into_iter().map(str::to_owned).collect(),
                }
            })?;
            // Custom missing-variant error: name the available variants and
            // suggest both a one-shot workaround (--variant <X>) and the
            // proper fix (re-download to auto-generate). This closes a
            // first-time-user papercut inline instead of adding the per-side --variant-from / --variant-to
            // flags, since no concrete asymmetric use case has surfaced yet.
            let entry = snap.pbf.get(variant).ok_or_else(|| {
                let mut available: Vec<&str> = snap.pbf.keys().map(String::as_str).collect();
                available.sort();
                ResolveError::SnapshotVariant {
                    dataset: dataset.to_owned(),
                    snapshot: key.clone(),
                    variant: variant.to_owned(),
                    available: available.into_iter().map(str::to_owned).collect(),
                }
            })?;
            (entry, format!("snapshot.{key}.pbf variant"))
        }
    };
    let path = finalize_entry_path(
        entry,
        &paths.data_dir,
        &kind,
        ds.origin.as_deref(),
        project_root,
    )?;
    Ok(ResolvedPbf { path, entry })
}

/// Resolve the selected PBF entry from the host's resolved dataset table.
pub(crate) fn resolve_pbf_entry<'a>(
    dataset: &str,
    variant: &str,
    paths: &'a config::ResolvedPaths,
    project_root: &Path,
) -> Result<ResolvedPbf<'a>, ResolveError> {
    resolve_snapshot_pbf_entry(dataset, &SnapshotRef::Base, variant, paths, project_root)
}

/// Resolve the PBF path from --dataset + --variant.
pub(crate) fn resolve_pbf_path(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, ResolveError> {
    Ok(resolve_pbf_entry(dataset, variant, paths, project_root)?.path)
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
) -> Result<(PathBuf, String), ResolveError> {
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

    let where_clause = match snapshot {
        SnapshotRef::Base => format!("dataset '{dataset}'"),
        SnapshotRef::Named(key) => format!("dataset '{dataset}' snapshot '{key}'"),
    };
    let (key, _) = select_one(osc_map, &where_clause, "osc", "--osc-seq")?;
    let key = key.clone();
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
/// seq order. The CLI passes the range after its shared typed parse.
pub(crate) fn resolve_osc_range(
    dataset: &str,
    snapshot: &SnapshotRef,
    range: &config::OscRange,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<Vec<PathBuf>, ResolveError> {
    let (lo, hi) = (range.lo(), range.hi());

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

    let mut resolved = Vec::new();
    for seq in lo..=hi {
        let key = seq.to_string();
        if !osc_map.contains_key(&key) {
            return Err(ResolveError::MissingOsc {
                scope: where_clause.clone(),
                seq,
                range: range.clone(),
            });
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
#[cfg(test)]
pub(crate) fn resolve_default_osc_path(
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, ResolveError> {
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

/// Resolve the already-parsed bbox from --bbox or dataset config.
pub(crate) fn resolve_bbox(
    bbox: Option<&Bbox>,
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<Bbox, ResolveError> {
    if let Some(bbox) = bbox {
        return Ok(bbox.clone());
    }
    let ds = paths
        .datasets
        .get(dataset)
        .ok_or_else(|| ResolveError::UnknownDataset(dataset.to_owned()))?;
    ds.bbox
        .clone()
        .ok_or_else(|| ResolveError::MissingBbox(dataset.to_owned()))
}

/// Resolve the PMTiles path from --dataset + --tiles variant.
pub(crate) fn resolve_pmtiles_path(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, ResolveError> {
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
) -> Result<PathBuf, ResolveError> {
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
) -> Result<(PathBuf, f64), ResolveError> {
    let path = resolve_pmtiles_path(dataset, variant, paths, project_root)?;
    let mb = file_size_mb(&path)?;
    Ok((path, mb))
}

/// Resolve default PMTiles path and its size in one call.
pub(crate) fn resolve_default_pmtiles_with_size(
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, f64), ResolveError> {
    let path = resolve_default_pmtiles_path(dataset, paths, project_root)?;
    let mb = file_size_mb(&path)?;
    Ok((path, mb))
}

/// Get file size in MB (decimal, consistent with bench scripts).
pub(crate) fn file_size_mb(path: &Path) -> Result<f64, ResolveError> {
    let meta = std::fs::metadata(path)?;
    Ok(meta.len() as f64 / 1_000_000.0)
}

/// Resolve PBF path and its size in one call.
pub(crate) fn resolve_pbf_with_size(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<(PathBuf, f64), ResolveError> {
    let path = resolve_pbf_path(dataset, variant, paths, project_root)?;
    let mb = file_size_mb(&path)?;
    Ok((path, mb))
}

#[cfg(test)]
#[path = "tests.rs"]
mod test_cases;
