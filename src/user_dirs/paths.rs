use std::path::PathBuf;

/// `$HOME`, if set to something non-empty.
pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The per-user directory for brokkr state shared across projects.
pub(crate) fn brokkr_state_dir() -> Option<PathBuf> {
    home().map(|home| home.join(".brokkr"))
}

/// The global brokkr lock file.
pub(crate) fn lock_file() -> Option<PathBuf> {
    brokkr_state_dir().map(|dir| dir.join("brokkr.lock"))
}

/// The lock shared by rustc compilers and drained exclusively by a fresh hold.
pub(crate) fn compile_lock() -> Option<PathBuf> {
    brokkr_state_dir().map(|dir| dir.join("compile.lock"))
}
