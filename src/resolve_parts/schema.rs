use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::DevError;
use crate::preflight::{self, FileDigest, PathDigest};

// ---------------------------------------------------------------------------
// FileEntry trait - unifies PBF, OSC, and PMTiles entry types
// ---------------------------------------------------------------------------

pub(crate) trait FileEntry {
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
    let entry = get_entry(entries, key, dataset, kind)?;
    finalize_entry_path(entry, data_dir, kind, origin, project_root)
}

fn get_entry<'a, E>(
    entries: &'a HashMap<String, E>,
    key: &str,
    dataset: &str,
    kind: &str,
) -> Result<&'a E, DevError> {
    entries.get(key).ok_or_else(|| {
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
        preflight::verify_file_digest(&path, expected, project_root, origin)?;
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
///
/// This resolved table is authoritative for both paths and entry metadata.
/// Callers that need a selected entry's flags should carry that entry from
/// here instead of looking it up again through the raw project config.
fn get_dataset<'a>(
    dataset: &str,
    paths: &'a config::ResolvedPaths,
) -> Result<&'a config::Dataset, DevError> {
    paths
        .datasets
        .get(dataset)
        .ok_or_else(|| DevError::Config(format!("unknown dataset: {dataset}")))
}

/// Whether a configured dataset path exists and passes its content pin.
/// Computed digests retain file-versus-tree shape, and configured pins are
/// compared as typed values rather than hexadecimal text.
pub enum DatasetStatus {
    /// The configured content pin matches.
    Verified,
    /// The input exists but has no configured pin; contains its digest.
    Present(PathDigest),
    /// The input exists but differs from its configured pin; contains the
    /// actual digest.
    HashMismatch(PathDigest),
    /// The configured input path is missing.
    Missing,
    /// The input exists but could not be hashed; contains a one-line reason.
    HashError(String),
    /// The dataset has no file entries or plain input path.
    NoFiles,
}

impl config::ResolvedPaths {
    /// Resolve and check every configured dataset entry for the environment
    /// report. Keeping this beside normal dataset resolution makes all callers
    /// share the same path and digest rules; `env` only formats the result.
    pub fn dataset_status(&self, project_root: &Path) -> Vec<(String, DatasetStatus)> {
        let mut out = Vec::new();

        for (name, dataset) in &self.datasets {
            check_file_entries(&mut out, name, &dataset.pbf, "", &self.data_dir, project_root);
            check_file_entries(
                &mut out,
                name,
                &dataset.osc,
                "osc.",
                &self.data_dir,
                project_root,
            );
            check_file_entries(
                &mut out,
                name,
                &dataset.pmtiles,
                "pmtiles.",
                &self.data_dir,
                project_root,
            );

            // Plain out-of-git inputs have one configured path and digest.
            // Reporting the computed digest lets the user register the pin.
            if let Some(path) = dataset.resolve_path(project_root) {
                let status = if path.exists() {
                    check_hash_status(&path, dataset.xxh128, project_root)
                } else {
                    DatasetStatus::Missing
                };
                out.push((name.clone(), status));
                continue;
            }

            if dataset.pbf.is_empty() && dataset.osc.is_empty() && dataset.pmtiles.is_empty() {
                out.push((name.clone(), DatasetStatus::NoFiles));
            }
        }

        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

fn check_file_entries<E: FileEntry>(
    out: &mut Vec<(String, DatasetStatus)>,
    dataset_name: &str,
    entries: &HashMap<String, E>,
    prefix: &str,
    data_dir: &Path,
    project_root: &Path,
) {
    for (key, entry) in entries {
        let label = format!("{dataset_name}/{prefix}{key}");
        let path = data_dir.join(entry.file());
        let status = if !path.exists() {
            DatasetStatus::Missing
        } else {
            check_hash_status(
                &path,
                entry.xxhash().map(FileDigest::value),
                project_root,
            )
        };
        out.push((label, status));
    }
}

fn check_hash_status(
    path: &Path,
    expected: Option<crate::preflight::Xxh128>,
    project_root: &Path,
) -> DatasetStatus {
    let digest = match preflight::cached_path_digest(path, project_root) {
        Ok(digest) => digest,
        Err(error) => return DatasetStatus::HashError(first_line(&error.to_string())),
    };

    match expected {
        None => DatasetStatus::Present(digest),
        Some(pin) if digest.value() == pin => DatasetStatus::Verified,
        Some(_) => DatasetStatus::HashMismatch(digest),
    }
}

fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).trim().to_owned()
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
/// `Base` selects the top-level `pbf.<variant>` entry. `Named(key)` selects
/// `dataset.snapshot.<key>.pbf.<variant>`.
pub(crate) fn resolve_snapshot_pbf_path(
    dataset: &str,
    snapshot: &SnapshotRef,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
    Ok(resolve_snapshot_pbf_entry(dataset, snapshot, variant, paths, project_root)?.path)
}

/// Resolve a snapshot PBF and retain the exact entry that supplied its path.
pub(crate) fn resolve_snapshot_pbf_entry<'a>(
    dataset: &str,
    snapshot: &SnapshotRef,
    variant: &str,
    paths: &'a config::ResolvedPaths,
    project_root: &Path,
) -> Result<ResolvedPbf<'a>, DevError> {
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
                DevError::Config(format!(
                    "dataset '{dataset}' has no snapshot '{key}' (available: {})",
                    available.join(", ")
                ))
            })?;
            // Custom missing-variant error: name the available variants and
            // suggest both a one-shot workaround (--variant <X>) and the
            // proper fix (re-download to auto-generate). This closes a
            // first-time-user papercut inline instead of adding the per-side --variant-from / --variant-to
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
    Ok(ResolvedPbf {
        path,
        entry,
    })
}

/// Resolve the selected PBF entry from the host's resolved dataset table.
pub(crate) fn resolve_pbf_entry<'a>(
    dataset: &str,
    variant: &str,
    paths: &'a config::ResolvedPaths,
    project_root: &Path,
) -> Result<ResolvedPbf<'a>, DevError> {
    resolve_snapshot_pbf_entry(dataset, &SnapshotRef::Base, variant, paths, project_root)
}

/// Resolve the PBF path from --dataset + --variant.
pub(crate) fn resolve_pbf_path(
    dataset: &str,
    variant: &str,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<PathBuf, DevError> {
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
/// seq order. The CLI passes the range after its shared typed parse.
pub(crate) fn resolve_osc_range(
    dataset: &str,
    snapshot: &SnapshotRef,
    range: &config::OscRange,
    paths: &config::ResolvedPaths,
    project_root: &Path,
) -> Result<Vec<PathBuf>, DevError> {
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
#[cfg(test)]
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

/// Resolve the already-parsed bbox from --bbox or dataset config.
pub(crate) fn resolve_bbox(
    bbox: Option<&Bbox>,
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<Bbox, DevError> {
    if let Some(bbox) = bbox {
        return Ok(bbox.clone());
    }
    let ds = paths
        .datasets
        .get(dataset)
        .ok_or_else(|| DevError::Config(format!("unknown dataset: {dataset}")))?;
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

/// Resolve the nidhogg dataset data directory (required, and must exist).
pub(crate) fn resolve_nidhogg_data_dir(
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, DevError> {
    let path = nidhogg_data_dir_path(dataset, paths)?;
    if !path.exists() {
        return Err(DevError::Config(format!(
            "data directory not found: {}",
            path.display()
        )));
    }
    Ok(path)
}

/// Where the nidhogg dataset data directory lives, without requiring it to
/// exist. `ingest` is the one caller that creates the directory, so it must
/// not be gated on its existence - a first ingest would be impossible.
pub(crate) fn nidhogg_data_dir_path(
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
    Ok(paths.data_dir.join(dir_name))
}

/// Path to the results database for the current project.
pub(crate) fn results_db_path(project_root: &Path) -> PathBuf {
    crate::clean::layout::ProjectState::new(project_root).results_db()
}

