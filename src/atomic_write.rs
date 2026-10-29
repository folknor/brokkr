//! Atomic replacement of a file: small hand-curated ones (`brokkr.toml`,
//! piners' `pins.toml`, the corpus digest, the hash cache and the
//! cargo config edited by `brokkr guard`) through [`replace`], and streamed
//! ones too large to hold in memory (tool and dataset downloads) through
//! [`Staged`]; plus a durable tier for the few records crash recovery reads
//! ([`replace_durable`], [`create_dir_all_durable`]).
//!
//! A plain `std::fs::write` truncates first and fills second, so a kill (or a
//! full disk) between the two leaves a truncated file no later run can load.
//! Both paths write a sibling temp file, fsync it, rename it over the target
//! and fsync the directory, the last three through one descriptor of the
//! target's directory, so a reader sees the old bytes or the new ones, never
//! a prefix. A leftover at the temp name (a killed run's file, or a link) is
//! removed before anything writes there: a temp file this module fills itself
//! is then created exclusively, never through a link, and written and
//! published through its descriptor, and a streamed writer is handed a name
//! that holds nothing, so it creates a fresh file. Any temp file must still be
//! the inode at its name and linked nowhere else when it is published, or
//! nothing is published, and one found to be another writer's is left to it.
//! A link put at the name while a streamed writer runs is beyond this. This
//! module is the one copy of that sequence; a hand-rolled temp-and-rename
//! elsewhere is a bug.
//!
//! Two tiers, told apart by what survives a crash (power loss, kernel panic),
//! on a journaling filesystem that keeps a rename atomic:
//!
//! - Atomic ([`replace`], [`Staged::commit`]): the temp file is fsynced before
//!   the rename, so after a crash the target holds the old bytes or the new
//!   ones, never a prefix - or is absent, when it did not exist before.
//!   Whether the new bytes survive is best effort: the directory fsync is
//!   attempted, a failure is warned about and the call still succeeds, since
//!   the target has already changed. A filesystem that refuses directory
//!   fsync outright (`EINVAL`, `EOPNOTSUPP`) is not warned about.
//! - Durable ([`replace_durable`]): success also means the directory fsync
//!   happened, so the new bytes survive a crash - provided the directory
//!   itself is durably reachable, which [`create_dir_all_durable`] establishes
//!   below an anchor the caller vouches for. A failure says whether the
//!   target was replaced ([`DurableError`]).

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// Atomically replace `path` with `contents`.
///
/// An existing target is resolved through any symlink first, so the link
/// survives and its target is what gets replaced, and the target's permissions
/// carry over. The temp file is `.<name>.tmp-<pid>-<n>` beside the target - hidden
/// and extension-less, so no `*.toml` directory scan ever picks it up - and is
/// removed on any failure before the rename. Crash survival of the new bytes
/// is best effort (the module doc's atomic tier).
pub(crate) fn replace(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut staged = Staged::per_process(path)?;
    let file = staged.write_new(contents)?;
    staged.finish(Some(file))
}

/// Why a [`replace_durable`] failed: before the target changed, or after.
#[derive(Debug)]
pub(crate) enum DurableError {
    /// Nothing was published; the target is untouched.
    Unpublished(std::io::Error),
    /// The target already holds the new bytes, but the fsync of `dir` that
    /// makes them survive a crash did not happen.
    Unconfirmed { dir: PathBuf, error: std::io::Error },
}

impl std::fmt::Display for DurableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unpublished(e) => write!(f, "{e}"),
            Self::Unconfirmed { dir, error } => write!(
                f,
                "replaced, but syncing {} failed: {error}; whether the new contents \
                 survive a crash is unconfirmed",
                crate::output::path(dir)
            ),
        }
    }
}

/// Replace `path` with `contents` as [`replace`] does, and succeed only once
/// the directory fsync happened, so the new bytes survive a crash provided
/// the directory is durably reachable ([`create_dir_all_durable`]).
///
/// Unlike [`replace`], the target is never resolved: the new file is
/// published at `path` itself, in the directory whose reachability the
/// caller established, and a target found to be a symlink is refused (one
/// that appears after that check is replaced, never followed, since the
/// rename does not follow it). A filesystem refusing directory fsync is
/// [`DurableError::Unconfirmed`], never success.
pub(crate) fn replace_durable(path: &Path, contents: &[u8]) -> Result<(), DurableError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(DurableError::Unpublished(std::io::Error::other(format!(
                "{} is a symlink, which a durable replacement refuses",
                crate::output::path(path)
            ))));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(DurableError::Unpublished(e)),
    }
    let mut staged = Staged::with_suffix(path, &per_process_suffix(), false)
        .map_err(DurableError::Unpublished)?;
    let file = staged
        .write_new(contents)
        .map_err(DurableError::Unpublished)?;
    let dir = staged.dir.clone();
    match staged
        .publish(Some(file))
        .map_err(DurableError::Unpublished)?
    {
        DirSync::Synced => Ok(()),
        DirSync::Unsupported(error) | DirSync::Failed(error) => {
            Err(DurableError::Unconfirmed { dir, error })
        }
    }
}

/// Make `dir` exist, with every directory from `anchor` down to it durable:
/// each is created if missing and fsynced, so its entries - the next one down
/// included - survive a crash. Every call syncs the whole chain, not only the
/// part it created, since an earlier call may have created it and died before
/// its syncs; existence is no evidence of durability.
///
/// `dir` must be `anchor` joined with plain names only. What this cannot
/// establish is assumed, and the caller vouches for it: the anchor (the
/// user's home, in production) is durably reachable; a symlink met on the way
/// is the user's own setup, followed, and it, its resolution and its target's
/// reachability are durable (brokkr never creates one on these paths); and
/// nothing renames a directory on the chain meanwhile. A bind mount or chroot
/// hiding a directory's real parent is outside what any walk can see.
pub(crate) fn create_dir_all_durable(anchor: &Path, dir: &Path) -> std::io::Result<()> {
    let rest = dir.strip_prefix(anchor).map_err(|_| {
        std::io::Error::other(format!(
            "{} is not below {}",
            crate::output::path(dir),
            crate::output::path(anchor)
        ))
    })?;
    let mut names: Vec<OsString> = Vec::new();
    for component in rest.components() {
        match component {
            Component::Normal(name) => names.push(name.to_owned()),
            _ => {
                return Err(std::io::Error::other(format!(
                    "{} has a component below {} that is not a plain name",
                    crate::output::path(dir),
                    crate::output::path(anchor)
                )));
            }
        }
    }
    let mut current = open_dir(anchor)?;
    for name in &names {
        match mkdir_at(&current, name) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        current.sync_all()?;
        let follow = is_symlink_at(&current, name)?;
        current = open_dir_at(&current, name, follow)?;
    }
    current.sync_all()
}

/// A sibling temp file that becomes the target on [`Staged::commit`], for a
/// writer that produces the bytes itself (a `curl -o`, say) rather than
/// handing [`replace`] a buffer. Dropped uncommitted, it removes the temp file.
///
/// Target resolution, permission carry-over and the fsync/rename/dir-fsync
/// sequence are the ones [`replace`] documents - `replace` is built on this -
/// and so is its atomic tier.
///
/// A staged write of a file a pbfhogg launch can use - a dataset download, a
/// pbfhogg child's output - is reserved (`crate::reservations`), in one of two
/// ways. [`Staged::stable_reserved`] holds a write reservation on the temp
/// file and the target from construction, before any byte is written, until
/// its commit or drop, so no launch reading or writing either is admitted
/// meanwhile, and its rename or removal runs as that reservation is released.
/// [`Staged::stable_keeping_for_child`] holds nothing, because the pbfhogg
/// launch writing its temp file reserves it; its commit and drop go through
/// the gate. Every other staged write - brokkr's own state and config files,
/// which no launch reads or writes - is plain, outside the reservation layer,
/// so a lease store problem never stops brokkr writing its own files.
pub(crate) struct Staged {
    target: PathBuf,
    dir: PathBuf,
    tmp: PathBuf,
    mode: Mode,
    committed: bool,
    /// The drop must leave the name at `tmp` alone: it holds another
    /// writer's file, or a reservation this write has yet to take may cover
    /// it.
    leave_tmp: bool,
}

/// How a staged write relates to the reservation layer.
enum Mode {
    /// brokkr's own files: no reservation, no gate.
    Plain,
    /// A write reservation on the temp file and target, released at the end.
    Reserved(Option<crate::reservations::Hold>),
    /// The temp file is a reserved launch's output: gated commit and drop.
    ForChild,
}

/// What the directory fsync after a publication came to.
enum DirSync {
    Synced,
    /// The filesystem offers no directory fsync.
    Unsupported(std::io::Error),
    Failed(std::io::Error),
}

impl Staged {
    /// Temp file `.<name>.tmp-<pid>-<n>`: unique per write, so two processes
    /// or two threads replacing the same file never write into one temp
    /// file. A kill leaves it behind under a name a later run only reuses
    /// after the pid comes round again, and then removes first - fine for
    /// small files.
    pub(crate) fn per_process(path: &Path) -> std::io::Result<Self> {
        Self::with_suffix(path, &per_process_suffix(), true)
    }

    /// Temp file `.<name>.partial`: the same name on every attempt, so an
    /// interrupted multi-gigabyte download is overwritten by the retry rather
    /// than leaking one partial per killed process. Only for writers already
    /// serialised by something else (the global brokkr lock, for downloads) -
    /// two concurrent writers would share the file.
    ///
    /// Every streamed constructor removes a leftover at the temp name before
    /// handing the name out, so the writer creates a fresh file rather than
    /// writing through a stale link into a live one.
    pub(crate) fn stable(path: &Path) -> std::io::Result<Self> {
        let staged = Self::with_suffix(path, "partial", true)?;
        clear(&staged.tmp)?;
        Ok(staged)
    }

    /// As [`stable`](Self::stable), for a file a pbfhogg launch can use (a
    /// dataset download): reserved for writing from now until commit or drop.
    pub(crate) fn stable_reserved(path: &Path) -> std::io::Result<Self> {
        let mut staged = Self::with_suffix(path, "partial", true)?;
        // Until the reservation is held, another launch may reserve the temp
        // name, so a failure here must leave it alone.
        staged.leave_tmp = true;
        staged.mode = Mode::Reserved(Some(crate::reservations::hold(&[
            &staged.tmp,
            &staged.target,
        ])?));
        staged.leave_tmp = false;
        // Under this write's own reservation, so no launch is using it.
        clear(&staged.tmp)?;
        Ok(staged)
    }

    /// As [`stable`](Self::stable), but for a pbfhogg child as the writer: the
    /// child infers its output format from the file name (`-o x.osm.pbf`), so
    /// when the target's name ends in `keep` the temp file is
    /// `.<rest>.partial<keep>` and the child sees the same format suffix. The
    /// child's launch reserves the temp file as its output while it writes,
    /// so this holds no reservation of its own; its commit and drop still go
    /// through the gate.
    pub(crate) fn stable_keeping_for_child(path: &Path, keep: &str) -> std::io::Result<Self> {
        let mut staged = Self::with_suffix(path, "partial", true)?;
        let name = staged
            .target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(rest) = name.strip_suffix(keep).filter(|r| !r.is_empty()) {
            staged.tmp = staged.dir.join(format!(".{rest}.partial{keep}"));
        }
        // Gated from here on, a refusal's drop included.
        staged.mode = Mode::ForChild;
        crate::reservations::mutate(&[&staged.tmp, &staged.target], &[], || clear(&staged.tmp))?;
        Ok(staged)
    }

    /// With `resolve`, an existing target is canonicalized, so a symlinked one
    /// is replaced where it leads; without, `path` is the target as spelled.
    fn with_suffix(path: &Path, suffix: &str, resolve: bool) -> std::io::Result<Self> {
        let target = if resolve {
            match std::fs::canonicalize(path) {
                Ok(p) => p,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
                Err(e) => return Err(e),
            }
        } else {
            path.to_path_buf()
        };
        let dir = target
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = dir.join(format!(".{name}.{suffix}"));
        Ok(Self {
            target,
            dir,
            tmp,
            mode: Mode::Plain,
            committed: false,
            leave_tmp: false,
        })
    }

    /// Where the writer puts the new bytes.
    pub(crate) fn tmp_path(&self) -> &Path {
        &self.tmp
    }

    /// The next spawn of `program` on this thread writes the temp file (a
    /// `curl -o`): that child inherits this write's reservation, so it stays
    /// held while the child lives, even past a brokkr killed under it.
    pub(crate) fn lend_to_child(&self, program: &str) {
        if let Mode::Reserved(Some(hold)) = &self.mode {
            hold.lend_to(Path::new(program));
        }
    }

    /// Make the temp file the target: carry the target's permissions over,
    /// fsync, rename, fsync the directory. On failure the temp file is removed
    /// (by the drop) and the target is untouched. A failed directory fsync is
    /// not a failure - the target has changed by then - but is warned about:
    /// whether the new bytes survive a crash is unconfirmed.
    pub(crate) fn commit(self) -> std::io::Result<()> {
        self.finish(None)
    }

    /// The atomic tier's commit, of the temp file `file` when this module
    /// filled it, else of the one at the temp name.
    fn finish(self, file: Option<File>) -> std::io::Result<()> {
        let (target, dir) = (self.target.clone(), self.dir.clone());
        match self.publish(file)? {
            DirSync::Synced | DirSync::Unsupported(_) => {}
            DirSync::Failed(e) => crate::output::warn(&format!(
                "{} was replaced, but syncing {} failed: {e}; whether the new contents \
                 survive a crash is unconfirmed",
                crate::output::path(&target),
                crate::output::path(&dir)
            )),
        }
        Ok(())
    }

    /// Create the temp file afresh and fill it with `contents`, for the
    /// plain writes this module fills itself: a leftover at its name (a
    /// killed run's, or a link put there) is removed first, and the new one
    /// is created exclusively and never through a link, so the bytes land in
    /// an inode nothing else names. The caller publishes this descriptor. A
    /// name taken between the removal and the creation is another writer's
    /// and is left to it.
    fn write_new(&mut self, contents: &[u8]) -> std::io::Result<File> {
        let dir = open_dir(&self.dir)?;
        let name = file_name(&self.tmp)?;
        match unlink_at(&dir, &name) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let created = open_at(
            &dir,
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        );
        let mut file = match created {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.leave_tmp = true;
                return Err(e);
            }
            other => other?,
        };
        file.write_all(contents)?;
        Ok(file)
    }

    /// The commit sequence, through one descriptor of the directory opened
    /// before anything changes, so the temp file synced, the rename and the
    /// directory fsync all address the same directory. The temp file - `file`,
    /// or the one at the temp name, opened without following a link - must
    /// still be the inode at the temp name and have no other link, or nothing
    /// is published. An `Err` means nothing was published; once the rename
    /// succeeds the write is committed, and the directory fsync's outcome is
    /// returned for the tier to judge, after the reservation section has
    /// ended.
    fn publish(mut self, file: Option<File>) -> std::io::Result<DirSync> {
        let dir = open_dir(&self.dir)?;
        let tmp_name = file_name(&self.tmp)?;
        let target_name = file_name(&self.target)?;
        let tmp_shown = self.tmp.clone();
        let foreign = std::cell::Cell::new(false);
        let publish = || -> std::io::Result<()> {
            let f = match file {
                Some(f) => f,
                None => open_file_at(&dir, &tmp_name)?,
            };
            let meta = f.metadata()?;
            let at_name = stat_at(&dir, &tmp_name, false)?;
            if (at_name.st_dev, at_name.st_ino) != (meta.dev(), meta.ino()) {
                foreign.set(true);
                return Err(std::io::Error::other(format!(
                    "{} is no longer the temp file this write filled",
                    crate::output::path(&tmp_shown)
                )));
            }
            if meta.nlink() != 1 {
                return Err(std::io::Error::other(format!(
                    "{} has another link, so publishing it would change another file",
                    crate::output::path(&tmp_shown)
                )));
            }
            if let Some(mode) = mode_at(&dir, &target_name) {
                f.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            f.sync_all()?;
            drop(f);
            rename_at(&dir, &tmp_name, &target_name) // gate: under release_then or mutate, or Plain
        };
        let targets = [self.tmp.as_path(), self.target.as_path()];
        let published = match &mut self.mode {
            Mode::Plain => publish(),
            Mode::Reserved(hold) => match hold.take() {
                Some(hold) => hold.release_then(&targets, publish),
                None => crate::reservations::mutate(&targets, &[], publish),
            },
            Mode::ForChild => crate::reservations::mutate(&targets, &[], publish),
        };
        if foreign.get() {
            self.leave_tmp = true;
        }
        published?;
        self.committed = true;
        Ok(match dir.sync_all() {
            Ok(()) => DirSync::Synced,
            // ENOTSUP is EOPNOTSUPP on Linux.
            Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)) => {
                DirSync::Unsupported(e)
            }
            Err(e) => DirSync::Failed(e),
        })
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        // A temp name left alone is not this write's to remove; a
        // reservation still held is released by its own drop.
        if self.committed || self.leave_tmp {
            return;
        }
        // A reserved one is refused while another launch still reserves the
        // temp file.
        let tmp = self.tmp.clone();
        let remove = move || std::fs::remove_file(&tmp); // gate: under release_then
        let removed = match &mut self.mode {
            Mode::Plain => std::fs::remove_file(&self.tmp), // gate: brokkr's own file
            Mode::Reserved(hold) => match hold.take() {
                Some(hold) => hold.release_then(&[self.tmp.as_path()], remove),
                None => crate::reservations::remove_file(&self.tmp),
            },
            Mode::ForChild => crate::reservations::remove_file(&self.tmp),
        };
        drop(removed);
    }
}

/// `tmp-<pid>-<n>`, the suffix of [`Staged::per_process`]'s temp name, `n`
/// counting this process's writes.
fn per_process_suffix() -> String {
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("tmp-{}-{n}", std::process::id())
}

/// Remove whatever is at a temp name before a writer is handed it - a link
/// is removed, never followed; absent is fine.
fn clear(tmp: &Path) -> std::io::Result<()> {
    let removed = std::fs::remove_file(tmp); // gate: under its write's reservation, the gate, or Plain
    match removed {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn file_name(path: &Path) -> std::io::Result<OsString> {
    path.file_name()
        .map(OsStr::to_owned)
        .ok_or_else(|| std::io::Error::other(format!("{} names no file", path.display())))
}

fn c_name(name: &OsStr) -> std::io::Result<CString> {
    CString::new(name.as_bytes()).map_err(std::io::Error::other)
}

/// A directory descriptor for fsync and the `*at` calls below.
fn open_dir(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

/// `openat` with `flags`, the result owned; a file it creates gets `0o666`
/// less the umask, as `File::create` gives.
fn open_at(dir: &File, name: &OsStr, flags: libc::c_int) -> std::io::Result<File> {
    let name = c_name(name)?;
    let mode: libc::c_uint = 0o666;
    // SAFETY: an open descriptor, a NUL-terminated name and the mode the
    // variadic argument takes; the returned descriptor is checked and then
    // owned by the File alone.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The temp file, for its permissions and fsync, never through a symlink.
fn open_file_at(dir: &File, name: &OsStr) -> std::io::Result<File> {
    open_at(dir, name, libc::O_WRONLY | libc::O_NOFOLLOW)
}

/// A child directory; a symlink is only followed when `follow`.
fn open_dir_at(dir: &File, name: &OsStr, follow: bool) -> std::io::Result<File> {
    let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
    open_at(dir, name, libc::O_RDONLY | libc::O_DIRECTORY | nofollow)
}

/// `fstatat` of a name in `dir`, following a symlink only when `follow`.
fn stat_at(dir: &File, name: &OsStr, follow: bool) -> std::io::Result<libc::stat> {
    let name = c_name(name)?;
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    // SAFETY: zeroed is a valid `stat`, which the call overwrites on success.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: an open descriptor, a NUL-terminated name and a live `stat`.
    if unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, flags) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

/// The target's permission bits, when it exists and can be inspected.
fn mode_at(dir: &File, name: &OsStr) -> Option<u32> {
    stat_at(dir, name, true).ok().map(|st| st.st_mode & 0o7777)
}

/// Whether `name` in `dir` is a symlink. The open that follows tells a
/// symlink apart only by `ENOTDIR`, the same errno as a file, so the type is
/// asked for first.
fn is_symlink_at(dir: &File, name: &OsStr) -> std::io::Result<bool> {
    stat_at(dir, name, false).map(|st| st.st_mode & libc::S_IFMT == libc::S_IFLNK)
}

fn mkdir_at(dir: &File, name: &OsStr) -> std::io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: an open descriptor and a NUL-terminated name.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Remove a non-directory `name` in `dir`, never following a link.
fn unlink_at(dir: &File, name: &OsStr) -> std::io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: an open descriptor and a NUL-terminated name.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn rename_at(dir: &File, from: &OsStr, to: &OsStr) -> std::io::Result<()> {
    let (from, to) = (c_name(from)?, c_name(to)?);
    let fd = dir.as_raw_fd();
    // SAFETY: one open descriptor and two NUL-terminated names.
    if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::{DurableError, Staged, create_dir_all_durable, replace, replace_durable};

    #[test]
    fn replaces_contents_and_leaves_no_temp_behind() {
        let dir = crate::test_scratch::scratch("atomic_write", "replace");
        let path = dir.join("pins.toml");
        std::fs::write(&path, "old = 1\n").unwrap();

        replace(&path, b"new = 2\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = 2\n");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != "pins.toml")
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn creates_a_missing_file() {
        let dir = crate::test_scratch::scratch("atomic_write", "create");
        let path = dir.join("new.toml");
        replace(&path, b"a = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 1\n");
    }

    #[test]
    fn a_symlink_survives_and_its_target_is_replaced() {
        let dir = crate::test_scratch::scratch("atomic_write", "symlink");
        let real = dir.join("real.toml");
        let link = dir.join("brokkr.toml");
        std::fs::write(&real, "old\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        replace(&link, b"new\n").unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new\n");
    }

    #[test]
    fn permissions_carry_over() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_scratch::scratch("atomic_write", "permissions");
        let path = dir.join("run.sh");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        replace(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o750);
    }

    #[test]
    fn a_link_at_the_temp_name_never_receives_the_bytes() {
        let dir = crate::test_scratch::scratch("atomic_write", "temp_alias");
        let path = dir.join("pins.toml");
        let other = dir.join("other.toml");
        std::fs::write(&path, "old").unwrap();
        std::fs::write(&other, "theirs").unwrap();
        let mut staged = Staged::per_process(&path).unwrap();
        std::os::unix::fs::symlink(&path, staged.tmp_path()).unwrap();
        let file = staged.write_new(b"one").unwrap();
        staged.finish(Some(file)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one");
        let mut staged = Staged::per_process(&path).unwrap();
        std::fs::hard_link(&other, staged.tmp_path()).unwrap();
        let file = staged.write_new(b"two").unwrap();
        staged.finish(Some(file)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "theirs");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    }

    #[test]
    fn a_stream_temp_name_is_cleared_and_a_linked_one_never_published() {
        let dir = crate::test_scratch::scratch("atomic_write", "stream_alias");
        let path = dir.join("tool.jar");
        let other = dir.join("other.jar");
        std::fs::write(&other, "theirs").unwrap();
        std::fs::hard_link(&other, dir.join(".tool.jar.partial")).unwrap();
        // The constructor removes the leftover link before any writer runs.
        let staged = Staged::stable(&path).unwrap();
        assert!(!staged.tmp_path().exists());
        // One linked after the writer ran is refused at commit.
        std::fs::hard_link(&other, staged.tmp_path()).unwrap();
        assert!(staged.commit().is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "theirs");
    }

    #[test]
    fn a_refused_reserved_construction_leaves_a_held_temp_alone() {
        let dir = crate::test_scratch::scratch("atomic_write", "refused_hold");
        let pbf = dir.join("dk.osm.pbf");
        let reserved_tmp = dir.join(".dk.osm.pbf.partial");
        let child_tmp = dir.join(".dk.partial.osm.pbf");
        std::fs::write(&reserved_tmp, "theirs").unwrap();
        std::fs::write(&child_tmp, "theirs").unwrap();
        let other = crate::reservations::hold(&[&reserved_tmp, &child_tmp]).unwrap();
        assert!(Staged::stable_reserved(&pbf).is_err());
        assert!(Staged::stable_keeping_for_child(&pbf, ".osm.pbf").is_err());
        assert_eq!(std::fs::read_to_string(&reserved_tmp).unwrap(), "theirs");
        assert_eq!(std::fs::read_to_string(&child_tmp).unwrap(), "theirs");
        drop(other);
    }

    #[test]
    fn a_missing_directory_publishes_nothing() {
        let dir = crate::test_scratch::scratch("atomic_write", "no_dir");
        let err = replace(&dir.join("gone/x.toml"), b"a").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!dir.join("gone").exists());
    }

    #[test]
    fn an_uncommitted_stage_leaves_the_target_and_no_temp() {
        let dir = crate::test_scratch::scratch("atomic_write", "uncommitted");
        let path = dir.join("planet.osm.pbf");
        std::fs::write(&path, "old").unwrap();
        {
            let staged = Staged::stable(&path).unwrap();
            // Distinct from the `.with_extension("tmp")` shape, which mapped
            // `a.osm.pbf` and `a.osm.gz` onto one `a.osm.tmp`.
            assert_eq!(
                staged.tmp_path().file_name().unwrap(),
                ".planet.osm.pbf.partial"
            );
            std::fs::write(staged.tmp_path(), "half").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_kept_suffix_survives_into_the_temp_name() {
        let dir = crate::test_scratch::scratch("atomic_write", "keeping");
        let pbf =
            Staged::stable_keeping_for_child(&dir.join("dk-merged.osm.pbf"), ".osm.pbf").unwrap();
        assert_eq!(
            pbf.tmp_path().file_name().unwrap(),
            ".dk-merged.partial.osm.pbf"
        );
        // A name without the suffix falls back to the plain stable shape.
        let other = Staged::stable_keeping_for_child(&dir.join("a.png"), ".osm.pbf").unwrap();
        assert_eq!(other.tmp_path().file_name().unwrap(), ".a.png.partial");
    }

    #[test]
    fn a_committed_stage_replaces_the_target() {
        let dir = crate::test_scratch::scratch("atomic_write", "committed");
        let path = dir.join("tool.jar");
        let staged = Staged::stable(&path).unwrap();
        std::fs::write(staged.tmp_path(), "whole").unwrap();
        staged.commit().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "whole");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_durable_replacement_writes_and_refuses_a_symlink() {
        let dir = crate::test_scratch::scratch("atomic_write", "durable");
        let path = dir.join("record.json");
        replace_durable(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");

        let real = dir.join("real.json");
        let link = dir.join("link.json");
        std::fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            replace_durable(&link, b"new"),
            Err(DurableError::Unpublished(_))
        ));
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "old");
    }

    #[test]
    fn a_durable_chain_is_created_through_a_symlink_and_rewalked() {
        let anchor = crate::test_scratch::scratch("atomic_write", "chain");
        std::fs::create_dir(anchor.join("real")).unwrap();
        std::os::unix::fs::symlink(anchor.join("real"), anchor.join("link")).unwrap();
        let dir = anchor.join("link/a/b");
        create_dir_all_durable(&anchor, &dir).unwrap();
        assert!(anchor.join("real/a/b").is_dir());
        // A second call over an existing chain syncs it again and succeeds.
        create_dir_all_durable(&anchor, &dir).unwrap();
        create_dir_all_durable(&anchor, &anchor).unwrap();
    }

    #[test]
    fn a_durable_chain_refuses_an_escape_or_a_file() {
        let anchor = crate::test_scratch::scratch("atomic_write", "chain_refusals");
        assert!(create_dir_all_durable(&anchor, &anchor.join("a/../b")).is_err());
        assert!(create_dir_all_durable(&anchor.join("x"), &anchor).is_err());
        std::fs::write(anchor.join("file"), "").unwrap();
        assert!(create_dir_all_durable(&anchor, &anchor.join("file/c")).is_err());
        assert!(!anchor.join("b").exists());
    }
}
