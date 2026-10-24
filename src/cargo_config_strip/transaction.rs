//! Installing and removing the filtered cargo config, by rename only.
//!
//! For the selected file `F` in a tree's `.cargo` directory, three names are
//! reserved and brokkr-owned: `F.brokkr-original` (S, the original object
//! moved off `F`), `F.brokkr-installing` (T, scratch that only ever leaves by
//! a no-clobber rename onto `F`) and `F.brokkr-displaced` (D, whatever object
//! was on `F` when a restore began, potentially user data). Every move is
//! `renameat2(RENAME_NOREPLACE)`, so no move ever replaces an object; the only
//! object brokkr ever deletes without moving is one it has just verified as
//! its own install, or its own scratch inside a non-conflict branch.
//!
//! The installed file is self-verifying: its first line names the hash of
//! the original it was filtered from and the hash of the rest of itself, and
//! its mode equals the original's. Recovery is therefore independent of the
//! current policy, and runs the same way whether called before a new
//! install, at the end of a locked window, before a worktree is admitted for
//! reuse, or from the watchdog's backstop.
//!
//! What is promised: a competing pathname replacement of `F` (an editor
//! saving) is never lost - it ends in a conflict that preserves every object.
//! What is not: writes through a descriptor already open on an inode brokkr
//! moved, and any external change to the reserved names.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::DevError;
use crate::output;

/// The two names cargo reads in a `.cargo` directory, legacy first.
pub const BASES: [&str; 2] = ["config", "config.toml"];
pub const ORIGINAL: &str = ".brokkr-original";
pub const INSTALLING: &str = ".brokkr-installing";
pub const DISPLACED: &str = ".brokkr-displaced";
const MARKER: &str = "# brokkr strip_rustflags v1 ";

/// Serializes every install, restore and recovery in this process, and lets
/// the watchdog's backstop take the transaction away from a stuck main thread.
static TXN: Mutex<()> = Mutex::new(());
/// Set by the backstop once it owns [`TXN`] for good.
static TAKEN_OVER: AtomicBool = AtomicBool::new(false);

fn txn() -> Option<std::sync::MutexGuard<'static, ()>> {
    let guard = TXN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    (!TAKEN_OVER.load(Ordering::SeqCst)).then_some(guard)
}

struct Names {
    live: PathBuf,
    original: PathBuf,
    installing: PathBuf,
    displaced: PathBuf,
}

fn names(cargo_dir: &Path, base: &str) -> Names {
    Names {
        live: cargo_dir.join(base),
        original: cargo_dir.join(format!("{base}{ORIGINAL}")),
        installing: cargo_dir.join(format!("{base}{INSTALLING}")),
        displaced: cargo_dir.join(format!("{base}{DISPLACED}")),
    }
}

fn hash(bytes: &[u8]) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes))
}

fn marker_line(original: &[u8], body: &[u8]) -> String {
    format!("{MARKER}original={} body={}\n", hash(original), hash(body))
}

fn io_error(context: &str, error: &std::io::Error) -> DevError {
    DevError::Io(std::io::Error::new(error.kind(), format!("{context}: {error}")))
}

fn exists(path: &Path) -> Result<bool, DevError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_error(&format!("cannot inspect {}", path.display()), &e)),
    }
}

/// `rename(from, to)` that fails rather than replace an existing `to`.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "nul in path"))?;
    let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "nul in path"))?;
    // SAFETY: both pointers are valid NUL-terminated strings for the call.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn mv(from: &Path, to: &Path) -> Result<(), DevError> {
    rename_noreplace(from, to).map_err(|e| {
        io_error(
            &format!("cargo config strip: cannot move {} to {}", from.display(), to.display()),
            &e,
        )
    })
}

fn remove(path: &Path) -> Result<(), DevError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(&format!("cargo config strip: cannot remove {}", path.display()), &e)),
    }
}

/// Remove scratch brokkr just created, on a path already failing: a leftover
/// is only noise, since recovery discards it in the next non-conflict pass.
fn discard_scratch(path: &Path) {
    if let Err(error) = remove(path) {
        output::warn(&error.to_string());
    }
}

/// A regular file's bytes and permission bits, or `None` for anything else.
fn regular(path: &Path) -> Result<Option<(Vec<u8>, u32)>, DevError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(&format!("cannot inspect {}", path.display()), &e)),
    };
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(path)
        .map_err(|e| io_error(&format!("cannot read {}", path.display()), &e))?;
    Ok(Some((bytes, meta.permissions().mode() & 0o7777)))
}

/// Whether `candidate` is brokkr's untouched install over `original`: its
/// marker names `original`'s hash and the hash of its own body, and its mode
/// is `original`'s.
fn installed_over(candidate: &Path, original: &[u8], mode: u32) -> Result<bool, DevError> {
    let Some((bytes, candidate_mode)) = regular(candidate)? else {
        return Ok(false);
    };
    let Some(newline) = bytes.iter().position(|b| *b == b'\n') else {
        return Ok(false);
    };
    let (line, body) = bytes.split_at(newline + 1);
    Ok(candidate_mode == mode && line == marker_line(original, body).as_bytes())
}

/// [`installed_over`] for two paths, for the git exemption: `live` is an
/// untouched install over the object at `original`.
pub fn verifies(live: &Path, original: &Path) -> bool {
    match regular(original) {
        Ok(Some((bytes, mode))) => installed_over(live, &bytes, mode).unwrap_or(false),
        _ => false,
    }
}

fn is_marked(path: &Path) -> Result<bool, DevError> {
    Ok(regular(path)?.is_some_and(|(bytes, _)| bytes.starts_with(MARKER.as_bytes())))
}

fn conflict(cargo_dir: &Path, base: &str) -> DevError {
    let n = names(cargo_dir, base);
    let present: Vec<String> = [
        (&n.live, "the file cargo reads"),
        (&n.original, "the original brokkr moved aside"),
        (&n.installing, "brokkr scratch"),
        (&n.displaced, "the object on the live path when a restore began"),
    ]
    .iter()
    .filter(|(path, _)| exists(path).unwrap_or(true))
    .map(|(path, role)| format!("{} ({role})", path.display()))
    .collect();
    DevError::Refused(format!(
        "strip_rustflags: {} is in a state brokkr will not resolve on its own, so \
         nothing was changed. Present: {}. Keep the content you want at {}, delete the other \
         brokkr-reserved files, then rerun",
        n.live.display(),
        present.join(", "),
        n.live.display(),
    ))
}

/// Put one name's transaction back to its original, whatever state a run
/// left it in. Policy-independent; the caller holds [`TXN`].
fn recover_name(cargo_dir: &Path, base: &str) -> Result<(), DevError> {
    let n = names(cargo_dir, base);
    let (live, original, displaced) = (exists(&n.live)?, exists(&n.original)?, exists(&n.displaced)?);
    let resolved = match (live, original, displaced) {
        (false, false, false) => true,
        // A filtered copy whose original is gone cannot be undone here.
        (true, false, false) => !is_marked(&n.live)?,
        // Moved aside, never installed (or restored halfway): the original
        // inode goes back; it is the only copy, so nothing can be lost.
        (false, true, false) => {
            mv(&n.original, &n.live)?;
            true
        }
        (true, true, false) => restore_installed(&n)?,
        (false, true, true) => resume_restore(&n)?,
        (true, false, true) => discard_displaced(&n)?,
        (true, true, true) | (false, false, true) => false,
    };
    if !resolved {
        return Err(conflict(cargo_dir, base));
    }
    // Scratch is only ever discarded once the rest was resolved.
    remove(&n.installing)
}

/// The installed state: displace the live file, and only when it is brokkr's
/// untouched install put the original back and drop it. `false` is a
/// conflict, with every object kept.
fn restore_installed(n: &Names) -> Result<bool, DevError> {
    let Some((bytes, mode)) = regular(&n.original)? else {
        return Ok(false);
    };
    if !installed_over(&n.live, &bytes, mode)? {
        return Ok(false);
    }
    mv(&n.live, &n.displaced)?;
    if !installed_over(&n.displaced, &bytes, mode)? {
        // Replaced between the check and the move: put it back.
        mv(&n.displaced, &n.live)?;
        return Ok(false);
    }
    mv(&n.original, &n.live)?;
    remove(&n.displaced)?;
    Ok(true)
}

/// A restore interrupted after displacing the live file: finish it when the
/// displaced object is brokkr's install, else put that object back.
fn resume_restore(n: &Names) -> Result<bool, DevError> {
    let Some((bytes, mode)) = regular(&n.original)? else {
        return Ok(false);
    };
    if installed_over(&n.displaced, &bytes, mode)? {
        mv(&n.original, &n.live)?;
        remove(&n.displaced)?;
        Ok(true)
    } else {
        mv(&n.displaced, &n.live)?;
        Ok(false)
    }
}

/// A restore interrupted before dropping the displaced install: drop it only
/// when it verifies over the restored original.
fn discard_displaced(n: &Names) -> Result<bool, DevError> {
    let Some((bytes, mode)) = regular(&n.live)? else {
        return Ok(false);
    };
    if !installed_over(&n.displaced, &bytes, mode)? {
        return Ok(false);
    }
    remove(&n.displaced)?;
    Ok(true)
}

/// Whether `dir`'s `.cargo` holds anything recovery would act on: a reserved
/// name, or a live file carrying brokkr's marker. For a lock-free command
/// deciding whether it must take the lock (whose activation recovers) before
/// reading the tree's cargo config. An unreadable state counts as leftovers.
pub fn has_leftovers(dir: &Path) -> bool {
    let cargo_dir = dir.join(".cargo");
    BASES.iter().any(|base| {
        let n = names(&cargo_dir, base);
        [&n.original, &n.installing, &n.displaced]
            .iter()
            .any(|path| exists(path).unwrap_or(true))
            || is_marked(&n.live).unwrap_or(true)
    })
}

/// Recover every reserved name in `dir`'s `.cargo`, before anything selects or
/// reads the file cargo will use.
pub fn recover(dir: &Path) -> Result<(), DevError> {
    let cargo_dir = dir.join(".cargo");
    if !exists(&cargo_dir)? {
        return Ok(());
    }
    let Some(_guard) = txn() else {
        return Err(DevError::Interrupted);
    };
    let mut first = None;
    for base in BASES {
        if let Err(error) = recover_name(&cargo_dir, base) {
            first.get_or_insert(error);
        }
    }
    // A recovered file changes what the host provenance would read.
    crate::build_provenance::forget_host_cache();
    first.map_or(Ok(()), Err)
}

/// The watchdog's backstop: take [`TXN`] away from a main thread that did not
/// unwind, recover `dirs`, and keep the mutex until the process exits so the
/// main thread can never resume a transaction underneath. Gives up quietly
/// when the mutex stays busy; next-run recovery handles any state left.
pub fn take_over_and_recover(dirs: &[PathBuf]) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let guard = loop {
        match TXN.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    };
    TAKEN_OVER.store(true, Ordering::SeqCst);
    for dir in dirs {
        let cargo_dir = dir.join(".cargo");
        for base in BASES {
            if let Err(error) = recover_name(&cargo_dir, base) {
                output::error_forced(&error.to_string());
            }
        }
    }
    std::mem::forget(guard);
}

/// A filtered config in place. Dropping it restores the original.
#[derive(Debug)]
pub struct Installed {
    cargo_dir: PathBuf,
    base: String,
}

impl Drop for Installed {
    fn drop(&mut self) {
        let Some(_guard) = txn() else {
            return;
        };
        match recover_name(&self.cargo_dir, &self.base) {
            Ok(()) => crate::build_provenance::forget_host_cache(),
            Err(error) => output::warn(&error.to_string()),
        }
    }
}

/// Install `filter(original)` over the selected file `live`. `filter` is run
/// on the bytes actually moved off `live`; `None` from it (no match, or the
/// object changed under the pre-scan) restores the original and installs
/// nothing.
pub fn install(
    live: &Path,
    filter: impl Fn(&str) -> Result<Option<String>, DevError>,
) -> Result<Option<Installed>, DevError> {
    let cargo_dir = live
        .parent()
        .ok_or_else(|| DevError::Internal(format!("{} has no parent", live.display())))?;
    let base = live
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| DevError::Internal(format!("{} has no file name", live.display())))?;
    let n = names(cargo_dir, base);
    let Some(_guard) = txn() else {
        return Err(DevError::Interrupted);
    };
    rename_noreplace(&n.live, &n.original).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            conflict(cargo_dir, base)
        } else {
            io_error(&format!("cargo config strip: cannot move {} aside", n.live.display()), &e)
        }
    })?;
    // From here every exit either installs or puts the original back.
    let put_back = |error: DevError| -> DevError {
        match rename_noreplace(&n.original, &n.live) {
            Ok(()) => error,
            Err(_) => conflict(cargo_dir, base),
        }
    };
    let (bytes, mode) = match regular(&n.original) {
        Ok(Some(found)) => found,
        Ok(None) => return Err(put_back(conflict(cargo_dir, base))),
        Err(error) => return Err(put_back(error)),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            return Err(put_back(DevError::Config(format!(
                "Cargo config {} is not UTF-8",
                n.live.display()
            ))));
        }
    };
    let body = match filter(text) {
        Ok(Some(body)) => body,
        Ok(None) => {
            mv(&n.original, &n.live)?;
            return Ok(None);
        }
        Err(error) => return Err(put_back(error)),
    };
    let mut installed = marker_line(&bytes, body.as_bytes()).into_bytes();
    installed.extend_from_slice(body.as_bytes());
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&n.installing)?;
        file.write_all(&installed)?;
        // The umask may have narrowed the creation mode.
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()
    })();
    if let Err(e) = written {
        // An existing scratch name is not ours to remove.
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            discard_scratch(&n.installing);
        }
        return Err(put_back(io_error(
            &format!("cargo config strip: cannot write {}", n.installing.display()),
            &e,
        )));
    }
    if let Err(e) = rename_noreplace(&n.installing, &n.live) {
        discard_scratch(&n.installing);
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            // Something new took the live path; keep it and the original.
            return Err(conflict(cargo_dir, base));
        }
        return Err(put_back(io_error(
            &format!("cargo config strip: cannot install {}", n.live.display()),
            &e,
        )));
    }
    // Durability beyond process death is best effort.
    if let Ok(dir) = std::fs::File::open(cargo_dir)
        && let Err(e) = dir.sync_all()
    {
        output::warn(&format!("cargo config strip: cannot sync {}: {e}", cargo_dir.display()));
    }
    crate::build_provenance::forget_host_cache();
    Ok(Some(Installed {
        cargo_dir: cargo_dir.to_owned(),
        base: base.to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn cargo_dir(test: &str) -> PathBuf {
        let root = crate::test_scratch::scratch("cargo_config_strip", test);
        let dir = root.join(".cargo");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn upper(text: &str) -> Result<Option<String>, DevError> {
        Ok(Some(text.to_uppercase()))
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn install_then_drop_restores_bytes_and_mode() {
        let dir = cargo_dir("roundtrip");
        let live = dir.join("config.toml");
        std::fs::write(&live, "a = 1\n").unwrap();
        std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o640)).unwrap();
        let guard = install(&live, upper).unwrap().unwrap();
        let installed = std::fs::read_to_string(&live).unwrap();
        assert!(installed.starts_with(MARKER), "{installed}");
        assert!(installed.ends_with("A = 1\n"));
        assert_eq!(mode(&live), 0o640);
        assert!(verifies(&live, &dir.join("config.toml.brokkr-original")));
        drop(guard);
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "a = 1\n");
        assert_eq!(mode(&live), 0o640);
        for suffix in [ORIGINAL, INSTALLING, DISPLACED] {
            assert!(!dir.join(format!("config.toml{suffix}")).exists());
        }
    }

    #[test]
    fn no_match_puts_the_original_back() {
        let dir = cargo_dir("no_match");
        let live = dir.join("config.toml");
        std::fs::write(&live, "a = 1\n").unwrap();
        assert!(install(&live, |_| Ok(None)).unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "a = 1\n");
        assert!(!dir.join("config.toml.brokkr-original").exists());
    }

    #[test]
    fn an_edited_install_is_a_conflict_that_keeps_everything() {
        let dir = cargo_dir("edited");
        let live = dir.join("config.toml");
        std::fs::write(&live, "a = 1\n").unwrap();
        let guard = install(&live, upper).unwrap().unwrap();
        // An editor saving by rename over the live path.
        let staged = dir.join("editor-tmp");
        std::fs::write(&staged, "edited = true\n").unwrap();
        std::fs::rename(&staged, &live).unwrap();
        drop(guard);
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "edited = true\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml.brokkr-original")).unwrap(),
            "a = 1\n"
        );
        let err = recover(dir.parent().unwrap()).unwrap_err();
        assert!(err.to_string().contains("will not resolve"), "{err}");
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "edited = true\n");
    }

    #[test]
    fn recovery_handles_every_interrupted_state() {
        // Moved aside, never installed.
        let dir = cargo_dir("state_p1");
        std::fs::write(dir.join("config.toml.brokkr-original"), "o\n").unwrap();
        std::fs::write(dir.join("config.toml.brokkr-installing"), "scratch").unwrap();
        recover(dir.parent().unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("config.toml")).unwrap(), "o\n");
        assert!(!dir.join("config.toml.brokkr-installing").exists());

        // Installed, process died.
        let dir = cargo_dir("state_p2");
        let live = dir.join("config.toml");
        std::fs::write(&live, "o\n").unwrap();
        std::mem::forget(install(&live, upper).unwrap().unwrap());
        recover(dir.parent().unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "o\n");

        // Died mid-restore with the install displaced.
        let dir = cargo_dir("state_displaced");
        let live = dir.join("config.toml");
        std::fs::write(&live, "o\n").unwrap();
        std::mem::forget(install(&live, upper).unwrap().unwrap());
        std::fs::rename(&live, dir.join("config.toml.brokkr-displaced")).unwrap();
        recover(dir.parent().unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "o\n");
        assert!(!dir.join("config.toml.brokkr-displaced").exists());

        // Restored, died before removing the displaced install.
        let dir = cargo_dir("state_cleanup");
        let live = dir.join("config.toml");
        std::fs::write(&live, "o\n").unwrap();
        std::mem::forget(install(&live, upper).unwrap().unwrap());
        std::fs::rename(&live, dir.join("config.toml.brokkr-displaced")).unwrap();
        std::fs::rename(dir.join("config.toml.brokkr-original"), &live).unwrap();
        recover(dir.parent().unwrap()).unwrap();
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "o\n");
        assert!(!dir.join("config.toml.brokkr-displaced").exists());
    }

    #[test]
    fn a_filtered_file_without_its_original_refuses() {
        let dir = cargo_dir("orphan");
        let live = dir.join("config.toml");
        std::fs::write(&live, "o\n").unwrap();
        std::mem::forget(install(&live, upper).unwrap().unwrap());
        std::fs::remove_file(dir.join("config.toml.brokkr-original")).unwrap();
        assert!(recover(dir.parent().unwrap()).is_err());
        assert!(live.exists());
    }

    #[test]
    fn conflict_leaves_scratch_alone() {
        let dir = cargo_dir("conflict_scratch");
        std::fs::write(dir.join("config.toml"), "theirs\n").unwrap();
        std::fs::write(dir.join("config.toml.brokkr-original"), "o\n").unwrap();
        std::fs::write(dir.join("config.toml.brokkr-installing"), "scratch").unwrap();
        assert!(recover(dir.parent().unwrap()).is_err());
        assert!(dir.join("config.toml.brokkr-installing").exists());
    }
}
