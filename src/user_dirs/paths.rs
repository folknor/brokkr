use std::path::PathBuf;

#[allow(dead_code)]
mod protocol {
    include!("../lock_protocol.rs");
}

/// `$HOME`, if set to something non-empty.
pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os(protocol::HOME_ENV)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The per-user directory for brokkr state shared across projects.
pub(crate) fn brokkr_state_dir() -> Option<PathBuf> {
    brokkr_state_dir_at(home().as_deref())
}

/// Resolve the state directory from an explicit home, for callers that need
/// deterministic paths without reading the process environment.
pub(crate) fn brokkr_state_dir_at(home: Option<&std::path::Path>) -> Option<PathBuf> {
    home.map(|home| home.join(protocol::STATE_DIRECTORY))
}

/// The global brokkr lock file.
pub(crate) fn lock_file() -> Option<PathBuf> {
    lock_file_at(home().as_deref())
}

pub(crate) fn lock_file_at(home: Option<&std::path::Path>) -> Option<PathBuf> {
    brokkr_state_dir_at(home).map(|dir| dir.join(protocol::LOCK_FILE))
}

/// The lock shared by rustc compilers and drained exclusively by a fresh hold.
pub(crate) fn compile_lock() -> Option<PathBuf> {
    compile_lock_at(home().as_deref())
}

pub(crate) fn compile_lock_at(home: Option<&std::path::Path>) -> Option<PathBuf> {
    brokkr_state_dir_at(home).map(|dir| dir.join(protocol::COMPILE_LOCK_FILE))
}
