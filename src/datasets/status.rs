//! Non-failing dataset inventory and content-pin status for environment reports.

use super::resolve::FileEntry;
use crate::config;
use crate::preflight::{self, FileDigest, PathDigest};
use std::collections::HashMap;
use std::path::Path;

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
            check_file_entries(
                &mut out,
                name,
                &dataset.pbf,
                "",
                &self.data_dir,
                project_root,
            );
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
            check_hash_status(&path, entry.xxhash().map(FileDigest::value), project_root)
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
