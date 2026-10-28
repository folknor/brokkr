//! Durable rotation of sidecar database backups.

use crate::error::DevError;
use std::path::{Path, PathBuf};

/// Number of rotating backup copies to keep.
const SIDECAR_BACKUP_COPIES: usize =
    crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.sidecar_backup_copies;

/// Resolve the sidecar backup directory.
///
/// Uses `$XDG_DATA_HOME/brokkr/sidecar-backups/`, falling back to
/// `$HOME/.local/share/brokkr/sidecar-backups/`. An empty or relative
/// `XDG_DATA_HOME` counts as unset (`user_dirs`), so the backups can never
/// land in - and dirty - the tree brokkr is running in.
fn sidecar_backup_dir() -> Result<PathBuf, DevError> {
    crate::user_dirs::sidecar_backups_dir().ok_or_else(|| {
        DevError::Config("cannot determine data directory for sidecar backup".into())
    })
}

/// Back up the sidecar DB with rotation and fsync.
///
/// Keeps `SIDECAR_BACKUP_COPIES` versions:
///   {project}-sidecar.db      (newest)
///   {project}-sidecar.db.1    (previous)
///   {project}-sidecar.db.2    (oldest)
///
/// Uses SQLite's online backup API to produce a self-contained backup
/// (DELETE journal mode, no WAL side files). The backup captures the
/// logical database state regardless of WAL or concurrent readers.
///
/// The sequence is: create temp backup via SQLite -> quick_check the
/// temp -> fsync -> shift older copies -> hard-link primary to .1 ->
/// atomic rename temp into primary slot -> fsync directory. The primary
/// slot is only overwritten by the atomic rename, so a failure before
/// that point leaves the current primary intact.
///
/// Called while the benchmark lock is still held.
pub(crate) fn backup_sidecar(
    sidecar_path: &Path,
    project: &crate::project::Project,
) -> Result<(), DevError> {
    backup_sidecar_to(sidecar_path, project, None)
}

/// Inner implementation that accepts an optional backup directory override
/// (used by tests to avoid mutating global XDG_DATA_HOME).
pub(crate) fn backup_sidecar_to(
    sidecar_path: &Path,
    project: &crate::project::Project,
    backup_dir_override: Option<&Path>,
) -> Result<(), DevError> {
    if !sidecar_path.exists() {
        return Ok(());
    }

    let backup_dir = match backup_dir_override {
        Some(d) => d.to_path_buf(),
        None => sidecar_backup_dir()?,
    };
    std::fs::create_dir_all(&backup_dir)?;

    let base = backup_dir.join(format!("{}-sidecar.db", project.name()));
    // The new backup is staged beside `base` and promoted by
    // `atomic_write::Staged::commit` (fsync, rename, dir fsync). Dropping
    // `staged` on any early error removes the temp file.
    let staged = crate::atomic_write::Staged::stable(&base)?;

    // Clean up any stale temp from a previous interrupted run: the backup
    // API writes into an existing file rather than replacing it.
    if staged.tmp_path().exists() {
        std::fs::remove_file(staged.tmp_path()).ok();
    }

    // Create backup via SQLite backup API. This reads the logical DB state
    // (including uncommitted WAL pages from other connections) and writes a
    // self-contained DELETE-journal-mode database at the temp path. The
    // backup API also runs quick_check on the result.
    crate::db::sidecar::backup_to_path(sidecar_path, staged.tmp_path())?;

    // Promote the new backup into the primary slot without displacing the
    // current primary until the new one is in place.
    //
    // Sequence:
    //   1. Shift older copies: .1 -> .2 (clears .1 slot, drops oldest)
    //   2. Preserve current primary: hard-link base -> .1
    //   3. Atomic promote: commit the staged file -> base (overwrites old base)
    //
    // Every step propagates errors. If any rotation or preservation step
    // fails, the backup is considered failed rather than silently losing
    // retention history.

    // Shift older copies: .1 -> .2, .2 -> .3, etc.
    // This clears the .1 slot so the hard-link in the next step can
    // succeed without a prior remove. These renames *move* finished copies
    // between retention slots - not a temp-and-replace - so they stay raw;
    // the final commit's directory fsync makes them durable.
    for i in (2..SIDECAR_BACKUP_COPIES).rev() {
        let from = base.with_extension(format!("db.{}", i - 1));
        let to = base.with_extension(format!("db.{i}"));
        if from.exists() {
            std::fs::rename(&from, &to)?;
        }
    }

    // Preserve the current primary as .1 via hard-link.
    // The .1 slot was cleared by the rename above (or never existed).
    if base.exists() {
        let slot1 = base.with_extension("db.1");
        std::fs::hard_link(&base, &slot1)?;
    }

    // Atomic promotion: the staged file replaces base. If this fails, base
    // is still the old copy (the hard-link in step 2 created .1 as a second
    // link to the same inode, so the data is preserved regardless).
    staged.commit()?;

    crate::output::sidecar_msg(&format!("backup: {}", crate::output::path(&base)));
    Ok(())
}
