//! The lease store: per launch a lock file whose flock proves some process
//! can still use the launch's resources, and a durable record naming them.
//!
//! `<id>.lock` is created exclusively and flocked by the launching brokkr;
//! the child inherits that open file description, so the flock lives as long
//! as any process holding it, in whatever PID namespace. `<id>.record` is the
//! versioned JSON naming every resource with its access and disposition. Both
//! are made durable (file and directory fsync) before anything they describe
//! exists, so after a crash, even a power loss, no private directory exists
//! without a record naming it. A lock that an independent open can take
//! exclusively has no holder left; only then is a record's private directory
//! removed, and only the private ones: read and write resources are
//! reservation subjects, never deleted here.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::paths::Spelled;

/// Bumped whenever the record's meaning changes; an unknown version reads as
/// an unknown reservation, never as an empty one.
const RECORD_VERSION: u32 = 1;

const LOCK_EXT: &str = "lock";
const RECORD_EXT: &str = "record";
const TMP_EXT: &str = "record.tmp";
/// A lock file between its creation and its flock. Renamed to [`LOCK_EXT`]
/// once locked, so no scan ever sees a final-named lock its creator does not
/// hold yet (the scheme `test_orphans` uses for its registrations).
const PENDING_EXT: &str = "pending";

/// How long a critical section waits for another process's to end.
const SERIAL_WAIT: Duration = Duration::from_secs(120);

/// How old an unlocked pending lock must be before a scan removes it. Its
/// window is a few syscalls long; a scan that removed one still in it would
/// only make that creation fail at its rename, never succeed unprotected.
const PENDING_GRACE: Duration = Duration::from_secs(60);

/// How long a release waits for the lease to become free after the child was
/// reaped: an unrelated child forked in that window holds the description
/// until its own exec closes it.
const RELEASE_WINDOW: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Disposition {
    /// Protected while the lease lives; never removed by reclamation.
    Reserve,
    /// A directory the launch created; removed once the lease is free.
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Resource {
    pub(crate) lexical: PathBuf,
    pub(crate) resolved: PathBuf,
    pub(crate) access: Access,
    pub(crate) disposition: Disposition,
    /// `(st_dev, st_ino)` of a private directory, once it exists.
    #[serde(default)]
    pub(crate) identity: Option<(u64, u64)>,
    /// The symlinks the path's resolution goes through.
    #[serde(default)]
    pub(crate) via: Vec<PathBuf>,
}

impl Resource {
    pub(crate) fn new(path: &Spelled, access: Access, disposition: Disposition) -> Self {
        Self {
            lexical: path.lexical.clone(),
            resolved: path.resolved.clone(),
            access,
            disposition,
            identity: None,
            via: path.via.clone(),
        }
    }

    pub(crate) fn spelled(&self) -> Spelled {
        Spelled::recorded(
            self.lexical.clone(),
            self.resolved.clone(),
            self.via.clone(),
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    version: u32,
    resources: Vec<Resource>,
}

/// What a scan found besides the dead leases it reclaimed.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Known resources that must not be touched: a held lease, or a free one
    /// whose reclamation failed (kept with the reason).
    pub(crate) reservations: Vec<Reservation>,
    /// Leases whose resources cannot be established: they block everything.
    pub(crate) unknown: Vec<(String, String)>,
}

#[derive(Debug)]
pub(crate) struct Reservation {
    pub(crate) id: String,
    pub(crate) resources: Vec<Resource>,
    /// `None` for a held lease; the reason for one kept after its
    /// reclamation failed.
    pub(crate) retained: Option<String>,
}

/// The store in one directory.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The per-user store, beside the global lock and the compile lease.
    /// Unit tests use a scratch store instead (`super::store`).
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn user() -> Option<Self> {
        crate::user_dirs::brokkr_state_dir().map(|dir| Self::at(dir.join("launch-leases")))
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, id: &str, ext: &str) -> PathBuf {
        self.root.join(format!("{id}.{ext}"))
    }

    /// The store's serial lock, flocked exclusively on return: `.serial`, a
    /// name no scan parses as a lease. Waits at most [`SERIAL_WAIT`] for
    /// another process's critical section (a long gated removal, a release
    /// waiting out its window), then fails rather than hang outside every
    /// time ceiling. A store directory made here is made durably.
    pub(crate) fn serial_lock(&self) -> io::Result<File> {
        if !self.root.is_dir() {
            self.ensure_root()?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(self.root.join(".serial"))?;
        let deadline = Instant::now() + SERIAL_WAIT;
        loop {
            if try_exclusive(&file)? {
                return Ok(file);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "another brokkr process held the launch lease store's serial lock for {}s",
                    SERIAL_WAIT.as_secs()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Make the store directory exist durably: its entry in its parent is
    /// synced on every creation of a lease, not only the call that made it,
    /// since an earlier creation may have died before its sync, and a power
    /// loss must not lose the store while a private directory it names
    /// survives on another filesystem.
    fn ensure_root(&self) -> io::Result<()> {
        // Every directory from the store up to the first one that already
        // existed is new and needs its entry synced in its parent; the first
        // existing one is synced too, for a creation an earlier call made
        // without finishing its syncs.
        let mut first_existing = self.root.as_path();
        while !first_existing.is_dir() {
            match first_existing.parent() {
                Some(parent) => first_existing = parent,
                None => break,
            }
        }
        std::fs::create_dir_all(&self.root)?;
        let mut dir = self.root.as_path();
        loop {
            sync_dir(dir)?;
            if dir == first_existing {
                break;
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
        match first_existing.parent() {
            Some(parent) => sync_dir(parent),
            None => Ok(()),
        }
    }

    /// Create a lease holding `resources`: the lock file, created under a
    /// pending name, flocked, then renamed to its final name, then the
    /// record, all durable on return.
    pub(crate) fn create(&self, resources: Vec<Resource>) -> io::Result<Lease> {
        self.ensure_root()?;
        let id = crate::lockfile::RunToken::mint()
            .ok_or_else(|| io::Error::other("no entropy for a lease id"))?
            .as_str()
            .to_owned();
        let pending = self.path(&id, PENDING_EXT);
        let lock_path = self.path(&id, LOCK_EXT);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&pending)?;
        if !try_exclusive(&file)? {
            return Err(io::Error::other(format!(
                "a freshly created lease {} is locked by someone else",
                pending.display()
            )));
        }
        // A scan that removed the pending file meanwhile makes this fail, so
        // a lease is never published on an inode no scan can open.
        std::fs::rename(&pending, &lock_path)?;
        sync_dir(&self.root)?;
        let lease = Lease {
            store: self.clone(),
            id,
            file: Some(file),
            resources,
        };
        if let Err(e) = lease.publish() {
            // Nothing the record would describe exists yet. A record that did
            // reach its name - or may have, when it cannot be inspected - stays
            // with its lock, for the next scan to reclaim; only a lease whose
            // record is certainly absent loses its lock file here.
            if absent(&self.path(&lease.id, RECORD_EXT)) {
                drop(std::fs::remove_file(&lock_path));
            }
            return Err(e);
        }
        Ok(lease)
    }

    /// Reclaim every free lease and report the rest. An absent store holds
    /// none; one that cannot be read is an unknown reservation.
    pub(crate) fn scan(&self) -> Scan {
        let mut scan = Scan::default();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return scan,
            Err(e) => {
                scan.unknown
                    .push(("*".into(), format!("the lease store cannot be read: {e}")));
                return scan;
            }
        };
        let mut ids: Vec<String> = Vec::new();
        let mut stray_records: Vec<String> = Vec::new();
        let mut temps: Vec<String> = Vec::new();
        let mut pendings: Vec<String> = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    scan.unknown
                        .push(("*".into(), format!("the lease store cannot be listed: {e}")));
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((id, ext)) = name.split_once('.') else {
                continue;
            };
            if crate::lockfile::RunToken::parse(id).is_none() {
                continue;
            }
            match ext {
                LOCK_EXT => ids.push(id.to_owned()),
                RECORD_EXT => stray_records.push(id.to_owned()),
                TMP_EXT => temps.push(id.to_owned()),
                PENDING_EXT => pendings.push(id.to_owned()),
                _ => {}
            }
        }
        for id in pendings {
            self.reclaim_pending(&id);
        }
        for id in &ids {
            self.scan_one(id, &mut scan);
        }
        // A record outlives its lock only outside this protocol: unknown.
        for id in stray_records {
            if !ids.contains(&id) && !absent(&self.path(&id, RECORD_EXT)) {
                scan.unknown
                    .push((id, "a lease record without its lock file".into()));
            }
        }
        // A temporary record is written only while its lock is held, so one
        // without a lock is a crash during publication of a lease that no
        // longer exists; a lock that cannot be inspected keeps it.
        for id in temps {
            if absent(&self.path(&id, LOCK_EXT)) && absent(&self.path(&id, PENDING_EXT)) {
                drop(std::fs::remove_file(self.path(&id, TMP_EXT)));
            }
        }
        scan
    }

    /// Remove a pending lock abandoned by a creation that died before its
    /// rename: past the grace period and lockable. Nothing ever held it under
    /// its final name, so no record or directory can depend on it.
    fn reclaim_pending(&self, id: &str) {
        let path = self.path(id, PENDING_EXT);
        let old = std::fs::symlink_metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= PENDING_GRACE);
        if old
            && let Ok(file) = open_lock(&path)
            && matches!(try_exclusive(&file), Ok(true))
        {
            drop(std::fs::remove_file(&path));
        }
    }

    fn scan_one(&self, id: &str, scan: &mut Scan) {
        let lock_path = self.path(id, LOCK_EXT);
        let file = match open_lock(&lock_path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(e) => {
                scan.unknown
                    .push((id.to_owned(), format!("its lock cannot be opened: {e}")));
                return;
            }
        };
        match try_exclusive(&file) {
            Ok(true) => match self.read_record(id) {
                Ok(None) => {
                    // Never published: nothing it would name was created.
                    drop(std::fs::remove_file(self.path(id, TMP_EXT)));
                    if let Err(e) = remove_synced(&lock_path, &self.root) {
                        scan.unknown
                            .push((id.to_owned(), format!("its lock cannot be removed: {e}")));
                    }
                }
                Ok(Some(resources)) => {
                    if let Err(e) = self.reclaim(id, &resources) {
                        scan.reservations.push(Reservation {
                            id: id.to_owned(),
                            resources,
                            retained: Some(e.to_string()),
                        });
                    }
                }
                Err(e) => scan
                    .unknown
                    .push((id.to_owned(), format!("its record is unreadable: {e}"))),
            },
            Ok(false) => match self.read_record(id) {
                Ok(Some(resources)) => scan.reservations.push(Reservation {
                    id: id.to_owned(),
                    resources,
                    retained: None,
                }),
                Ok(None) => scan
                    .unknown
                    .push((id.to_owned(), "a held lease not yet recorded".into())),
                Err(e) => scan
                    .unknown
                    .push((id.to_owned(), format!("its record is unreadable: {e}"))),
            },
            Err(e) => scan
                .unknown
                .push((id.to_owned(), format!("its lock cannot be tested: {e}"))),
        }
        drop(file);
    }

    /// The record's resources; `None` only when it does not exist.
    fn read_record(&self, id: &str) -> io::Result<Option<Vec<Resource>>> {
        let text = match std::fs::read_to_string(self.path(id, RECORD_EXT)) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let record: Record = serde_json::from_str(&text).map_err(io::Error::other)?;
        if record.version != RECORD_VERSION {
            return Err(io::Error::other(format!(
                "record version {}",
                record.version
            )));
        }
        Ok(Some(record.resources))
    }

    /// Remove a free lease: its private directories, then the record, then
    /// the lock file. Any failure keeps what is left, so a later scan retries.
    fn reclaim(&self, id: &str, resources: &[Resource]) -> io::Result<()> {
        for resource in resources
            .iter()
            .filter(|r| r.disposition == Disposition::Private)
        {
            remove_private(resource)?;
        }
        remove_synced(&self.path(id, RECORD_EXT), &self.root)?;
        remove_synced(&self.path(id, LOCK_EXT), &self.root)
    }
}

/// One launch's lease, held by this process until [`Lease::release`].
#[derive(Debug)]
pub(crate) struct Lease {
    store: Store,
    id: String,
    file: Option<File>,
    resources: Vec<Resource>,
}

impl Lease {
    #[cfg(test)]
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    #[cfg(test)]
    pub(crate) fn close_own_for_test(&mut self) {
        drop(self.file.take());
    }

    /// The descriptor a child inherits.
    pub(crate) fn fd(&self) -> Option<std::os::fd::RawFd> {
        self.file.as_ref().map(AsRawFd::as_raw_fd)
    }

    pub(crate) fn resources(&self) -> &[Resource] {
        &self.resources
    }

    /// Record the identity of the private directory at `index`, durably.
    pub(crate) fn set_identity(&mut self, index: usize, identity: (u64, u64)) -> io::Result<()> {
        if let Some(resource) = self.resources.get_mut(index) {
            resource.identity = Some(identity);
        }
        self.publish()
    }

    /// Drop the resource at `index` from the record, durably: a private
    /// directory whose creation failed, which this lease must never remove.
    pub(crate) fn forget(&mut self, index: usize) -> io::Result<()> {
        if index < self.resources.len() {
            self.resources.remove(index);
        }
        self.publish()
    }

    fn publish(&self) -> io::Result<()> {
        let record = Record {
            version: RECORD_VERSION,
            resources: self.resources.clone(),
        };
        let text = serde_json::to_vec(&record).map_err(io::Error::other)?;
        let tmp = self.store.path(&self.id, TMP_EXT);
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(&text)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, self.store.path(&self.id, RECORD_EXT))?;
        sync_dir(&self.store.root)
    }

    /// Give up this process's hold and, once no process holds the lease any
    /// more, remove it with its private directories. A lease still held after
    /// the release window stays recorded, a live reservation, and is the
    /// error.
    pub(crate) fn release(mut self) -> io::Result<()> {
        drop(self.file.take());
        let lock_path = self.store.path(&self.id, LOCK_EXT);
        let deadline = Instant::now() + RELEASE_WINDOW;
        loop {
            let file = match open_lock(&lock_path) {
                Ok(file) => file,
                // Another process's scan found the lease free once this one
                // gave up its hold, and reclaimed it: nothing is left to do.
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            if try_exclusive(&file)? {
                let result = self.store.reclaim(&self.id, &self.resources);
                drop(file);
                return result;
            }
            drop(file);
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "lease {} is still held after its child exited: some process kept its \
                     descriptor, so its private directories are kept",
                    lock_path.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn open_lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

/// `flock(LOCK_EX | LOCK_NB)` on `file`'s own description: `Ok(false)` when
/// someone else holds it.
fn try_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: an owned open descriptor, no pointers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Whether `path` is certainly absent: only `NotFound` says so, any other
/// inspection error reads as present.
fn absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn remove_synced(path: &Path, dir: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_dir(dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove one recorded private resource - the launch's private directory,
/// or a disposable file its launcher named (a profiler's report) - where the
/// kernel put it (the resolved spelling): gone already is fine; one whose
/// identity differs from the recorded one, or that is neither a plain
/// directory nor a regular file, is not this lease's and is refused. The
/// parent is synced in both cases, so a retry after a failed sync completes
/// it rather than taking the absence as durable.
fn remove_private(resource: &Resource) -> io::Result<()> {
    let path = &resource.resolved;
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if let Some(identity) = resource.identity
                && identity != (meta.dev(), meta.ino())
            {
                return Err(io::Error::other(format!(
                    "{} is not the one the lease created",
                    path.display()
                )));
            }
            if meta.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else if meta.is_file() {
                std::fs::remove_file(path)?;
            } else {
                return Err(io::Error::other(format!(
                    "{} is neither a directory nor a file",
                    path.display()
                )));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match path.parent() {
        Some(parent) => match sync_dir(parent) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn private(path: &Path) -> Resource {
        Resource::new(
            &Spelled::of(path, Path::new("/")).unwrap(),
            Access::Write,
            Disposition::Private,
        )
    }

    #[test]
    fn a_held_lease_is_a_reservation_and_a_released_one_is_reclaimed() {
        let root = crate::test_scratch::scratch("reservations", "store-cycle");
        let store = Store::at(root.join("leases"));
        let dir = root.join("out").join(".brokkr-pbfhogg-x");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("staging"), b"").unwrap();
        let output = Resource::new(
            &Spelled::of(&root.join("out/o.pbf"), &root).unwrap(),
            Access::Write,
            Disposition::Reserve,
        );
        let mut lease = store.create(vec![output, private(&dir)]).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        lease.set_identity(1, (meta.dev(), meta.ino())).unwrap();

        let scan = store.scan();
        assert_eq!(scan.reservations.len(), 1, "{scan:?}");
        assert!(scan.reservations[0].retained.is_none());
        assert!(dir.exists());

        lease.release().unwrap();
        assert!(!dir.exists(), "the private directory goes with the lease");
        assert!(
            root.join("out").exists(),
            "a reserved output's directory is never removed"
        );
        assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);
    }

    /// A lease whose holder died without releasing is reclaimed by the next
    /// scan; a forged directory with the recorded name but another identity
    /// is refused and the lease kept.
    #[test]
    fn a_dead_lease_is_reclaimed_unless_its_directory_was_replaced() {
        let root = crate::test_scratch::scratch("reservations", "store-dead");
        let store = Store::at(root.join("leases"));
        let dir = root.join(".brokkr-pbfhogg-y");
        std::fs::create_dir_all(&dir).unwrap();
        let mut lease = store.create(vec![private(&dir)]).unwrap();
        lease.set_identity(0, (0, 0)).unwrap();
        // The holder dies: its descriptor closes without a release.
        drop(lease.file.take());
        drop(lease);
        let scan = store.scan();
        assert_eq!(scan.reservations.len(), 1);
        assert!(
            scan.reservations[0].retained.is_some(),
            "identity mismatch keeps it"
        );
        assert!(dir.exists());

        // The same death with the directory's real identity recorded: the
        // next scan reclaims the directory and the lease.
        let real = root.join(".brokkr-pbfhogg-z");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("staging"), b"").unwrap();
        let mut lease = store.create(vec![private(&real)]).unwrap();
        let meta = std::fs::metadata(&real).unwrap();
        lease.set_identity(0, (meta.dev(), meta.ino())).unwrap();
        let id = lease.id().to_owned();
        drop(lease.file.take());
        drop(lease);
        let scan = store.scan();
        assert!(scan.reservations.iter().all(|r| r.id != id), "{scan:?}");
        assert!(!real.exists(), "a dead lease's own directory is reclaimed");
        assert!(absent(&store.path(&id, LOCK_EXT)) && absent(&store.path(&id, RECORD_EXT)));
    }

    #[test]
    fn an_unreadable_record_is_unknown_and_a_missing_one_is_unpublished() {
        let root = crate::test_scratch::scratch("reservations", "store-unknown");
        let store = Store::at(root.join("leases"));
        let lease = store.create(Vec::new()).unwrap();
        let id = lease.id().to_owned();
        drop(lease);
        std::fs::write(store.path(&id, RECORD_EXT), b"{garbage").unwrap();
        let scan = store.scan();
        assert_eq!(scan.unknown.len(), 1, "{scan:?}");
        assert!(
            store.path(&id, LOCK_EXT).exists(),
            "an unreadable record keeps its lock"
        );

        std::fs::remove_file(store.path(&id, RECORD_EXT)).unwrap();
        let scan = store.scan();
        assert!(
            scan.unknown.is_empty() && scan.reservations.is_empty(),
            "{scan:?}"
        );
        assert!(
            !store.path(&id, LOCK_EXT).exists(),
            "an unpublished lease is removed"
        );
    }

    /// A forked child that inherits the description keeps the lease held
    /// after the parent's own descriptor is gone.
    #[test]
    fn an_inherited_description_keeps_the_lease_held() {
        let root = crate::test_scratch::scratch("reservations", "store-inherit");
        let store = Store::at(root.join("leases"));
        let lease = store.create(Vec::new()).unwrap();
        let dup = lease.file.as_ref().unwrap().try_clone().unwrap();
        let err = lease.release().unwrap_err();
        assert!(err.to_string().contains("still held"), "{err}");
        let scan = store.scan();
        assert_eq!(scan.reservations.len(), 1);
        drop(dup);
        let scan = store.scan();
        assert!(scan.reservations.is_empty(), "{scan:?}");
    }
}
