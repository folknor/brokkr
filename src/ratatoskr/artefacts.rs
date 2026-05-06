//! Per-test artefact directories for the service-test harness.
//!
//! Each script run gets `<project_root>/.brokkr/ratatoskr/<test>/run-N/`,
//! where N is the smallest positive integer such that `run-N/` does not
//! already exist. Callers populate the directory while the run executes
//! (frame log, event log, /proc snapshots, data-dir copies); when the
//! run finishes they call [`ArtefactDir::finalize_success`] or
//! [`ArtefactDir::finalize_failure`] to drive the retention policy:
//!
//! - **Failure** -> directory preserved unconditionally.
//! - **Success** -> directory removed unless the caller asked to keep
//!   artefacts (e.g. via the CLI `--keep-artefacts` flag).
//!
//! If neither finalizer is called (panic, early return) the `Drop` impl
//! preserves the directory: losing diagnostics by default is the worst
//! possible failure mode for a test harness, so the safe behaviour is
//! "keep what we have."
//!
//! The helper lives under `src/ratatoskr/` for now because it has no
//! other callers; the API takes a generic parent path so it lifts to a
//! shared location unchanged the day a second project wants the same
//! shape.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::DevError;

/// A freshly-allocated `<parent>/<test_id>/run-N/` directory.
///
/// `parent` is supplied by the caller. The harness convention is
/// `<project_root>/.brokkr/ratatoskr`, but nothing in this module
/// depends on that.
#[allow(dead_code)] // wired in once the harness invokes it from cmd.rs
#[derive(Debug)]
pub struct ArtefactDir {
    path: PathBuf,
    keep_on_success: bool,
    /// `true` means no finalizer has run yet. Drop preserves in that
    /// case; `finalize_success` / `finalize_failure` clear it.
    armed: bool,
}

#[allow(dead_code)]
impl ArtefactDir {
    /// Allocate `<parent>/<test_id>/run-N/`.
    ///
    /// `<parent>/<test_id>/` is created on demand. N is the smallest
    /// positive integer for which `run-N/` does not yet exist; gaps in
    /// the existing numbering are NOT filled (so chronological order is
    /// preserved when listing).
    ///
    /// `test_id` is validated to be a single path component (no `/`,
    /// `\`, `..`, leading `.`, or empty). Anything else is rejected as
    /// a config error rather than silently mangled - the test_id is
    /// almost always derived from a script filename, and a malformed
    /// one points at a real bug upstream.
    pub fn allocate(
        parent: &Path,
        test_id: &str,
        keep_on_success: bool,
    ) -> Result<Self, DevError> {
        validate_test_id(test_id)?;

        let test_dir = parent.join(test_id);
        fs::create_dir_all(&test_dir)?;

        let n = next_run_number(&test_dir)?;
        let run_dir = test_dir.join(format!("run-{n}"));
        fs::create_dir(&run_dir)?;

        Ok(Self {
            path: run_dir,
            keep_on_success,
            armed: true,
        })
    }

    /// The directory the caller should write artefacts into.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mark the run as successful.
    ///
    /// Removes the directory unless the caller asked to keep artefacts
    /// at allocation time. Returns any I/O error from the removal so
    /// the caller can surface it; the directory state is "armed=false"
    /// regardless of whether removal succeeded.
    pub fn finalize_success(mut self) -> Result<(), DevError> {
        self.armed = false;
        if self.keep_on_success {
            return Ok(());
        }
        match fs::remove_dir_all(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(DevError::Io(err)),
        }
    }

    /// Mark the run as failed. The directory is preserved unconditionally.
    pub fn finalize_failure(mut self) {
        self.armed = false;
    }
}

impl Drop for ArtefactDir {
    fn drop(&mut self) {
        // No finalizer ran (panic, early return, ?) - preserve. Losing
        // diagnostics by default is the worst possible behaviour for a
        // test-harness artefact dir.
    }
}

fn validate_test_id(test_id: &str) -> Result<(), DevError> {
    if test_id.is_empty() {
        return Err(DevError::Config("artefact test_id is empty".into()));
    }
    if test_id == "." || test_id == ".." {
        return Err(DevError::Config(format!(
            "artefact test_id must not be '.' or '..' (got {test_id:?})"
        )));
    }
    if test_id.starts_with('.') {
        return Err(DevError::Config(format!(
            "artefact test_id must not start with '.' (got {test_id:?})"
        )));
    }
    if test_id.contains('/')
        || test_id.contains('\\')
        || test_id.contains('\0')
    {
        return Err(DevError::Config(format!(
            "artefact test_id must be a single path component, no separators (got {test_id:?})"
        )));
    }
    Ok(())
}

fn next_run_number(test_dir: &Path) -> Result<u32, DevError> {
    let mut highest: u32 = 0;
    let read = match fs::read_dir(test_dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(1),
        Err(err) => return Err(DevError::Io(err)),
    };
    for entry in read {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix("run-") else {
            continue;
        };
        if let Ok(n) = rest.parse::<u32>()
            && n > highest
        {
            highest = n;
        }
    }
    Ok(highest + 1)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::fs;

    use super::*;

    fn tmpdir(test_name: &str) -> PathBuf {
        // Per project rules: no /tmp. CARGO_TARGET_TMPDIR is only set
        // for integration tests; unit tests inside `src/` use a fixed
        // path under the crate's `target/` (which is gitignored).
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-tmp/artefacts")
            .join(test_name);
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn allocate_first_run_is_run_1() {
        let parent = tmpdir("artefacts_first");
        let dir = ArtefactDir::allocate(&parent, "test_alpha", false).unwrap();
        assert_eq!(dir.path().file_name().unwrap(), "run-1");
        assert!(dir.path().is_dir());
        dir.finalize_failure();
    }

    #[test]
    fn allocate_increments_past_existing() {
        let parent = tmpdir("artefacts_increment");
        let test_dir = parent.join("test_beta");
        fs::create_dir_all(&test_dir).unwrap();
        fs::create_dir(test_dir.join("run-1")).unwrap();
        fs::create_dir(test_dir.join("run-3")).unwrap();
        // gap at run-2 is intentional - chronological ordering.
        let dir = ArtefactDir::allocate(&parent, "test_beta", false).unwrap();
        assert_eq!(dir.path().file_name().unwrap(), "run-4");
        dir.finalize_failure();
    }

    #[test]
    fn finalize_success_deletes_when_not_keep() {
        let parent = tmpdir("artefacts_success_delete");
        let dir = ArtefactDir::allocate(&parent, "test_gamma", false).unwrap();
        let path = dir.path().to_owned();
        fs::write(path.join("frames.jsonl"), "stuff").unwrap();
        dir.finalize_success().unwrap();
        assert!(!path.exists(), "{path:?} should have been removed");
    }

    #[test]
    fn finalize_success_preserves_when_keep() {
        let parent = tmpdir("artefacts_success_keep");
        let dir = ArtefactDir::allocate(&parent, "test_delta", true).unwrap();
        let path = dir.path().to_owned();
        fs::write(path.join("frames.jsonl"), "stuff").unwrap();
        dir.finalize_success().unwrap();
        assert!(path.exists());
        assert!(path.join("frames.jsonl").exists());
    }

    #[test]
    fn finalize_failure_always_preserves() {
        let parent = tmpdir("artefacts_failure");
        let dir = ArtefactDir::allocate(&parent, "test_epsilon", false).unwrap();
        let path = dir.path().to_owned();
        fs::write(path.join("evidence.txt"), "stuff").unwrap();
        dir.finalize_failure();
        assert!(path.exists());
    }

    #[test]
    fn drop_without_finalize_preserves() {
        let parent = tmpdir("artefacts_drop");
        let path = {
            let dir = ArtefactDir::allocate(&parent, "test_zeta", false).unwrap();
            dir.path().to_owned()
        };
        // Dropped without a finalize call - safer to keep diagnostics.
        assert!(path.exists());
    }

    #[test]
    fn rejects_separators_and_traversal() {
        let parent = tmpdir("artefacts_validate");
        for bad in ["", ".", "..", ".hidden", "with/slash", "with\\back", "with\0null"] {
            let result = ArtefactDir::allocate(&parent, bad, false);
            assert!(
                matches!(result, Err(DevError::Config(_))),
                "expected Config error for {bad:?}, got {result:?}"
            );
        }
    }
}
