//! Stable names and paths of downloaded and promoted inputs.

use crate::config::Dataset;
use std::path::{Path, PathBuf};

pub(super) fn raw_pbf_filename(dataset_key: &str, date_or_snapshot: &str) -> String {
    format!("{dataset_key}-{date_or_snapshot}.osm.pbf")
}

pub(super) fn indexed_pbf_filename(dataset_key: &str, date_or_snapshot: &str) -> String {
    format!("{dataset_key}-{date_or_snapshot}-with-indexdata.osm.pbf")
}

pub(super) fn snapshot_pbf_filename(dataset_key: &str, snap_key: &str, variant: &str) -> String {
    if variant == "indexed" {
        indexed_pbf_filename(dataset_key, snap_key)
    } else {
        raw_pbf_filename(dataset_key, snap_key)
    }
}

/// Format a 9-digit zero-padded sequence number as a 3-level path: `000/004/715`.
pub(super) fn seq_path(seq: u64) -> String {
    let padded = format!("{seq:09}");
    let (a, rest) = padded.split_at(3);
    let (b, c) = rest.split_at(3);
    format!("{a}/{b}/{c}")
}

pub(super) fn has_existing_pbf(dataset: Option<&Dataset>, data_dir: &Path) -> Option<PathBuf> {
    let ds = dataset?;
    // Check raw first.
    if let Some(entry) = ds.pbf.get("raw") {
        let path = data_dir.join(&entry.file);
        if is_nonempty(&path) {
            return Some(path);
        }
    }
    // Fall back to any variant.
    for entry in ds.pbf.values() {
        let path = data_dir.join(&entry.file);
        if is_nonempty(&path) {
            return Some(path);
        }
    }
    None
}

/// Find the highest configured OSC sequence number in the dataset.
pub(super) fn max_osc_seq(dataset: Option<&Dataset>) -> Option<u64> {
    let ds = dataset?;
    ds.osc.keys().filter_map(|k| k.parse::<u64>().ok()).max()
}

/// Check whether a configured OSC file for the given seq already exists.
pub(super) fn has_existing_osc(
    dataset: Option<&Dataset>,
    data_dir: &Path,
    seq: u64,
) -> Option<PathBuf> {
    let ds = dataset?;
    let key = seq.to_string();
    let entry = ds.osc.get(&key)?;
    let path = data_dir.join(&entry.file);
    if is_nonempty(&path) { Some(path) } else { None }
}

/// Find the "raw" PBF path - either the configured `raw` variant, or a
/// dated filename as fallback.
pub(super) fn raw_pbf_path(
    dataset: Option<&Dataset>,
    data_dir: &Path,
    dataset_key: &str,
    date: &str,
) -> PathBuf {
    if let Some(ds) = dataset
        && let Some(entry) = ds.pbf.get("raw")
    {
        return data_dir.join(&entry.file);
    }
    data_dir.join(raw_pbf_filename(dataset_key, date))
}

/// Find the "indexed" PBF path - either the configured `indexed` variant, or
/// a dated filename as fallback.
pub(super) fn indexed_pbf_path(
    dataset: Option<&Dataset>,
    data_dir: &Path,
    dataset_key: &str,
    date: &str,
) -> PathBuf {
    if let Some(ds) = dataset
        && let Some(entry) = ds.pbf.get("indexed")
    {
        return data_dir.join(&entry.file);
    }
    data_dir.join(indexed_pbf_filename(dataset_key, date))
}

/// Build the OSC destination filename: dataset and replication sequence only.
///
/// No date: the only one at hand is the download day (or, for a snapshot
/// download, the snapshot key), neither of which says what the diff holds,
/// and a dated name invites applying it to the base of that date. The
/// sequence alone identifies the file within one dataset's replication
/// stream, so the primary chain and every snapshot share one copy of it.
pub(super) fn osc_filename(dataset_key: &str, seq: u64) -> String {
    format!("{dataset_key}-seq{seq}.osc.gz")
}

/// Check that a file exists and is not empty (guards against partial downloads).
pub(super) fn is_nonempty(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 0)
}
