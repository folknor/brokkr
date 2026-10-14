pub(crate) fn sidecar_db_path(project_root: &Path) -> PathBuf {
    crate::clean::layout::ProjectState::new(project_root).sidecar_db()
}

/// Path to the piners corpus runs database (gitignored, local run history).
/// Lives under the corpus artefact tree so `brokkr corpus` and `brokkr
/// corpus-results` (piners only) resolve it identically.
pub(crate) fn corpus_runs_db_path(project_root: &Path) -> PathBuf {
    crate::clean::layout::ProjectState::new(project_root)
        .piners()
        .corpus_runs_db()
}

/// Path to the piners *lint* corpus runs database (gitignored, local run
/// history). Sibling of [`corpus_runs_db_path`] under the lint artefact tree
/// so `brokkr lint-corpus` and `brokkr lint-results` resolve it identically.
pub(crate) fn lint_runs_db_path(project_root: &Path) -> PathBuf {
    crate::clean::layout::ProjectState::new(project_root)
        .piners()
        .lint_runs_db()
}

