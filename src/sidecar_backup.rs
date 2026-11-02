//! Crash-safe rotation of sidecar database backups, one rotation per project
//! root.
//!
//! ## Layout
//!
//! ```text
//! $XDG_DATA_HOME/brokkr/sidecar-backups/<root key>/
//!     origin            the project root's canonical path, one line
//!     sidecar.db        newest
//!     sidecar.db.1      previous
//!     sidecar.db.2      oldest
//! ```
//!
//! **The root key** is the canonical project root's directory name, cut to
//! [`LABEL_MAX`] bytes, then `-` and the full 64-bit xxh3 of the canonical
//! path's raw bytes. The backed-up store is the project root's own
//! `.brokkr/sidecar.db`, so the project root is its identity: two checkouts
//! of one project (`~/Programs/pbfhogg` and a copy under `PRs/`) keep separate
//! rotations instead of evicting each other's, and under the
//! config-one-level-up layout every checkout below one config shares the one
//! store and so the one rotation. The name is only a label; the hash and the
//! origin file use the path's bytes, never a lossy rendering. A moved or
//! renamed checkout starts a new rotation and its old one stays, named by its
//! origin file.
//!
//! This is deliberately not `worktree.rs`'s checkout key, which names the
//! worktree container: that one hashes the git root to 32 bits and takes the
//! name from the path as spelled, and changing it would re-key every existing
//! worktree slot.
//!
//! **The origin file is an ownership assertion**, checked before anything in
//! the directory is written. Every slot and the origin must be absent or a
//! regular file (`symlink_metadata`; only a missing entry counts as absent,
//! and any other inspection failure refuses). An origin must hold exactly
//! this root's canonical bytes, or the backup is refused: two roots whose keys
//! collide never rotate each other's copies. An absent origin is claimed only
//! in a directory that holds no slot; a directory with slots and no origin is
//! refused, since nothing proves whose copies they are. The origin is written
//! before the first primary, in the same atomic tier, so a crash that keeps a
//! published primary but loses the origin's directory entry leaves exactly
//! that refused shape: restoring the origin file, or moving the directory
//! aside, is the way out, and no copy is lost by the refusal.
//!
//! **An earlier brokkr** keyed the rotation by project kind
//! (`<kind>-sidecar.db`, `.1`, `.2` directly under `sidecar-backups/`). Those
//! copies are left alone: a finished backup may be the only copy of a
//! checkout that no longer exists, and nothing cheap proves one redundant.
//! Its unfinished staging file, `.<kind>-sidecar.db.partial`, which a killed
//! backup leaves behind at the size of a whole database, is removed by exact
//! name on every backup of that kind.

use crate::error::DevError;
use std::path::{Path, PathBuf};

/// Number of rotating backup copies to keep.
const SIDECAR_BACKUP_COPIES: usize =
    crate::sidecar::DEFAULT_MEASUREMENT_TUNABLES.sidecar_backup_copies;

/// The primary slot's name inside a root's directory.
const PRIMARY: &str = "sidecar.db";

/// The ownership file inside a root's directory.
const ORIGIN: &str = "origin";

/// The most bytes of the root's directory name the key carries, so the key
/// stays well inside a filename's length limit.
const LABEL_MAX: usize = 64;

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

/// Back up the project's sidecar DB into its root's rotation, with fsync.
///
/// Keeps `SIDECAR_BACKUP_COPIES` versions in the root's directory (module
/// doc): `sidecar.db` (newest), `sidecar.db.1`, `sidecar.db.2` (oldest).
///
/// Uses SQLite's online backup API to produce a self-contained backup
/// (DELETE journal mode, no WAL side files). The backup captures the
/// logical database state regardless of WAL or concurrent readers.
///
/// The sequence is: admit the directory (module doc) -> create temp backup
/// via SQLite -> quick_check the temp -> shift older copies -> hard-link
/// primary to .1 -> fsync the temp, atomically rename it into the primary
/// slot and fsync the directory (`atomic_write::Staged::commit`). The primary
/// slot is only overwritten by the atomic rename, so a failure before that
/// point leaves the current primary intact, and the temp is in no slot until
/// it has been fsynced. The rotation is not one transaction: a crash
/// mid-rotation can lose a retention slot, never leave a partial primary.
/// The directory fsync is the atomic tier's, best effort and warned about
/// when it fails.
///
/// Called while the benchmark lock is still held, which serialises every
/// backup of every checkout.
pub(crate) fn backup_sidecar(
    state: &crate::clean::layout::ProjectState,
    project: &crate::project::Project,
) -> Result<(), DevError> {
    backup_sidecar_to(&state.sidecar_db(), state.project_root(), project, None)
}

/// Inner implementation that accepts an optional backup directory override
/// (used by tests to avoid mutating global XDG_DATA_HOME).
pub(crate) fn backup_sidecar_to(
    sidecar_path: &Path,
    project_root: &Path,
    project: &crate::project::Project,
    backup_dir_override: Option<&Path>,
) -> Result<(), DevError> {
    let backups = match backup_dir_override {
        Some(d) => d.to_path_buf(),
        None => sidecar_backup_dir()?,
    };

    // Unfinished writer state is worth nothing whatever this backup comes
    // to, so it goes before anything can refuse.
    remove_legacy_partial(&backups, project);

    if !sidecar_path.exists() {
        return Ok(());
    }

    // Identity is the canonical root; a root that cannot be resolved has no
    // identity to file under, and a fallback spelling would be another one.
    let canonical = std::fs::canonicalize(project_root).map_err(|e| {
        DevError::Config(format!(
            "cannot resolve the project root {} for the sidecar backup: {e}",
            crate::output::path(project_root)
        ))
    })?;
    let dir = backups.join(root_key(&canonical));
    std::fs::create_dir_all(&dir)?;
    admit(&dir, &canonical)?;

    let base = dir.join(PRIMARY);
    // The new backup is staged beside `base` and promoted by
    // `atomic_write::Staged::commit` (fsync, rename, dir fsync). Dropping
    // `staged` on any early error removes the temp file. Admission refused
    // a symlinked primary, so the staged target is `base` itself.
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
    // the final commit's best-effort directory fsync covers them too.
    for i in (2..SIDECAR_BACKUP_COPIES).rev() {
        let from = slot(&dir, i - 1);
        let to = slot(&dir, i);
        if from.exists() {
            std::fs::rename(&from, &to)?;
        }
    }

    // Preserve the current primary as .1 via hard-link.
    // The .1 slot was cleared by the rename above (or never existed).
    if base.exists() {
        std::fs::hard_link(&base, slot(&dir, 1))?;
    }

    // Atomic promotion: the staged file replaces base. If this fails, base
    // is still the old copy (the hard-link in step 2 created .1 as a second
    // link to the same inode, so the data is preserved regardless).
    staged.commit()?;

    crate::output::sidecar_msg(&format!("backup: {}", crate::output::path(&base)));
    Ok(())
}

/// Retention slot `index` of a root's directory: 0 is the primary.
fn slot(dir: &Path, index: usize) -> PathBuf {
    if index == 0 {
        dir.join(PRIMARY)
    } else {
        dir.join(format!("{PRIMARY}.{index}"))
    }
}

/// `<directory name, at most LABEL_MAX bytes>-<xxh3_64 of the path's bytes>`.
fn root_key(canonical: &Path) -> String {
    let name = canonical
        .file_name()
        .map_or_else(|| "root".to_owned(), |n| n.to_string_lossy().into_owned());
    let mut end = name.len().min(LABEL_MAX);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let label = name.get(..end).unwrap_or_default();
    let hash = xxhash_rust::xxh3::xxh3_64(canonical.as_os_str().as_encoded_bytes());
    format!("{label}-{hash:016x}")
}

/// What one admitted entry of a root's directory is.
#[derive(PartialEq, Eq)]
enum Entry {
    Absent,
    File,
}

/// An entry is absent only when there is no directory entry at all; one
/// that is there must be a regular file, never followed through a link.
fn entry(path: &Path) -> Result<Entry, DevError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(Entry::File),
        Ok(_) => Err(DevError::Refused(format!(
            "refused: {} is not a regular file",
            crate::output::path(path)
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Entry::Absent),
        Err(e) => Err(DevError::Refused(format!(
            "refused: cannot inspect {}: {e}",
            crate::output::path(path)
        ))),
    }
}

/// Check that `dir` is `canonical`'s rotation before anything in it is
/// written, claiming it when it is new (module doc).
fn admit(dir: &Path, canonical: &Path) -> Result<(), DevError> {
    let mut expected = canonical.as_os_str().as_encoded_bytes().to_vec();
    expected.push(b'\n');

    let origin = dir.join(ORIGIN);
    let origin_entry = entry(&origin)?;
    let mut any_slot = false;
    for index in 0..SIDECAR_BACKUP_COPIES {
        any_slot |= entry(&slot(dir, index))? == Entry::File;
    }

    match origin_entry {
        Entry::File => {
            let found = std::fs::read(&origin)?;
            if found != expected {
                return Err(DevError::Refused(format!(
                    "refused: {} belongs to {}, not {}",
                    crate::output::path(dir),
                    String::from_utf8_lossy(found.strip_suffix(b"\n").unwrap_or(&found)),
                    crate::output::path(canonical)
                )));
            }
        }
        Entry::Absent if any_slot => {
            return Err(DevError::Refused(format!(
                "refused: {} holds copies but no origin file, so nothing \
                 says whose they are; restore its origin file or move the directory aside",
                crate::output::path(dir)
            )));
        }
        Entry::Absent => crate::atomic_write::replace(&origin, &expected)?,
    }
    Ok(())
}

/// Remove the staging file a killed backup of the kind-keyed rotation an
/// earlier brokkr wrote leaves behind, by its exact constructed name. A
/// failure is a warning: the backup itself does not depend on it.
fn remove_legacy_partial(backups: &Path, project: &crate::project::Project) {
    let partial = backups.join(format!(".{}-sidecar.db.partial", project.name()));
    match std::fs::remove_file(&partial) {
        Ok(()) => crate::output::sidecar_msg(&format!(
            "removed an unfinished sidecar backup: {}",
            crate::output::path(&partial)
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => crate::output::warn(&format!(
            "could not remove the unfinished sidecar backup {}: {e}",
            crate::output::path(&partial)
        )),
    }
}
