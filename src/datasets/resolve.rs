//! Resolve registered dataset entries and verify their input files.

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

impl FileEntry for config::PolygonEntry {
    fn file(&self) -> &str {
        &self.file
    }
    fn xxhash(&self) -> Option<&FileDigest> {
        self.xxhash.as_ref()
    }
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

/// Build the on-disk path, check existence,
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

/// A reference to a registered dataset snapshot.
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
#[derive(Debug)]
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

/// Get file size in MB (decimal, consistent with bench scripts).
pub(crate) fn file_size_mb(path: &Path) -> Result<f64, ResolveError> {
    let meta = std::fs::metadata(path)?;
    Ok(meta.len() as f64 / 1_000_000.0)
}

#[cfg(test)]
#[path = "tests.rs"]
mod test_cases;
