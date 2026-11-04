//! Taking ownership of the database file before SQLite opens it.
//!
//! The file is named by one canonical path, so two servers given two
//! spellings of one file (a symlink, a relative parent) take the same lock:
//! an existing file is resolved fully, a missing one by its canonical parent
//! and its name. A file with another hard link is refused, since the other
//! name would get its own lock. The lock is an exclusive, non-blocking
//! `flock` on `<file>.lock` beside it, a file never removed, so a second
//! server fails at once rather than waiting, and a replacement lock file
//! cannot let one in. The lock coordinates servers only: a brokkr pointed at
//! the same file as its local store is outside it, and unsupported.

use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::store::Store;

/// The open store with the lock that makes this process its only server.
pub(crate) struct Owned {
    pub(crate) store: Store,
    pub(crate) lock: File,
    pub(crate) path: PathBuf,
}

/// The canonical path `db` names, creating its directory.
pub(crate) fn resolve(db: &Path) -> Result<PathBuf, String> {
    if !db.is_absolute() {
        return Err(format!("--db {} is not an absolute path", db.display()));
    }
    let (Some(parent), Some(name)) = (db.parent(), db.file_name()) else {
        return Err(format!("--db {} does not name a file", db.display()));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    match std::fs::symlink_metadata(db) {
        Ok(_) => std::fs::canonicalize(db).map_err(|e| {
            format!(
                "{} cannot be resolved (a link to nothing?): {e}",
                db.display()
            )
        }),
        Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {
            let parent = std::fs::canonicalize(parent)
                .map_err(|e| format!("cannot resolve {}: {e}", parent.display()))?;
            Ok(parent.join(name))
        }
        Err(e) => Err(format!("cannot inspect {}: {e}", db.display())),
    }
}

/// Lock the file `db` names and open the store in it, migrating it.
pub(crate) fn own(db: &Path) -> Result<Owned, String> {
    let path = resolve(db)?;
    match std::fs::metadata(&path) {
        Ok(meta) if !meta.is_file() => {
            return Err(format!("{} is not a regular file", path.display()));
        }
        Ok(meta) if meta.nlink() > 1 => {
            return Err(format!(
                "{} has {} hard links; another name for it would take its own lock",
                path.display(),
                meta.nlink()
            ));
        }
        Ok(_) => {}
        Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot inspect {}: {e}", path.display())),
    }
    let mut lock_name = path.file_name().unwrap_or_default().to_owned();
    lock_name.push(".lock");
    let lock_path = path.with_file_name(lock_name);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(format!(
                "another server holds {} (its lock is {})",
                path.display(),
                lock_path.display()
            ));
        }
        Err(TryLockError::Error(e)) => {
            return Err(format!("cannot lock {}: {e}", lock_path.display()));
        }
    }
    let store = Store::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Owned { store, lock, path })
}
