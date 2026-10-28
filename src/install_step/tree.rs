//! The installed-tree primitives: the fingerprint that is a copy step's
//! ownership evidence, the staging copy, and the removal of a retired tree.
//!
//! The fingerprint is SHA-256 over a versioned, length-delimited encoding of
//! the tree's protected representation: each entry's raw relative path bytes,
//! its type (regular file or directory), its permission bits, and a file's
//! length and bytes. The root is an entry with an empty path, so a lone file
//! and a directory hash through the same rule. Entries go depth first, each
//! directory's children in raw byte order. Ownership, timestamps, xattrs and
//! hard links are not part of the representation. Any other object type is not
//! hashed at all - it is reported as foreign, without being opened, since no
//! payload brokkr publishes can contain one.

use std::ffi::OsString;
use std::fs::Metadata;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

use crate::error::DevError;
use crate::preflight::digest::Sha256State as Sha256;

const FORMAT: &[u8] = b"brokkr install tree v1\0";

/// What sits at a path, as the fingerprint sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Observed {
    Absent,
    /// Regular files and directories only; the digest in lowercase hex.
    Tree(String),
    /// The first object of another type (a symlink, fifo, socket or device),
    /// or a file that changed type between listing and opening.
    Foreign(PathBuf),
}

impl Observed {
    /// The observation an expected fingerprint (`None` for "nothing there")
    /// corresponds to.
    pub(super) fn expected(fingerprint: Option<&str>) -> Self {
        fingerprint.map_or(Self::Absent, |f| Self::Tree(f.to_owned()))
    }
}

/// Fingerprint whatever is at `root`. Any read error, or a file whose length
/// changes while it is read, is an error rather than an observation: a
/// fingerprint that might be wrong must never authorize a replacement.
pub(super) fn observe(root: &Path) -> std::io::Result<Observed> {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Observed::Absent),
        Err(e) => return Err(e),
    };
    let mut hasher = Sha256::new();
    hasher.update(FORMAT);
    if let Some(foreign) = walk(root, b"", &meta, &mut hasher)? {
        return Ok(Observed::Foreign(foreign));
    }
    Ok(Observed::Tree(hasher.finish().to_string()))
}

/// The lowercase-hex SHA-256 of `bytes`.
pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finish().to_string()
}

fn sorted_children(dir: &Path) -> std::io::Result<Vec<OsString>> {
    let mut names = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

fn header(hasher: &mut Sha256, rel: &[u8], kind: u8, mode: u32) {
    hasher.update(&(rel.len() as u64).to_le_bytes());
    hasher.update(rel);
    hasher.update(&[kind]);
    hasher.update(&mode.to_le_bytes());
}

fn walk(
    path: &Path,
    rel: &[u8],
    meta: &Metadata,
    hasher: &mut Sha256,
) -> std::io::Result<Option<PathBuf>> {
    let kind = meta.file_type();
    let mode = meta.permissions().mode() & 0o7777;
    if kind.is_file() {
        header(hasher, rel, b'f', mode);
        return hash_file(path, meta, hasher);
    }
    if !kind.is_dir() {
        return Ok(Some(path.to_path_buf()));
    }
    header(hasher, rel, b'd', mode);
    for name in sorted_children(path)? {
        let child = path.join(&name);
        let child_meta = std::fs::symlink_metadata(&child)?;
        let mut child_rel = rel.to_vec();
        if !child_rel.is_empty() {
            child_rel.push(b'/');
        }
        child_rel.extend_from_slice(name.as_bytes());
        if let Some(foreign) = walk(&child, &child_rel, &child_meta, hasher)? {
            return Ok(Some(foreign));
        }
    }
    Ok(None)
}

/// Hash a regular file's length and bytes. Opened without following a link
/// and without blocking, then checked to be the object that was listed, so a
/// fifo swapped in after the listing is reported, never read.
fn hash_file(
    path: &Path,
    listed: &Metadata,
    hasher: &mut Sha256,
) -> std::io::Result<Option<PathBuf>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let mut file = match file {
        Ok(file) => file,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(Some(path.to_path_buf())),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.dev() != listed.dev() || meta.ino() != listed.ino() {
        return Ok(Some(path.to_path_buf()));
    }
    let len = meta.len();
    hasher.update(&len.to_le_bytes());
    let mut buf = vec![0u8; 64 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len());
        let got = file.read(&mut buf[..want])?;
        if got == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("{} shrank while it was read", path.display()),
            ));
        }
        hasher.update(&buf[..got]);
        remaining -= got as u64;
    }
    if file.read(&mut buf[..1])? != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} grew while it was read", path.display()),
        ));
    }
    Ok(None)
}

fn io_at(what: &str, path: &Path, e: &std::io::Error) -> DevError {
    DevError::Io(std::io::Error::new(
        e.kind(),
        format!("{what} {}: {e}", path.display()),
    ))
}

/// Copy the payload `src` to the absent path `dst`: regular files and
/// directories only, permission bits carried over, every file and directory
/// fsynced. Directories are created owner-writable and get their final mode
/// only after their children are in, so a read-only source directory still
/// stages. A symlink or special file anywhere in `src` refuses the copy.
pub(super) fn stage(src: &Path, dst: &Path) -> Result<(), DevError> {
    let meta = std::fs::symlink_metadata(src).map_err(|e| io_at("cannot read", src, &e))?;
    let mode = meta.permissions().mode() & 0o7777;
    if meta.is_file() {
        let mut input = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(src)
            .map_err(|e| io_at("cannot open", src, &e))?;
        let opened = input
            .metadata()
            .map_err(|e| io_at("cannot inspect", src, &e))?;
        if !opened.is_file() || opened.dev() != meta.dev() || opened.ino() != meta.ino() {
            return Err(DevError::Refused(format!(
                "{} changed type while it was copied; an install payload holds regular files and \
                 directories only",
                src.display()
            )));
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dst)
            .map_err(|e| io_at("cannot create", dst, &e))?;
        std::io::copy(&mut input, &mut output).map_err(|e| io_at("cannot copy", src, &e))?;
        output
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| io_at("cannot set the mode of", dst, &e))?;
        output
            .sync_all()
            .map_err(|e| io_at("cannot fsync", dst, &e))?;
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(DevError::Refused(format!(
            "{} is a symlink or special file; an install payload holds regular files and \
             directories only",
            src.display()
        )));
    }
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(dst)
        .map_err(|e| io_at("cannot create", dst, &e))?;
    for name in sorted_children(src).map_err(|e| io_at("cannot list", src, &e))? {
        stage(&src.join(&name), &dst.join(&name))?;
    }
    // Opened while still owner-accessible: the final mode may take away the
    // read bit an open needs, and it is applied and synced through the
    // descriptor, so the mode the fingerprint records is durable too.
    let dir = std::fs::File::open(dst).map_err(|e| io_at("cannot open", dst, &e))?;
    dir.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(|e| io_at("cannot set the mode of", dst, &e))?;
    dir.sync_all().map_err(|e| io_at("cannot fsync", dst, &e))
}

/// Remove the tree at `path`, granting the owner write and search on each
/// directory first, since a staged read-only directory cannot have its
/// entries unlinked. Never follows a symlink. Resumable: whatever an earlier,
/// interrupted removal already took counts as done.
pub(super) fn remove_tree(path: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let gone = |r: std::io::Result<()>| match r {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    };
    if !meta.is_dir() {
        return gone(std::fs::remove_file(path));
    }
    gone(std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(0o700),
    ))?;
    for name in sorted_children(path)? {
        remove_tree(&path.join(name))?;
    }
    gone(std::fs::remove_dir(path))
}

/// fsync a directory, making the entries created or removed in it durable.
pub(super) fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        crate::test_scratch::scratch("install_step_tree", name)
    }

    fn tree_of(path: &Path) -> String {
        match observe(path).unwrap() {
            Observed::Tree(digest) => digest,
            other => panic!("expected a tree at {}, got {other:?}", path.display()),
        }
    }

    #[test]
    fn a_staged_copy_fingerprints_like_its_source_even_when_read_only() {
        let dir = scratch("staged_copy");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("assets")).unwrap();
        std::fs::write(src.join("SKILL.md"), "body").unwrap();
        std::fs::write(src.join("assets/hook.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            src.join("assets/hook.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(src.join("assets"), std::fs::Permissions::from_mode(0o555))
            .unwrap();

        let dst = dir.join("dst");
        let staged = stage(&src, &dst);
        let fingerprints = (observe(&src), observe(&dst));
        let hook = std::fs::metadata(dst.join("assets/hook.sh"));
        let removed = remove_tree(&dst);
        // Restored before any assertion, so a failure cannot leave a
        // read-only directory the next run's scratch reset trips over.
        std::fs::set_permissions(src.join("assets"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        staged.unwrap();
        let (src_fp, dst_fp) = (fingerprints.0.unwrap(), fingerprints.1.unwrap());
        assert!(matches!(src_fp, Observed::Tree(_)));
        assert_eq!(src_fp, dst_fp);
        assert_eq!(hook.unwrap().permissions().mode() & 0o777, 0o755);
        removed.unwrap();
        assert_eq!(observe(&dst).unwrap(), Observed::Absent);
    }

    #[test]
    fn content_mode_and_names_all_change_the_fingerprint() {
        let dir = scratch("fingerprint_inputs");
        let a = dir.join("a");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("f"), "x").unwrap();
        let base = tree_of(&a);

        std::fs::write(a.join("f"), "y").unwrap();
        let content = tree_of(&a);
        assert_ne!(base, content);

        std::fs::set_permissions(a.join("f"), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_ne!(content, tree_of(&a));

        std::fs::write(a.join("g"), "").unwrap();
        let added = tree_of(&a);
        std::fs::remove_file(a.join("g")).unwrap();
        assert_ne!(added, tree_of(&a));
    }

    #[test]
    fn a_lone_file_hashes_its_bytes() {
        let dir = scratch("lone_file");
        std::fs::write(dir.join("one"), "a").unwrap();
        std::fs::write(dir.join("two"), "b").unwrap();
        assert_ne!(tree_of(&dir.join("one")), tree_of(&dir.join("two")));
    }

    #[test]
    fn a_symlink_is_foreign_and_refuses_staging() {
        let dir = scratch("symlink");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", src.join("link")).unwrap();
        assert_eq!(observe(&src).unwrap(), Observed::Foreign(src.join("link")));
        assert!(stage(&src, &dir.join("dst")).is_err());
    }
}
