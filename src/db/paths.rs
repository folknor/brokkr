//! Locations of the measurement databases.

use std::path::{Path, PathBuf};

/// Path to the results database for the current project.
pub(crate) fn results_db_path(project_root: &Path) -> PathBuf {
    crate::clean::layout::ProjectState::new(project_root).results_db()
}

pub(crate) fn sidecar_db_path(project_root: &Path) -> PathBuf {
    crate::clean::layout::ProjectState::new(project_root).sidecar_db()
}
