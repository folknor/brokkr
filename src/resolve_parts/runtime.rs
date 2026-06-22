pub(crate) fn sidecar_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("sidecar.db")
}

/// Path to the piners corpus runs database (gitignored, local run history).
/// Lives under the corpus artefact tree so `brokkr corpus` and `brokkr
/// corpus-results` (piners only) resolve it identically.
pub(crate) fn corpus_runs_db_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".brokkr")
        .join("piners")
        .join("corpus")
        .join("runs.db")
}

/// Path to the piners *lint* corpus runs database (gitignored, local run
/// history). Sibling of [`corpus_runs_db_path`] under the lint artefact tree
/// so `brokkr lint-corpus` and `brokkr lint-results` resolve it identically.
pub(crate) fn lint_runs_db_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".brokkr")
        .join("piners")
        .join("lint")
        .join("runs.db")
}

