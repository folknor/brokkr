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
//!
//! A script-bench call's borrow is the one exception to durability, and is
//! named apart for it: `<id>.borrow.lock` and `<id>.borrow.record`. A borrow
//! holds no private resource and pins its owner only while held, and a power
//! loss leaves nothing held, so nothing needs its record across one; its
//! files are written and removed without a sync, which would otherwise cost
//! every `binary()` call several fsyncs inside the measured block. Its name
//! is what lets a scan clear the residue a power loss can leave (a torn or
//! empty record, a record without its lock): a free borrow-named lease is
//! removed without reading its record, and a borrow-named record or
//! temporary with no lock beside it is removed. A held one must hold a valid
//! borrow record. A borrow under the ordinary names, which a brokkr before
//! the split wrote, is read and reclaimed as any ordinary lease is.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::paths::Spelled;

/// Bumped whenever the record's meaning changes; an unknown version reads as
/// an unknown reservation, never as an empty one. Version 2 added the ties of
/// an iteration owner and its borrows; a version 1 record reads as untied.
const RECORD_VERSION: u32 = 2;

const LOCK_EXT: &str = "lock";
const RECORD_EXT: &str = "record";
const TMP_EXT: &str = "record.tmp";
const BORROW_LOCK_EXT: &str = "borrow.lock";
const BORROW_RECORD_EXT: &str = "borrow.record";
const BORROW_TMP_EXT: &str = "borrow.record.tmp";

/// Which names a lease's files carry, and with them whether they are
/// durable: an ordinary lease's are; a borrow's are not (the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Durable,
    Borrow,
}

impl Kind {
    fn of(tie: &Tie) -> Self {
        match tie {
            Tie::Borrow(_) => Self::Borrow,
            Tie::None | Tie::Owner => Self::Durable,
        }
    }

    fn lock(self) -> &'static str {
        match self {
            Self::Durable => LOCK_EXT,
            Self::Borrow => BORROW_LOCK_EXT,
        }
    }

    fn record(self) -> &'static str {
        match self {
            Self::Durable => RECORD_EXT,
            Self::Borrow => BORROW_RECORD_EXT,
        }
    }

    fn tmp(self) -> &'static str {
        match self {
            Self::Durable => TMP_EXT,
            Self::Borrow => BORROW_TMP_EXT,
        }
    }
}
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
    /// An iteration owner: its borrows pin it (`Tie::Owner`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    owner: bool,
    /// The owner a borrow belongs to (`Tie::Borrow`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    borrows_from: Option<String>,
}

/// How a lease relates to others. A launch's or a staged write's lease stands
/// alone. An iteration owner lease is held by brokkr alone and never lent; each
/// of its children inherits a borrow naming it instead, and while any borrow
/// is held the owner is kept even when its own lock is free, so a brokkr that
/// died under a running child never has its iteration reclaimed under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tie {
    None,
    Owner,
    Borrow(String),
}

/// A published lease as its record states it.
#[derive(Debug)]
struct Recorded {
    resources: Vec<Resource>,
    tie: Tie,
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
    /// The directory the store's durability is anchored at: one assumed
    /// durably reachable, the user's home for the per-user store.
    anchor: PathBuf,
    root: PathBuf,
}

impl Store {
    /// The store in `root`, below `anchor`.
    pub(crate) fn at(anchor: PathBuf, root: PathBuf) -> Self {
        Self { anchor, root }
    }

    /// The per-user store, beside the global lock and the compile lease.
    /// Unit tests use a scratch store instead (`super::store`).
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn user() -> Option<Self> {
        let home = crate::user_dirs::home()?;
        let root = crate::user_dirs::brokkr_state_dir()?.join("launch-leases");
        Some(Self::at(home, root))
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

    /// Make the store directory exist durably: the whole chain from the
    /// anchor down to it is synced on every creation of a lease, not only the
    /// call that made it, since an earlier creation may have died before its
    /// syncs - existence is no evidence of durability - and a power loss must
    /// not lose the store while a private directory it names survives on
    /// another filesystem.
    fn ensure_root(&self) -> io::Result<()> {
        crate::atomic_write::create_dir_all_durable(&self.anchor, &self.root)
    }

    /// Create a lease holding `resources`: the lock file, created under a
    /// pending name, flocked, then renamed to its final name, then the
    /// record, all durable on return.
    pub(crate) fn create(&self, resources: Vec<Resource>) -> io::Result<Lease> {
        self.create_tied(resources, Tie::None)
    }

    /// [`Store::create`] for a lease tied to others: an iteration owner, or a
    /// borrow of one. A borrow never holds a private resource: what it uses
    /// is its owner's to remove.
    pub(crate) fn create_tied(&self, resources: Vec<Resource>, tie: Tie) -> io::Result<Lease> {
        if matches!(tie, Tie::Borrow(_))
            && resources
                .iter()
                .any(|r| r.disposition == Disposition::Private)
        {
            return Err(io::Error::other("a borrow cannot hold a private resource"));
        }
        let kind = Kind::of(&tie);
        match kind {
            Kind::Durable => self.ensure_root()?,
            // Its owner made the store durably; a borrow needs it to exist.
            Kind::Borrow => std::fs::create_dir_all(&self.root)?,
        }
        let id = crate::lockfile::RunToken::mint()
            .ok_or_else(|| io::Error::other("no entropy for a lease id"))?
            .as_str()
            .to_owned();
        let pending = self.path(&id, PENDING_EXT);
        let lock_path = self.path(&id, kind.lock());
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
        if kind == Kind::Durable {
            sync_dir(&self.root)?;
        }
        let lease = Lease {
            store: self.clone(),
            id,
            file: Some(file),
            resources,
            tie,
        };
        if let Err(e) = lease.publish() {
            // Nothing the record would describe exists yet. A record that did
            // reach its name - or may have, when it cannot be inspected - stays
            // with its lock, for the next scan to reclaim; only a lease whose
            // record is certainly absent loses its lock file here. A borrow,
            // which no reader needs, goes whole, its lock last.
            match kind {
                Kind::Durable => {
                    if absent(&self.path(&lease.id, kind.record())) {
                        drop(std::fs::remove_file(&lock_path));
                    }
                }
                Kind::Borrow => self.remove_borrow_or_warn(&lease.id),
            }
            return Err(e);
        }
        Ok(lease)
    }

    /// Reclaim every free lease and report the rest. An absent store holds
    /// none; one that cannot be read is an unknown reservation.
    ///
    /// Two phases, so the order the directory lists leases in decides
    /// nothing: every lease is classified first, each free one with its
    /// exclusive flock kept, then the free ones are decided. A free lease
    /// never becomes held again (nothing holds its description to pass on),
    /// so a borrow seen held and freed since only delays its owner's
    /// reclamation, and one seen free cannot revive to need it. The one
    /// exception to the order is a free borrow-named lease, removed as phase
    /// one meets it: it owns nothing and pins nothing, so nothing waits on
    /// deciding it.
    pub(crate) fn scan(&self) -> Scan {
        let mut scan = Scan::default();
        let inventory = self.classify(&mut scan);
        for free in inventory.free {
            match owner_kept(&free.id, &free.recorded.tie, &inventory.pins, &scan) {
                Some(retained) => scan.reservations.push(Reservation {
                    id: free.id,
                    resources: free.recorded.resources,
                    retained,
                }),
                None => {
                    if let Err(e) = self.reclaim(&free.id, &free.recorded.resources) {
                        scan.reservations.push(Reservation {
                            id: free.id,
                            resources: free.recorded.resources,
                            retained: Some(e.to_string()),
                        });
                    }
                }
            }
            drop(free.lock);
        }
        scan
    }

    /// Phase one of a scan: held leases become reservations and their
    /// borrows' owners pins, unestablished ones unknown; the free ones are
    /// returned undecided, each with its exclusive flock. Pending locks past
    /// their grace, unpublished free leases and orphaned temporary records,
    /// none of which anything can depend on, are removed on the way.
    fn classify(&self, scan: &mut Scan) -> Inventory {
        let mut inventory = Inventory::default();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return inventory,
            Err(e) => {
                scan.unknown
                    .push(("*".into(), format!("the lease store cannot be read: {e}")));
                return inventory;
            }
        };
        let mut ids: Vec<String> = Vec::new();
        let mut stray_records: Vec<String> = Vec::new();
        let mut temps: Vec<String> = Vec::new();
        let mut pendings: Vec<String> = Vec::new();
        let mut borrows: Vec<String> = Vec::new();
        let mut borrow_residue: Vec<String> = Vec::new();
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
                BORROW_LOCK_EXT => borrows.push(id.to_owned()),
                BORROW_RECORD_EXT | BORROW_TMP_EXT => borrow_residue.push(id.to_owned()),
                _ => {}
            }
        }
        for id in pendings {
            self.reclaim_pending(&id);
        }
        for id in &ids {
            self.classify_one(id, scan, &mut inventory);
        }
        for id in &borrows {
            self.classify_borrow(id, scan, &mut inventory);
        }
        // A borrow's record or temporary without its lock (nor the pending
        // name it is made under) is what a power loss can leave of a borrow
        // nothing holds: only certain absence removes it.
        for id in borrow_residue {
            if !borrows.contains(&id)
                && absent(&self.path(&id, BORROW_LOCK_EXT))
                && absent(&self.path(&id, PENDING_EXT))
            {
                self.remove_borrow_or_warn(&id);
            }
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
        inventory
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

    fn classify_one(&self, id: &str, scan: &mut Scan, inventory: &mut Inventory) {
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
            Ok(true) => match self.read_record(id, Kind::Durable) {
                Ok(None) => {
                    // Never published: nothing it would name was created.
                    drop(std::fs::remove_file(self.path(id, TMP_EXT)));
                    if let Err(e) = remove_synced(&lock_path, &self.root) {
                        scan.unknown
                            .push((id.to_owned(), format!("its lock cannot be removed: {e}")));
                    }
                }
                Ok(Some(recorded)) => inventory.free.push(Free {
                    id: id.to_owned(),
                    recorded,
                    lock: file,
                }),
                Err(e) => scan
                    .unknown
                    .push((id.to_owned(), format!("its record is unreadable: {e}"))),
            },
            Ok(false) => match self.read_record(id, Kind::Durable) {
                Ok(Some(recorded)) => {
                    if let Tie::Borrow(owner) = recorded.tie {
                        inventory.pins.insert(owner);
                    }
                    scan.reservations.push(Reservation {
                        id: id.to_owned(),
                        resources: recorded.resources,
                        retained: None,
                    });
                }
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
    }

    /// A borrow-named lease. Free: removed whatever its record says, since a
    /// borrow owns nothing and a free one pins nothing; the lock goes last,
    /// so a removal that fails leaves it for the next scan. Held: it must
    /// hold a valid borrow record, else it is unknown.
    fn classify_borrow(&self, id: &str, scan: &mut Scan, inventory: &mut Inventory) {
        let lock_path = self.path(id, BORROW_LOCK_EXT);
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
            Ok(true) => {
                self.remove_borrow_or_warn(id);
                drop(file);
            }
            Ok(false) => match self.read_record(id, Kind::Borrow) {
                Ok(Some(Recorded {
                    resources,
                    tie: Tie::Borrow(owner),
                })) => {
                    inventory.pins.insert(owner);
                    scan.reservations.push(Reservation {
                        id: id.to_owned(),
                        resources,
                        retained: None,
                    });
                }
                Ok(Some(_)) => scan.unknown.push((
                    id.to_owned(),
                    "a borrow-named lease whose record is no borrow".into(),
                )),
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
    }

    /// [`Store::remove_borrow`] where nothing waits on the outcome (a scan, a
    /// failed creation): a failure blocks nothing, since a free borrow owns
    /// and pins nothing, and the next scan retries, so it is a warning that
    /// names what stays.
    fn remove_borrow_or_warn(&self, id: &str) {
        if let Err(e) = self.remove_borrow(id) {
            crate::output::warn(&format!(
                "free borrow lease {id} could not be removed from {} ({e}); a later scan retries",
                self.root.display()
            ));
        }
    }

    /// Remove a free borrow's files, unsynced: temporary, record, lock.
    fn remove_borrow(&self, id: &str) -> io::Result<()> {
        for ext in [BORROW_TMP_EXT, BORROW_RECORD_EXT, BORROW_LOCK_EXT] {
            match std::fs::remove_file(self.path(id, ext)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// The record's resources and tie; `None` only when it does not exist. A
    /// record whose tie breaks the protocol's shape is unreadable.
    fn read_record(&self, id: &str, kind: Kind) -> io::Result<Option<Recorded>> {
        let text = match std::fs::read_to_string(self.path(id, kind.record())) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let record: Record = serde_json::from_str(&text).map_err(io::Error::other)?;
        let tie = match (record.version, record.owner, record.borrows_from) {
            (1, false, None) | (RECORD_VERSION, false, None) => Tie::None,
            (RECORD_VERSION, true, None) => Tie::Owner,
            (RECORD_VERSION, false, Some(owner)) => {
                if crate::lockfile::RunToken::parse(&owner).is_none() {
                    return Err(io::Error::other(format!(
                        "a borrow of {owner:?}, which is no lease id"
                    )));
                }
                if record
                    .resources
                    .iter()
                    .any(|r| r.disposition == Disposition::Private)
                {
                    return Err(io::Error::other("a borrow holding a private resource"));
                }
                Tie::Borrow(owner)
            }
            (1 | RECORD_VERSION, _, _) => {
                return Err(io::Error::other(format!(
                    "record version {} with an impossible tie",
                    record.version
                )));
            }
            (version, _, _) => {
                return Err(io::Error::other(format!("record version {version}")));
            }
        };
        Ok(Some(Recorded {
            resources: record.resources,
            tie,
        }))
    }

    /// Whether an iteration owner whose own lock is free must be kept, and
    /// why: phase one again, for this one owner, at its release.
    fn owner_release_blocked(&self, id: &str) -> Option<String> {
        let mut scan = Scan::default();
        let inventory = self.classify(&mut scan);
        match owner_kept(id, &Tie::Owner, &inventory.pins, &scan) {
            Some(None) => Some(format!(
                "a borrow of iteration lease {id} is still held: some process that \
                 inherited it still runs, so the iteration is kept until a later scan finds \
                 it free"
            )),
            Some(Some(why)) => Some(format!("iteration lease {id} is kept: {why}")),
            None => None,
        }
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

/// What phase one of a scan leaves to decide.
#[derive(Default)]
struct Inventory {
    free: Vec<Free>,
    /// The owners named by held borrows.
    pins: std::collections::BTreeSet<String>,
}

/// A free lease, its exclusive flock kept until it is decided.
struct Free {
    id: String,
    recorded: Recorded,
    lock: File,
}

/// Whether the free lease `id` must be kept rather than reclaimed: only an
/// iteration owner ever is. `Some(None)` while a held borrow pins it (it is
/// in use); `Some(Some(why))` while an unestablished lease exists, which may
/// be a borrow of it. Every other free lease is reclaimed as it always was.
fn owner_kept(
    id: &str,
    tie: &Tie,
    pins: &std::collections::BTreeSet<String>,
    scan: &Scan,
) -> Option<Option<String>> {
    if *tie != Tie::Owner {
        return None;
    }
    if pins.contains(id) {
        return Some(None);
    }
    scan.unknown.first().map(|(other, why)| {
        Some(format!(
            "lease {other} cannot be established ({why}) and may be a borrow of it"
        ))
    })
}

/// One lease, held by this process until [`Lease::release`].
#[derive(Debug)]
pub(crate) struct Lease {
    store: Store,
    id: String,
    file: Option<File>,
    resources: Vec<Resource>,
    tie: Tie,
}

impl Lease {
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

    /// Record the identity of the private directory at `index`, durably for
    /// every lease but a borrow (which holds none).
    pub(crate) fn set_identity(&mut self, index: usize, identity: (u64, u64)) -> io::Result<()> {
        if let Some(resource) = self.resources.get_mut(index) {
            resource.identity = Some(identity);
        }
        self.publish()
    }

    /// Drop the resource at `index` from the record, durably but for a
    /// borrow: a private
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
            owner: self.tie == Tie::Owner,
            borrows_from: match &self.tie {
                Tie::Borrow(owner) => Some(owner.clone()),
                Tie::None | Tie::Owner => None,
            },
        };
        let text = serde_json::to_vec(&record).map_err(io::Error::other)?;
        let kind = Kind::of(&self.tie);
        let tmp = self.store.path(&self.id, kind.tmp());
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&tmp)?;
        file.write_all(&text)?;
        if kind == Kind::Durable {
            file.sync_all()?;
        }
        drop(file);
        // Renamed into place either way, so a concurrent reader sees a whole
        // record or none.
        std::fs::rename(&tmp, self.store.path(&self.id, kind.record()))?;
        match kind {
            Kind::Durable => sync_dir(&self.store.root),
            Kind::Borrow => Ok(()),
        }
    }

    /// Give up this process's hold and, once no process holds the lease any
    /// more, remove it with its private directories. A lease still held after
    /// the release window stays recorded, a live reservation, and is the
    /// error. So is an iteration owner that a held borrow pins, or that an
    /// unestablished lease may: its record stays for a later scan to reclaim
    /// once nothing can need it.
    pub(crate) fn release(self) -> io::Result<()> {
        self.release_with(&mut || {})
    }

    /// [`release`](Self::release), calling `wait` before each pause of the
    /// release window, so a caller can keep serving what a lingering holder
    /// may be blocked on (a script-bench call drains its marker FIFO).
    pub(crate) fn release_with(mut self, wait: &mut dyn FnMut()) -> io::Result<()> {
        drop(self.file.take());
        let kind = Kind::of(&self.tie);
        let lock_path = self.store.path(&self.id, kind.lock());
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
                if self.tie == Tie::Owner
                    && let Some(why) = self.store.owner_release_blocked(&self.id)
                {
                    drop(file);
                    return Err(io::Error::other(why));
                }
                let result = match kind {
                    Kind::Durable => self.store.reclaim(&self.id, &self.resources),
                    Kind::Borrow => self.store.remove_borrow(&self.id),
                };
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
            wait();
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
///
/// Adjudicated (brokkr#46 slice 4 audit): the path is not walked without
/// following links, so a child that put a symlink in place of an ancestor
/// (a library replay's `outputs/`) has this reach the directory through it.
/// That is not a hazard: the release runs only once no process holds the
/// lease, so nothing can swap the tree under it, and only the inode the
/// lease created is removed, wherever a link led; the library's own checks
/// after the release then report the link.
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
        let store = Store::at(root.clone(), root.join("leases"));
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
        let store = Store::at(root.clone(), root.join("leases"));
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
        let store = Store::at(root.clone(), root.join("leases"));
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

    /// An owner lease holding a fresh private directory, and one borrow of it
    /// whose description a "child" (a duplicate) keeps; both of this
    /// process's own descriptors are gone, as when brokkr dies mid-borrow.
    fn abandoned_pair(store: &Store, root: &Path, n: usize) -> (PathBuf, File) {
        let dir = root.join(format!("iteration-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("out"), b"").unwrap();
        let mut owner = store.create_tied(vec![private(&dir)], Tie::Owner).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        owner.set_identity(0, (meta.dev(), meta.ino())).unwrap();
        let borrow = store
            .create_tied(Vec::new(), Tie::Borrow(owner.id().to_owned()))
            .unwrap();
        let child = borrow.file.as_ref().unwrap().try_clone().unwrap();
        drop(owner.file.take());
        drop(owner);
        let mut borrow = borrow;
        drop(borrow.file.take());
        drop(borrow);
        (dir, child)
    }

    /// Crash mid-borrow at the store: whatever order the directory lists the
    /// leases in, a free owner a held borrow names is kept, and once the
    /// borrow's last holder is gone the next scan reclaims both.
    #[test]
    fn a_held_borrow_pins_its_free_owner_in_any_listing_order() {
        let root = crate::test_scratch::scratch("reservations", "store-pin");
        let store = Store::at(root.clone(), root.join("leases"));
        let pairs: Vec<(PathBuf, File)> =
            (0..8).map(|n| abandoned_pair(&store, &root, n)).collect();
        let scan = store.scan();
        assert!(scan.unknown.is_empty(), "{scan:?}");
        assert_eq!(scan.reservations.len(), 16, "{scan:?}");
        assert!(scan.reservations.iter().all(|r| r.retained.is_none()));
        for (dir, _) in &pairs {
            assert!(dir.exists(), "a pinned owner keeps {}", dir.display());
        }
        let dirs: Vec<PathBuf> = pairs
            .into_iter()
            .map(|(dir, child)| {
                drop(child);
                dir
            })
            .collect();
        let scan = store.scan();
        assert!(
            scan.reservations.is_empty() && scan.unknown.is_empty(),
            "{scan:?}"
        );
        for dir in &dirs {
            assert!(!dir.exists(), "reclaimed with its owner: {}", dir.display());
        }
        assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);
    }

    /// Crash after the child exited: neither lease was released, neither is
    /// held, and one scan reclaims both with the owner's directory.
    #[test]
    fn a_crash_after_the_child_exited_is_reclaimed_by_one_scan() {
        let root = crate::test_scratch::scratch("reservations", "store-exited");
        let store = Store::at(root.clone(), root.join("leases"));
        let (dir, child) = abandoned_pair(&store, &root, 0);
        drop(child);
        let scan = store.scan();
        assert!(
            scan.reservations.is_empty() && scan.unknown.is_empty(),
            "{scan:?}"
        );
        assert!(!dir.exists());
        assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);
    }

    /// An owner's release while a borrow of it is still held is refused and
    /// keeps the record; the scan after the borrow's holder is gone reclaims.
    #[test]
    fn an_owner_release_under_a_held_borrow_keeps_the_iteration() {
        let root = crate::test_scratch::scratch("reservations", "store-owner-release");
        let store = Store::at(root.clone(), root.join("leases"));
        let dir = root.join("iteration");
        std::fs::create_dir_all(&dir).unwrap();
        let mut owner = store.create_tied(vec![private(&dir)], Tie::Owner).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        owner.set_identity(0, (meta.dev(), meta.ino())).unwrap();
        let borrow = store
            .create_tied(Vec::new(), Tie::Borrow(owner.id().to_owned()))
            .unwrap();
        let child = borrow.file.as_ref().unwrap().try_clone().unwrap();
        let err = borrow.release().unwrap_err();
        assert!(err.to_string().contains("still held"), "{err}");
        let err = owner.release().unwrap_err();
        assert!(err.to_string().contains("borrow"), "{err}");
        assert!(dir.exists(), "kept while the borrow is held");
        assert_eq!(store.scan().reservations.len(), 2);
        drop(child);
        let scan = store.scan();
        assert!(
            scan.reservations.is_empty() && scan.unknown.is_empty(),
            "{scan:?}"
        );
        assert!(!dir.exists());
    }

    /// A lease whose record cannot be read may be a borrow of any owner, so
    /// a free owner is kept while it exists; an untied free lease is still
    /// reclaimed, as before ties existed.
    #[test]
    fn an_unknown_lease_keeps_free_owners_but_not_plain_leases() {
        let root = crate::test_scratch::scratch("reservations", "store-unknown-owner");
        let store = Store::at(root.clone(), root.join("leases"));
        let owned = root.join("owned");
        let plain = root.join("plain");
        std::fs::create_dir_all(&owned).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        let mut owner = store
            .create_tied(vec![private(&owned)], Tie::Owner)
            .unwrap();
        drop(owner.file.take());
        drop(owner);
        let mut launch = store.create(vec![private(&plain)]).unwrap();
        drop(launch.file.take());
        drop(launch);
        let unknown = store.create(Vec::new()).unwrap();
        std::fs::write(store.path(unknown.id(), RECORD_EXT), b"{garbage").unwrap();
        let scan = store.scan();
        assert_eq!(scan.unknown.len(), 1, "{scan:?}");
        assert_eq!(scan.reservations.len(), 1, "{scan:?}");
        assert!(scan.reservations[0].retained.is_some());
        assert!(owned.exists(), "an owner is kept while a borrow may exist");
        assert!(!plain.exists(), "a plain lease is reclaimed as always");
        let id = unknown.id().to_owned();
        std::fs::remove_file(store.path(&id, RECORD_EXT)).unwrap();
        drop(unknown);
        let scan = store.scan();
        assert!(
            scan.reservations.is_empty() && scan.unknown.is_empty(),
            "{scan:?}"
        );
        assert!(!owned.exists());
    }

    /// A version 1 record still reads, untied; a tie outside the protocol's
    /// shape is unreadable.
    #[test]
    fn a_version_one_record_reads_untied_and_a_bad_tie_is_unknown() {
        let root = crate::test_scratch::scratch("reservations", "store-versions");
        let store = Store::at(root.clone(), root.join("leases"));
        let dir = root.join("old");
        std::fs::create_dir_all(&dir).unwrap();
        let mut old = store.create(Vec::new()).unwrap();
        let v1 = serde_json::json!({ "version": 1, "resources": [private(&dir)] });
        std::fs::write(store.path(old.id(), RECORD_EXT), v1.to_string()).unwrap();
        drop(old.file.take());
        drop(old);
        let scan = store.scan();
        assert!(
            scan.reservations.is_empty() && scan.unknown.is_empty(),
            "{scan:?}"
        );
        assert!(!dir.exists(), "a version 1 lease is reclaimed as before");

        for bad in [
            serde_json::json!({ "version": 2, "resources": [], "owner": true,
                                "borrows_from": "x" }),
            serde_json::json!({ "version": 2, "resources": [], "borrows_from": "not a token" }),
            serde_json::json!({ "version": 1, "resources": [], "owner": true }),
            serde_json::json!({ "version": 3, "resources": [] }),
        ] {
            let lease = store.create(Vec::new()).unwrap();
            std::fs::write(store.path(lease.id(), RECORD_EXT), bad.to_string()).unwrap();
            let scan = store.scan();
            assert_eq!(scan.unknown.len(), 1, "{bad}: {scan:?}");
            let id = lease.id().to_owned();
            std::fs::remove_file(store.path(&id, RECORD_EXT)).unwrap();
            drop(lease);
            assert!(store.scan().unknown.is_empty());
        }
        let borrow = store
            .create_tied(vec![private(&dir)], Tie::Borrow("x".into()))
            .unwrap_err();
        assert!(borrow.to_string().contains("private"), "{borrow}");
    }

    /// A borrow is written under its own names, and what a power loss can
    /// leave of a free one (a torn record, a record or temporary without its
    /// lock) is cleared by the next scan without blocking anything or
    /// keeping its owner.
    #[test]
    fn a_free_borrows_residue_is_cleared_and_never_blocks() {
        let root = crate::test_scratch::scratch("reservations", "store-borrow-residue");
        let store = Store::at(root.clone(), root.join("leases"));
        let dir = root.join("iteration");
        std::fs::create_dir_all(&dir).unwrap();
        let mut owner = store.create_tied(vec![private(&dir)], Tie::Owner).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        owner.set_identity(0, (meta.dev(), meta.ino())).unwrap();
        let mut borrow = store
            .create_tied(Vec::new(), Tie::Borrow(owner.id().to_owned()))
            .unwrap();
        let id = borrow.id().to_owned();
        assert!(store.path(&id, BORROW_LOCK_EXT).exists());
        assert!(store.path(&id, BORROW_RECORD_EXT).exists());
        assert!(absent(&store.path(&id, LOCK_EXT)) && absent(&store.path(&id, RECORD_EXT)));
        // Torn by a power loss, and nothing holds it any more.
        std::fs::write(store.path(&id, BORROW_RECORD_EXT), b"").unwrap();
        borrow.close_own_for_test();
        drop(borrow);
        // Residue of other borrows: a record and a temporary without a lock.
        let stray = crate::lockfile::RunToken::mint()
            .unwrap()
            .as_str()
            .to_owned();
        std::fs::write(store.path(&stray, BORROW_RECORD_EXT), b"{torn").unwrap();
        std::fs::write(store.path(&stray, BORROW_TMP_EXT), b"").unwrap();
        let scan = store.scan();
        assert!(scan.unknown.is_empty(), "{scan:?}");
        assert_eq!(scan.reservations.len(), 1, "only the owner: {scan:?}");
        for ext in [BORROW_LOCK_EXT, BORROW_RECORD_EXT, BORROW_TMP_EXT] {
            assert!(absent(&store.path(&id, ext)), "{ext}");
            assert!(absent(&store.path(&stray, ext)), "{ext}");
        }
        owner.release().unwrap();
        assert!(!dir.exists());
        assert_eq!(std::fs::read_dir(store.root()).unwrap().count(), 0);
    }

    /// Residue that cannot be removed (a directory where a borrow's record
    /// would be) stays for a later scan and blocks nothing.
    #[test]
    fn unremovable_borrow_residue_blocks_nothing() {
        let root = crate::test_scratch::scratch("reservations", "store-borrow-stuck");
        let store = Store::at(root.clone(), root.join("leases"));
        std::fs::create_dir_all(store.root()).unwrap();
        let stray = crate::lockfile::RunToken::mint()
            .unwrap()
            .as_str()
            .to_owned();
        std::fs::create_dir(store.path(&stray, BORROW_RECORD_EXT)).unwrap();
        let scan = store.scan();
        assert!(
            scan.unknown.is_empty() && scan.reservations.is_empty(),
            "{scan:?}"
        );
        assert!(store.path(&stray, BORROW_RECORD_EXT).is_dir());
    }

    /// A held borrow-named lease must hold a borrow record; anything else
    /// there is unknown. A borrow under the ordinary names, as a brokkr
    /// before the split wrote one, still pins its owner while held and is
    /// reclaimed when free.
    #[test]
    fn a_held_borrow_needs_a_borrow_record_and_a_legacy_one_still_pins() {
        let root = crate::test_scratch::scratch("reservations", "store-borrow-kinds");
        let store = Store::at(root.clone(), root.join("leases"));
        let borrow = store
            .create_tied(
                Vec::new(),
                Tie::Borrow(
                    crate::lockfile::RunToken::mint()
                        .unwrap()
                        .as_str()
                        .to_owned(),
                ),
            )
            .unwrap();
        let plain = serde_json::json!({ "version": 2, "resources": [] });
        std::fs::write(
            store.path(borrow.id(), BORROW_RECORD_EXT),
            plain.to_string(),
        )
        .unwrap();
        let scan = store.scan();
        assert_eq!(scan.unknown.len(), 1, "{scan:?}");
        borrow.release().unwrap();
        assert!(store.scan().unknown.is_empty());

        let dir = root.join("legacy-iteration");
        std::fs::create_dir_all(&dir).unwrap();
        let mut owner = store.create_tied(vec![private(&dir)], Tie::Owner).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        owner.set_identity(0, (meta.dev(), meta.ino())).unwrap();
        // A legacy borrow: an ordinary lease whose record names the owner.
        let mut legacy = store.create(Vec::new()).unwrap();
        let record = serde_json::json!({
            "version": 2, "resources": [], "borrows_from": owner.id(),
        });
        std::fs::write(store.path(legacy.id(), RECORD_EXT), record.to_string()).unwrap();
        let child = legacy.file.as_ref().unwrap().try_clone().unwrap();
        legacy.close_own_for_test();
        let legacy_id = legacy.id().to_owned();
        drop(legacy);
        drop(owner.file.take());
        drop(owner);
        let scan = store.scan();
        assert!(scan.unknown.is_empty(), "{scan:?}");
        assert!(dir.exists(), "the held legacy borrow pins its owner");
        drop(child);
        let scan = store.scan();
        assert!(
            scan.reservations.is_empty() && scan.unknown.is_empty(),
            "{scan:?}"
        );
        assert!(!dir.exists());
        assert!(absent(&store.path(&legacy_id, LOCK_EXT)));
    }

    /// A forked child that inherits the description keeps the lease held
    /// after the parent's own descriptor is gone.
    #[test]
    fn an_inherited_description_keeps_the_lease_held() {
        let root = crate::test_scratch::scratch("reservations", "store-inherit");
        let store = Store::at(root.clone(), root.join("leases"));
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
