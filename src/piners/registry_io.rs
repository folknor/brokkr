//! Crash-safe writes for the piners registries (`pins.toml`, `lints.toml`).
//!
//! Every registry writer (`corpus --reseed`/`--bless`, `lint-corpus
//! --reseed`/`--bless`/`--reanchor`) rewrites a hand-curated, git-tracked file
//! whose comments are part of the review surface, so every write goes through
//! [`write_atomic`] (see `crate::atomic_write`): a reader sees either the old
//! bytes or the new ones, never a truncated prefix.
//!
//! [`lock`] is the other half: every writer takes the global brokkr lock
//! before it reads the file it is about to rewrite, so two writers can never
//! interleave a read-modify-write (a reseed landing between a bless's read and
//! its write would otherwise be silently reverted).

use std::path::Path;

use crate::error::DevError;
use crate::lockfile::{self, LockContext, LockGuard};

/// Atomically replace a registry file with `contents`.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), DevError> {
    crate::atomic_write::replace(path, contents.as_bytes()).map_err(DevError::Io)
}

/// Take the global brokkr lock for a registry writer that does not otherwise
/// hold it (the two `--reseed` paths; the run-then-write paths already hold it
/// for the run).
pub fn lock(project_root: &Path, command: &'static str) -> Result<LockGuard, DevError> {
    let project_root_str = project_root.display().to_string();
    lockfile::acquire(&LockContext {
        project: "piners",
        command,
        project_root: &project_root_str,
    })
}
