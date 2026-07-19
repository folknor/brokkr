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

impl FileEntry for config::BlessedEntry {
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

    /// Parse an optional `--snapshot` CLI value into a `SnapshotRef`.
    /// `None` (flag omitted) → `Base` (the dataset's primary data, current
    /// behavior preserved). `Some(s)` parses `s` (`"base"` → `Base`).
    pub(crate) fn from_opt(snapshot: Option<&str>) -> Result<Self, DevError> {
        match snapshot {
            None => Ok(Self::Base),
            Some(s) => Self::parse(s),
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

/// Resolve the blessed reference archive for a dataset (for `regress`).
/// Looks up the singular `[<host>.datasets.<D>.blessed]` entry, joins its
/// `file` under the data dir, checks existence, and verifies the recorded
/// xxhash. Errors clearly when no blessed archive is registered.
pub(crate) fn resolve_blessed_path(
    dataset: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    let ds = get_dataset(dataset, paths)?;
    let blessed = ds.blessed.as_ref().ok_or_else(|| {
        DevError::Config(format!(
            "dataset '{dataset}' has no blessed archive registered on {}; \
             run `brokkr bless --dataset {dataset}` after a gate-passing build",
            paths.hostname
        ))
    })?;
    finalize_entry_path(
        blessed,
        &paths.data_dir,
        "blessed archive",
        ds.origin.as_deref(),
        project_root,
    )
}

/// Resolve a PMTiles file by --dataset/--commit/--file, per the
/// `<output>/<dataset>-<commit>.pmtiles` naming convention tilegen produces.
/// `<output>` is the durable output dir (never wiped by a run), NOT scratch.
/// `--file` skips resolution entirely. `--commit` defaults to the current
/// HEAD short hash. Only reads the file; does not rebuild for historical
/// commits (the current release binary can inspect a file built by any
/// commit).
pub(crate) fn resolve_pmtiles_by_commit(
    dataset: &str,
    commit: Option<&str>,
    file: Option<&str>,
    paths: &config::ResolvedPaths,
    build_root: &Path,
) -> Result<PathBuf, DevError> {
    if let Some(f) = file {
        let p = PathBuf::from(f);
        if !p.exists() {
            return Err(DevError::Config(format!("file not found: {}", p.display())));
        }
        return Ok(p);
    }
    let hash = match commit {
        Some(c) => c.to_owned(),
        None => crate::git::collect(build_root)?.commit,
    };
    let path = paths.output_dir.join(format!("{dataset}-{hash}.pmtiles"));
    if !path.exists() {
        return Err(DevError::Config(format!(
            "no build for {hash}; run brokkr tilegen first (looked for {})",
            path.display()
        )));
    }
    Ok(path)
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

