pub(crate) fn sidecar_db_path(project_root: &Path) -> PathBuf {
    project_root.join(".brokkr").join("sidecar.db")
}

/// Path to the piners corpus runs database (gitignored, local run history).
/// Lives under the corpus artefact tree so `brokkr corpus` and `brokkr
/// results` (project-gated to piners) resolve it identically.
pub(crate) fn corpus_runs_db_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".brokkr")
        .join("piners")
        .join("corpus")
        .join("runs.db")
}

