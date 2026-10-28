//! Publishing a copy step's payload, and recovering an interrupted one.
//!
//! A payload is staged in a private container beside the destination,
//! `.brokkr-install-<random>`, created exclusively with mode 0700, and swapped
//! in with one `renameat2`: `RENAME_EXCHANGE` over an existing destination,
//! `RENAME_NOREPLACE` onto an absent one. The container shares the
//! destination's directory, so the rename never crosses a mount, and the
//! payload sits one level inside it, so a consumer that lists the parent's
//! children for a manifest finds nothing there.
//!
//! The journal (see [`super::registry::Pending`]) runs
//! allocated -> preparing -> prepared -> retiring | aborting -> done, and each
//! phase is durable before the step it authorizes. Recovery reads the phase
//! and what is on disk, never the current config, so an install interrupted
//! under a step since removed is still finished.
//!
//! The assumptions, stated in `docs/commands/run.md`: the destination is
//! quiescent while it is replaced (a write through a descriptor already open
//! on the displaced object can escape the check that preserves it), and
//! nobody else creates or replaces a `.brokkr-install-*` name.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::registry::{DirId, Owner, Pending, Record, Registry};
use super::tree::{self, Observed};
use crate::error::DevError;
use crate::output;

const CONTAINER_PREFIX: &str = ".brokkr-install-";
const PAYLOAD: &str = "payload";

/// A resolved destination: its canonical parent and final name.
pub(super) struct Target<'a> {
    pub path: &'a Path,
    pub key: &'a str,
    pub parent: &'a Path,
    pub name: &'a std::ffi::OsStr,
}

fn cstr(bytes: &[u8]) -> std::io::Result<CString> {
    CString::new(bytes).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "nul in path"))
}

fn open_dir(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn fd(file: &std::fs::File) -> libc::c_int {
    use std::os::fd::AsRawFd as _;
    file.as_raw_fd()
}

fn mkdirat(parent: &std::fs::File, name: &str) -> std::io::Result<()> {
    let name = cstr(name.as_bytes())?;
    // SAFETY: a valid directory descriptor and a NUL-terminated name.
    let rc = unsafe { libc::mkdirat(fd(parent), name.as_ptr(), 0o700) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn renameat2(
    from_dir: &std::fs::File,
    from: &str,
    to_dir: &std::fs::File,
    to: &std::ffi::OsStr,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    let from = cstr(from.as_bytes())?;
    let to = cstr(to.as_bytes())?;
    // SAFETY: valid directory descriptors and NUL-terminated names.
    let rc = unsafe { libc::renameat2(fd(from_dir), from.as_ptr(), fd(to_dir), to.as_ptr(), flags) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

fn random_name() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    // SAFETY: the buffer is valid for its length.
    let got = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if usize::try_from(got).ok() != Some(bytes.len()) {
        return Err(std::io::Error::last_os_error());
    }
    Ok(format!("{CONTAINER_PREFIX}{:032x}", u128::from_le_bytes(bytes)))
}

fn dir_id(meta: &std::fs::Metadata) -> DirId {
    DirId { dev: meta.dev(), ino: meta.ino() }
}

fn failed(step: &str, msg: &str) -> DevError {
    DevError::Refused(format!("install step '{step}': {msg}"))
}

fn io(step: &str, what: &str, path: &Path, e: &std::io::Error) -> DevError {
    failed(step, &format!("{what} {}: {e}", path.display()))
}

/// The state of a transaction's container.
enum Container {
    Absent,
    /// The recorded directory, holding nothing but (at most) the payload.
    Ours,
    /// Something else: another object, or unexpected entries.
    Foreign(String),
}

fn inspect(path: &Path, id: DirId) -> std::io::Result<Container> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Container::Absent),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() || dir_id(&meta) != id {
        return Ok(Container::Foreign(format!("{} is not the container brokkr created", path.display())));
    }
    for entry in std::fs::read_dir(path)? {
        let name = entry?.file_name();
        if name != PAYLOAD {
            return Ok(Container::Foreign(format!(
                "{} holds {:?}, which brokkr did not put there",
                path.display(),
                name
            )));
        }
    }
    Ok(Container::Ours)
}

/// Publish `source` at `target` for `owner`. `prior` is the record's owner
/// before this transaction and `displaced` the admitted fingerprint of the
/// live destination (`None` when absent); admission has already established
/// that replacing it is allowed.
pub(super) fn install(
    registry: &Registry,
    step: &str,
    target: &Target<'_>,
    source: &Path,
    owner_of: impl Fn(String) -> Owner,
    prior: Option<&Owner>,
    displaced: Option<&str>,
) -> Result<(), DevError> {
    let parent = open_dir(target.parent).map_err(|e| io(step, "cannot open", target.parent, &e))?;
    let mut record = Record::new(target.key, prior.cloned());
    let store = |record: &Record| registry.store(record).map_err(|e| failed(step, &e.to_string()));
    let (container, id) = allocate(&store, &mut record, step, target, &parent, prior)?;
    let container_path = target.parent.join(&container);

    // Undo an unpublished transaction - unless the parent path no longer
    // names the directory the container was made in, when every path-based
    // step below would address the wrong place; the journal is then left
    // for recovery, which refuses what it cannot prove.
    let abort = |record: &mut Record, err: DevError| -> DevError {
        if !same_dir(&parent, target.parent) {
            return failed(
                step,
                &format!("{err}; {} was replaced, so the staging directory is left for recovery", target.parent.display()),
            );
        }
        match finish_cleanup(registry, record, &container_path, id, prior.cloned(), false) {
            Ok(()) => err,
            Err(cleanup) => failed(step, &format!("{err}; and the staging directory was not removed: {cleanup}")),
        }
    };

    // Prepare.
    let payload = container_path.join(PAYLOAD);
    if let Err(e) = tree::stage(source, &payload) {
        return Err(abort(&mut record, failed(step, &e.to_string())));
    }
    if let Err(e) = tree::fsync_dir(&container_path) {
        return Err(abort(&mut record, io(step, "cannot fsync", &container_path, &e)));
    }
    let staged = match tree::observe(&payload) {
        Ok(Observed::Tree(fingerprint)) => owner_of(fingerprint),
        Ok(other) => return Err(abort(&mut record, failed(step, &format!("the staged copy is not a tree: {other:?}")))),
        Err(e) => return Err(abort(&mut record, io(step, "cannot fingerprint", &payload, &e))),
    };
    record.pending = Some(Pending::Prepared {
        container: container.clone(),
        id,
        prior: prior.cloned(),
        staged: staged.clone(),
        displaced: displaced.map(str::to_owned),
    });
    store(&record)?;

    // Publish. The destination is checked again first; the exchange itself
    // cannot be made conditional on it, which is why the displaced object is
    // verified before anything deletes it.
    if !same_dir(&parent, target.parent) {
        return Err(abort(&mut record, failed(step, "the destination's parent was replaced")));
    }
    let expected = Observed::expected(displaced);
    match tree::observe(target.path) {
        Ok(live) if live == expected => {}
        Ok(_) => {
            return Err(abort(&mut record, failed(step, &format!("{} changed while the payload was staged", target.path.display()))));
        }
        Err(e) => return Err(abort(&mut record, io(step, "cannot fingerprint", target.path, &e))),
    }
    let container_dir = match open_dir(&container_path) {
        Ok(dir) if dir.metadata().is_ok_and(|m| dir_id(&m) == id) => dir,
        Ok(_) => return Err(failed(step, &format!("{} was replaced while staging", container_path.display()))),
        Err(e) => return Err(abort(&mut record, io(step, "cannot open", &container_path, &e))),
    };
    let flags = if displaced.is_some() { libc::RENAME_EXCHANGE } else { libc::RENAME_NOREPLACE };
    if let Err(e) = renameat2(&container_dir, PAYLOAD, &parent, target.name, flags) {
        return Err(abort(&mut record, io(step, "cannot publish at", target.path, &e)));
    }
    // From here on the destination may hold the new payload; a failure leaves
    // the prepared record for recovery rather than guessing.
    container_dir.sync_all().map_err(|e| io(step, "cannot fsync", &container_path, &e))?;
    parent.sync_all().map_err(|e| io(step, "cannot fsync", target.parent, &e))?;
    if !same_dir(&parent, target.parent) {
        return Err(failed(
            step,
            &format!("{} was replaced during publication; the install is left for recovery", target.parent.display()),
        ));
    }

    let displaced_now = tree::observe(&payload).map_err(|e| io(step, "cannot fingerprint", &payload, &e))?;
    if displaced_now != expected {
        return Err(failed(
            step,
            &format!(
                "the object displaced from {} is not the one brokkr installed - something changed it \
                 during the install. It is preserved at {}; inspect it, then remove {}, that directory \
                 and the record {} to install afresh",
                target.path.display(),
                payload.display(),
                target.path.display(),
                registry.file_for(target.key).display()
            ),
        ));
    }
    finish_cleanup(registry, &mut record, &container_path, id, Some(staged), true)
        .map_err(|e| failed(step, &format!("published, but cleanup failed: {e}")))
}

/// Create the private container: the intent is durable before the directory
/// exists, a name that already exists is never treated as ours, and the
/// container's entry is durable before its identity is journaled. Leaves the
/// record in the preparing phase.
fn allocate(
    store: &impl Fn(&Record) -> Result<(), DevError>,
    record: &mut Record,
    step: &str,
    target: &Target<'_>,
    parent: &std::fs::File,
    prior: Option<&Owner>,
) -> Result<(String, DirId), DevError> {
    let parent_meta = parent.metadata().map_err(|e| io(step, "cannot inspect", target.parent, &e))?;
    record.parent = Some(dir_id(&parent_meta));
    let mut container = None;
    for _ in 0..4 {
        let name = random_name().map_err(|e| failed(step, &format!("no random bytes: {e}")))?;
        record.pending = Some(Pending::Allocated { container: name.clone(), prior: prior.cloned() });
        store(record)?;
        match mkdirat(parent, &name) {
            Ok(()) => {
                container = Some(name);
                break;
            }
            Err(e) => {
                record.settle(prior.cloned());
                store(record)?;
                record.parent = Some(dir_id(&parent_meta));
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(io(step, "cannot create a staging directory in", target.parent, &e));
                }
            }
        }
    }
    let Some(container) = container else {
        return Err(failed(step, "four random staging names were all taken"));
    };
    let container_path = target.parent.join(&container);
    parent.sync_all().map_err(|e| io(step, "cannot fsync", target.parent, &e))?;
    if !same_dir(parent, target.parent) {
        return Err(failed(step, &format!("{} was replaced during the install", target.parent.display())));
    }
    let meta = std::fs::symlink_metadata(&container_path).map_err(|e| io(step, "cannot inspect", &container_path, &e))?;
    let id = dir_id(&meta);
    record.pending = Some(Pending::Preparing { container: container.clone(), id, prior: prior.cloned() });
    store(record)?;
    Ok((container, id))
}

/// `dir` (an open descriptor) is still what `path` names.
fn same_dir(dir: &std::fs::File, path: &Path) -> bool {
    match (dir.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Enter (or continue) a cleanup phase: remove the container, make that
/// durable, then commit `owner` (retiring) or restore it (aborting).
///
/// The container's identity and layout are checked before the phase is
/// written, so a durable cleanup phase always rests on a container brokkr
/// verified; once durable, the phase alone authorizes resuming a removal
/// that a crash cut short, and a container already gone counts as removed.
fn finish_cleanup(
    registry: &Registry,
    record: &mut Record,
    container_path: &Path,
    id: DirId,
    owner: Option<Owner>,
    retiring: bool,
) -> Result<(), DevError> {
    let container = container_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let phase = match (retiring, owner.clone()) {
        (true, Some(owner)) => Pending::Retiring { container, id, owner },
        (true, None) => return Err(DevError::Internal("a retiring install has no owner to commit".into())),
        (false, prior) => Pending::Aborting { container, id, prior },
    };
    let state = inspect(container_path, id).map_err(DevError::Io)?;
    if let Container::Foreign(why) = state {
        return Err(DevError::Refused(why));
    }
    if record.pending.as_ref() != Some(&phase) {
        record.pending = Some(phase);
        registry.store(record)?;
    }
    if matches!(state, Container::Ours) {
        tree::remove_tree(container_path).map_err(DevError::Io)?;
    }
    if let Some(parent) = container_path.parent() {
        tree::fsync_dir(parent).map_err(DevError::Io)?;
    }
    record.settle(owner);
    registry.store(record)
}

/// Finish or undo every interrupted install in the registry. A transaction
/// that cannot be resolved stays journaled and is reported; admission refuses
/// its destination until someone resolves it by hand.
///
/// A settled record is never pruned here, even when its destination is gone.
/// Ownership is content-based, so an unchanged artifact moved away and back
/// (a renamed parent, an unmounted filesystem, a restore) is still this
/// step's to replace; absence observed by some install cannot tell that from
/// abandonment, and the record costs a few hundred bytes per destination ever
/// used. Records are removed only by `Registry::store` when nothing is owned.
pub(super) fn recover_all(registry: &Registry) -> Result<(), DevError> {
    let scan = registry.all()?;
    for broken in &scan.broken {
        output::warn(&format!("{broken}; copy steps refuse until it is repaired or removed"));
    }
    for (file, mut record) in scan.records {
        if record.pending.is_none() {
            continue;
        }
        if let Err(why) = recover(registry, &mut record) {
            output::warn(&format!(
                "interrupted install of {} left unresolved: {why} (record {})",
                record.destination,
                file.display()
            ));
        }
    }
    Ok(())
}

fn recover(registry: &Registry, record: &mut Record) -> Result<(), String> {
    let Some(pending) = record.pending.clone() else { return Ok(()) };
    let destination = PathBuf::from(&record.destination);
    let parent = destination.parent().ok_or("the destination has no parent")?.to_path_buf();
    let container_path = parent.join(pending.container());
    // Every absence below means "gone" only while the parent path still
    // names the directory the transaction ran in; a container missing from
    // a replaced parent is elsewhere, so the journal is kept.
    let parent_now = std::fs::metadata(&parent).map(|m| dir_id(&m)).ok();
    if parent_now.is_none() || parent_now != record.parent {
        return Err(format!(
            "{} is missing or is not the directory the install ran in, so its staging directory cannot be found",
            parent.display()
        ));
    }
    let finish = |record: &mut Record, id, owner, retiring| {
        finish_cleanup(registry, record, &container_path, id, owner, retiring).map_err(|e| e.to_string())
    };
    match pending {
        Pending::Allocated { prior, .. } => {
            // Only an empty directory can be ours here: nothing is copied in
            // before the preparing phase is durable. Its removal - or its
            // absence - is made durable before the intent is cleared.
            match std::fs::remove_dir(&container_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => output::warn(&format!(
                    "left {} in place: it is not empty, so brokkr did not create it as it stands",
                    container_path.display()
                )),
                Err(e) => return Err(format!("cannot remove {}: {e}", container_path.display())),
            }
            tree::fsync_dir(&parent).map_err(|e| format!("cannot fsync {}: {e}", parent.display()))?;
            record.settle(prior);
            registry.store(record).map_err(|e| e.to_string())
        }
        // Nothing is published before the prepared phase, so the destination
        // is untouched and undoing is right; a container that is gone (from
        // the verified parent) leaves nothing to remove, which the cleanup
        // makes durable before restoring the prior owner.
        Pending::Preparing { id, prior, .. } => finish(record, id, prior, false),
        Pending::Aborting { id, prior, .. } => finish(record, id, prior, false),
        Pending::Retiring { id, owner, .. } => finish(record, id, Some(owner), true),
        Pending::Prepared { id, prior, staged, displaced, .. } => {
            // Both outcomes need their full evidence: the container with the
            // object the other side of the rename left in it. The namespace
            // just observed is made durable before a cleanup phase acts on it.
            match inspect(&container_path, id).map_err(|e| e.to_string())? {
                Container::Ours => {}
                Container::Foreign(why) => return Err(why),
                Container::Absent => {
                    return Err(format!("{} is gone, so neither outcome can be proven", container_path.display()));
                }
            }
            tree::fsync_dir(&parent).map_err(|e| format!("cannot fsync {}: {e}", parent.display()))?;
            tree::fsync_dir(&container_path).map_err(|e| format!("cannot fsync {}: {e}", container_path.display()))?;
            let live = tree::observe(&destination).map_err(|e| e.to_string())?;
            let in_container = tree::observe(&container_path.join(PAYLOAD)).map_err(|e| e.to_string())?;
            let new = Observed::Tree(staged.fingerprint.clone());
            let old = Observed::expected(displaced.as_deref());
            if live == new && in_container == old {
                finish(record, id, Some(staged), true)
            } else if live == old && in_container == new {
                finish(record, id, prior, false)
            } else {
                Err(format!(
                    "neither the published nor the original state is on disk at {} and {}",
                    destination.display(),
                    container_path.display()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    struct Fixture {
        registry: Registry,
        source: PathBuf,
        parent: PathBuf,
        dest: PathBuf,
        key: String,
    }

    fn fixture(name: &str) -> Fixture {
        let root = crate::test_scratch::scratch("install_step_transaction", name);
        let source = root.join("src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "v1").unwrap();
        let parent = root.join("skills");
        std::fs::create_dir_all(&parent).unwrap();
        let dest = parent.join("mod");
        let key = dest.to_str().unwrap().to_owned();
        Fixture { registry: Registry::at(root.join("state/install")), source, parent, dest, key }
    }

    fn owner(fingerprint: String) -> Owner {
        Owner { project: "p".into(), step: "s".into(), fingerprint }
    }

    fn run(f: &Fixture, prior: Option<&Owner>, displaced: Option<&str>) -> Result<(), DevError> {
        let target = Target { path: &f.dest, key: &f.key, parent: &f.parent, name: f.dest.file_name().unwrap() };
        install(&f.registry, "s", &target, &f.source, owner, prior, displaced)
    }

    fn fingerprint(path: &Path) -> String {
        match tree::observe(path).unwrap() {
            Observed::Tree(f) => f,
            other => panic!("{other:?}"),
        }
    }

    fn siblings(f: &Fixture) -> Vec<String> {
        std::fs::read_dir(&f.parent).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn a_fresh_install_then_a_replacement_leave_only_the_destination() {
        let f = fixture("fresh_then_replace");
        run(&f, None, None).unwrap();
        assert_eq!(std::fs::read_to_string(f.dest.join("SKILL.md")).unwrap(), "v1");
        let first = f.registry.load(&f.key).unwrap().unwrap();
        assert_eq!(first.pending, None);
        let p = first.owner.clone().unwrap();
        assert_eq!(p.fingerprint, fingerprint(&f.dest));

        std::fs::write(f.source.join("SKILL.md"), "v2").unwrap();
        std::fs::remove_file(f.source.join("SKILL.md")).unwrap();
        std::fs::write(f.source.join("NEW.md"), "v2").unwrap();
        run(&f, Some(&p), Some(&p.fingerprint)).unwrap();
        assert!(!f.dest.join("SKILL.md").exists(), "a file removed from the source is gone");
        assert_eq!(std::fs::read_to_string(f.dest.join("NEW.md")).unwrap(), "v2");
        assert_eq!(siblings(&f), vec!["mod".to_owned()]);
        let second = f.registry.load(&f.key).unwrap().unwrap();
        assert_eq!(second.owner.unwrap().fingerprint, fingerprint(&f.dest));
    }

    #[test]
    fn a_destination_changed_after_admission_is_not_replaced() {
        let f = fixture("changed_after_admission");
        run(&f, None, None).unwrap();
        let p = f.registry.load(&f.key).unwrap().unwrap().owner.unwrap();
        std::fs::write(f.dest.join("mine.txt"), "user edit").unwrap();
        assert!(run(&f, Some(&p), Some(&p.fingerprint)).is_err());
        assert_eq!(std::fs::read_to_string(f.dest.join("mine.txt")).unwrap(), "user edit");
        assert_eq!(siblings(&f), vec!["mod".to_owned()], "the aborted container is cleaned up");
        let record = f.registry.load(&f.key).unwrap().unwrap();
        assert_eq!(record.owner, Some(p), "the prior owner is restored");
        assert_eq!(record.pending, None);
    }

    #[test]
    fn recovery_completes_a_published_install_whose_cleanup_was_cut_short() {
        let f = fixture("recover_retiring");
        run(&f, None, None).unwrap();
        let p = f.registry.load(&f.key).unwrap().unwrap().owner.unwrap();
        // Simulate a crash after the exchange: the old tree sits in a
        // container and the record still says prepared.
        let container = f.parent.join(format!("{CONTAINER_PREFIX}feed"));
        std::fs::create_dir(&container).unwrap();
        tree::stage(&f.source, &container.join(PAYLOAD)).unwrap();
        std::fs::write(f.source.join("SKILL.md"), "v2").unwrap();
        let old = f.dest.clone();
        let staged_tmp = f.parent.join("v2");
        tree::stage(&f.source, &staged_tmp).unwrap();
        let s = fingerprint(&staged_tmp);
        tree::remove_tree(&old).unwrap();
        std::fs::rename(&staged_tmp, &old).unwrap();
        let id = dir_id(&std::fs::symlink_metadata(&container).unwrap());
        journal(&f, Some(p.clone()), Pending::Prepared {
            container: format!("{CONTAINER_PREFIX}feed"),
            id,
            prior: Some(p.clone()),
            staged: owner(s.clone()),
            displaced: Some(p.fingerprint.clone()),
        });

        recover_all(&f.registry).unwrap();
        let done = f.registry.load(&f.key).unwrap().unwrap();
        assert_eq!(done.pending, None);
        assert_eq!(done.owner.unwrap().fingerprint, s);
        assert_eq!(siblings(&f), vec!["mod".to_owned()]);
    }

    #[test]
    fn recovery_preserves_a_container_that_is_not_ours() {
        let f = fixture("recover_foreign");
        let container = f.parent.join(format!("{CONTAINER_PREFIX}beef"));
        std::fs::create_dir(&container).unwrap();
        std::fs::write(container.join("notes.txt"), "keep").unwrap();
        journal(&f, None, Pending::Retiring {
            container: format!("{CONTAINER_PREFIX}beef"),
            id: DirId { dev: 0, ino: 0 },
            owner: owner("00".into()),
        });
        recover_all(&f.registry).unwrap();
        assert!(container.join("notes.txt").exists());
        assert!(f.registry.load(&f.key).unwrap().unwrap().pending.is_some());
    }

    #[test]
    fn an_allocated_container_is_removed_only_when_empty() {
        let f = fixture("recover_allocated");
        let container = f.parent.join(format!("{CONTAINER_PREFIX}cafe"));
        std::fs::create_dir(&container).unwrap();
        std::fs::write(container.join("x"), "").unwrap();
        journal(&f, None, Pending::Allocated { container: format!("{CONTAINER_PREFIX}cafe"), prior: None });
        recover_all(&f.registry).unwrap();
        assert!(container.join("x").exists(), "a non-empty directory is left alone");
        assert_eq!(f.registry.load(&f.key).unwrap(), None);
    }

    #[test]
    fn a_replaced_parent_keeps_the_journal() {
        let f = fixture("recover_moved_parent");
        let container = f.parent.join(format!("{CONTAINER_PREFIX}d00d"));
        std::fs::create_dir(&container).unwrap();
        let id = dir_id(&std::fs::symlink_metadata(&container).unwrap());
        journal(&f, None, Pending::Preparing { container: format!("{CONTAINER_PREFIX}d00d"), id, prior: None });
        // The parent moves away and a new one takes its path: the container
        // is missing from the path, not gone.
        let moved = f.parent.with_file_name("skills.old");
        std::fs::rename(&f.parent, &moved).unwrap();
        std::fs::create_dir(&f.parent).unwrap();
        recover_all(&f.registry).unwrap();
        assert!(f.registry.load(&f.key).unwrap().unwrap().pending.is_some());
        assert!(moved.join(format!("{CONTAINER_PREFIX}d00d")).is_dir());
    }

    /// Store a journaled record for `f`'s destination, its parent identity
    /// taken as the transaction would have.
    fn journal(f: &Fixture, owner: Option<Owner>, pending: Pending) {
        let mut record = Record::new(&f.key, owner);
        record.pending = Some(pending);
        record.parent = Some(dir_id(&std::fs::metadata(&f.parent).unwrap()));
        f.registry.store(&record).unwrap();
    }
}
