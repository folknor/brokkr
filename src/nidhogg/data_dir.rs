//! Dataset data-directory selection for ingest and server commands.

use crate::config;
use crate::datasets::error::ResolveError;
use std::path::PathBuf;

/// Resolve the nidhogg dataset data directory (required, and must exist).
pub(crate) fn resolve_nidhogg_data_dir(
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, ResolveError> {
    let path = nidhogg_data_dir_path(dataset, paths)?;
    if !path.exists() {
        return Err(ResolveError::MissingDirectory(path));
    }
    Ok(path)
}

/// Where the nidhogg dataset data directory lives, without requiring it to
/// exist. `ingest` is the one caller that creates the directory, so it must
/// not be gated on its existence - a first ingest would be impossible.
pub(crate) fn nidhogg_data_dir_path(
    dataset: &str,
    paths: &config::ResolvedPaths,
) -> Result<PathBuf, ResolveError> {
    let ds = paths
        .datasets
        .get(dataset)
        .ok_or_else(|| ResolveError::UnknownDataset(dataset.to_owned()))?;
    let dir_name = ds
        .data_dir
        .as_ref()
        .ok_or_else(|| ResolveError::MissingDataDir(dataset.to_owned()))?;
    Ok(paths.data_dir.join(dir_name))
}
