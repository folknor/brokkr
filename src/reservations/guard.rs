//! The mutation gate: every brokkr deletion, rename or replacement that can
//! touch a path a pbfhogg launch uses goes through here.
//!
//! Each operation takes the process-wide reservation mutex, scans the store
//! (reclaiming free leases on the way), and refuses when any reservation's
//! resource overlaps an operand, whatever its access: a deletion is a write,
//! and a live reader's input is as protected as a writer's output. A lease
//! whose resources cannot be read blocks every operation, and so does an
//! operand whose identity cannot be resolved. A recursive removal also
//! refuses a tree holding a `.pbfhogg-backup-*` entry, which can be the only
//! copy of a replaced output. The mutex is held through the operation itself,
//! so no launch in this process is admitted between the check and the act.
//!
//! The functions return `io::Result` like the `std::fs` calls they replace; a
//! refusal is an `io::Error` whose message names the reservation and which
//! [`is_refusal`] recognizes (its kind is [`io::ErrorKind::ResourceBusy`], but a
//! genuine `EBUSY` has that kind too), so a caller that sweeps many targets
//! (`brokkr clean`) can skip one and go on. [`mutate`] runs a caller's own
//! mutation - several removals, a `git worktree remove` - under the same
//! refusals with the mutex held through it.

use std::io;
use std::path::{Path, PathBuf};

use super::paths::Spelled;

/// The name prefix of the backup pbfhogg keeps beside a replaced output.
pub(crate) const BACKUP_PREFIX: &str = ".pbfhogg-backup-";

pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    gated(&[path], None, || std::fs::remove_file(path))
}

pub(crate) fn remove_dir_all(path: &Path) -> io::Result<()> {
    gated(&[path], Some(path), || std::fs::remove_dir_all(path))
}

/// `rename(from, to)`, checking both operands.
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    gated(&[from, to], None, || std::fs::rename(from, to))
}

/// Write `contents` to `path`, replacing it: a file a child reads, such as a
/// generated extract config.
pub(crate) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    gated(&[path], None, || std::fs::write(path, contents))
}

/// Run `op`, a mutation made some other way (several removals, a `git
/// worktree remove`), under the gate: refused as the gate refuses for
/// `targets`, and for each of `trees` removed recursively, with the
/// reservation mutex held through all of `op`.
pub(crate) fn mutate<T, E: From<io::Error>>(
    targets: &[&Path],
    trees: &[&Path],
    op: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let _serial = super::serialize()?;
    refuse(targets, trees)?;
    op()
}

/// The payload of a refusal, so [`is_refusal`] tells one from a genuine
/// `EBUSY`, which maps to the same [`io::ErrorKind`].
#[derive(Debug)]
struct Refusal(String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

pub(super) fn busy(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::ResourceBusy, Refusal(message))
}

/// Whether `error` is the reservation gate's refusal, not an I/O failure.
pub(crate) fn is_refusal(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Refusal>)
}

fn gated<T>(
    targets: &[&Path],
    tree: Option<&Path>,
    op: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let _serial = super::serialize()?;
    refuse(targets, tree.as_slice())?;
    op()
}

/// The refusals of every gated operation, under the caller's mutex.
pub(super) fn refuse(targets: &[&Path], trees: &[&Path]) -> io::Result<()> {
    if let Some(store) = super::store() {
        refuse_overlaps(&store.scan(), targets)?;
    }
    for tree in trees {
        let backup = find_backup(tree).map_err(|e| {
            busy(format!(
                "refusing to remove {}: it could not be searched for a pbfhogg backup: {e}",
                tree.display()
            ))
        })?;
        if let Some(backup) = backup {
            return Err(busy(format!(
                "refusing to remove {}: it holds {}, which pbfhogg kept as the previous copy of \
                 an output; look at it and remove it by hand",
                tree.display(),
                backup.display()
            )));
        }
    }
    Ok(())
}

/// Refuse when the scan has an unknown lease or a reservation overlapping a
/// target, or a target cannot be resolved.
pub(crate) fn refuse_overlaps(scan: &super::store::Scan, targets: &[&Path]) -> io::Result<()> {
    if let Some((id, why)) = scan.unknown.first() {
        return Err(busy(format!(
            "lease {id} in the launch lease store cannot be established ({why}); nothing it might \
             protect can be changed until it is resolved"
        )));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    for target in targets {
        let target = Spelled::of(target, &cwd).map_err(|e| {
            busy(format!(
                "refusing to change {}: its identity cannot be resolved: {e}",
                target.display()
            ))
        })?;
        for reservation in &scan.reservations {
            if let Some(resource) = reservation
                .resources
                .iter()
                .find(|r| r.spelled().overlaps(&target))
            {
                let state = match &reservation.retained {
                    None => "is held by a process still running".to_owned(),
                    Some(why) => format!("is kept because its cleanup failed: {why}"),
                };
                return Err(busy(format!(
                    "refusing to change {}: it overlaps {}, which lease {} {state}",
                    target.lexical.display(),
                    resource.lexical.display(),
                    reservation.id
                )));
            }
        }
    }
    Ok(())
}

/// The first `.pbfhogg-backup-*` entry at or under `root`. A root that does
/// not exist holds none; any other error is returned, since an unread
/// directory may hold one.
fn find_backup(root: &Path) -> io::Result<Option<PathBuf>> {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if is_backup(root) {
        return Ok(Some(root.to_path_buf()));
    }
    if !meta.is_dir() {
        return Ok(None);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if is_backup(&path) {
                return Ok(Some(path));
            }
            if entry.file_type()?.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(None)
}

/// By the name's bytes: a backup of an output with a non-UTF-8 name is a
/// backup all the same.
fn is_backup(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    path.file_name()
        .is_some_and(|name| name.as_bytes().starts_with(BACKUP_PREFIX.as_bytes()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::super::store::{Access, Disposition, Reservation, Resource, Scan};
    use super::*;

    #[test]
    fn a_reservation_blocks_its_paths_their_ancestors_and_their_contents() {
        let root = crate::test_scratch::scratch("reservations", "guard-check");
        let input = root.join("data/in.pbf");
        let scan = Scan {
            reservations: vec![Reservation {
                id: "x".into(),
                resources: vec![Resource::new(
                    &Spelled::of(&input, &root).unwrap(),
                    Access::Read,
                    Disposition::Reserve,
                )],
                retained: None,
            }],
            unknown: Vec::new(),
        };
        let refused = refuse_overlaps(&scan, &[&input]).unwrap_err();
        assert!(is_refusal(&refused), "{refused}");
        assert!(
            !is_refusal(&io::Error::from_raw_os_error(libc::EBUSY)),
            "a real EBUSY is no refusal"
        );
        assert!(
            refuse_overlaps(&scan, &[&root.join("data")]).is_err(),
            "an enclosing directory"
        );
        assert!(
            refuse_overlaps(&scan, &[&root.join("data/in.pbf/x")]).is_err(),
            "a path inside it"
        );
        assert!(
            refuse_overlaps(&scan, &[&root.join("other.pbf")]).is_ok(),
            "an unrelated path"
        );
        let unknown = Scan {
            reservations: Vec::new(),
            unknown: vec![("y".into(), "unreadable".into())],
        };
        assert!(
            refuse_overlaps(&unknown, &[&root.join("other.pbf")]).is_err(),
            "an unknown lease blocks all"
        );
    }

    #[test]
    fn a_tree_holding_a_backup_is_never_removed() {
        let root = crate::test_scratch::scratch("reservations", "guard-backup");
        let tree = root.join("multi-extract");
        std::fs::create_dir_all(tree.join("a")).unwrap();
        std::fs::write(
            tree.join("a").join(format!("{BACKUP_PREFIX}1-0.out.pbf")),
            b"",
        )
        .unwrap();
        let err = remove_dir_all(&tree).unwrap_err();
        assert!(is_refusal(&err), "{err}");
        assert!(err.to_string().contains(BACKUP_PREFIX), "{err}");
        assert!(tree.exists());
        std::fs::remove_dir_all(tree.join("a")).unwrap();
        remove_dir_all(&tree).unwrap();
        assert!(!tree.exists());
    }

    #[test]
    fn a_backup_with_a_non_utf8_name_is_still_a_backup() {
        use std::os::unix::ffi::OsStrExt as _;
        let root = crate::test_scratch::scratch("reservations", "guard-backup-bytes");
        let mut name = BACKUP_PREFIX.as_bytes().to_vec();
        name.extend_from_slice(b"1-0.\xff.pbf");
        let tree = root.join("t");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(std::ffi::OsStr::from_bytes(&name)), b"").unwrap();
        assert!(remove_dir_all(&tree).is_err());
        assert!(tree.exists());
    }
}
