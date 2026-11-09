//! Iteration ownership: what one script-bench iteration owns, and the borrow
//! each of its children launches with (`reference/bench-resources.md`).
//!
//! The iteration holds an owner lease, never lent to a child: its record
//! names the run's inputs for reading and one private iteration directory,
//! with `out/`, `scratch/`, `stdout/`, `stderr/` and `profile/` inside it,
//! for writing. Each child
//! launches with a borrow of its own, a lease whose record names the owner
//! and the call's declared resources, and inherits that one descriptor as a
//! pbfhogg child inherits its launch lease. A held borrow pins its owner: a
//! scan keeps a free owner while any borrow of it is held, so a brokkr that
//! dies under a running child never has the iteration reclaimed under it,
//! and the scan after the last holder is gone reclaims both. A borrow's
//! release, which tests that no process holds it any more, is the call's
//! completion: only then is its stdout spool handed back for reading. Its
//! stderr spool the caller keeps apart, so what it holds can be snapshotted
//! even when the release fails.
//!
//! A borrow is admitted in three steps, each handle spelled afresh at the
//! call: a declared write overlapping an input or an earlier call's spool is
//! refused (both are read-only for the run); every declared path must lie,
//! as it resolves now, within an owner resource, a write within a written
//! one; and the ordinary conflict rule applies against every reservation
//! but the owner itself, sibling borrows included.
//!
//! brokkr makes its own files in the iteration through directory descriptors
//! opened when the iteration was laid out, never following a link, and
//! checks before each that the advertised path still names the directory it
//! was given, so a child that displaced `stdout/` or `scratch/` makes the
//! next creation fail rather than land somewhere the reservations do not
//! name. What a child does with a path after its admission is outside the
//! guarantee: handles reserve, they do not confine.

use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use super::launch::{arm, conflict, disarm, io_error, make_private_dir};
use super::paths::Spelled;
use super::store::{Access, Disposition, Lease, Resource, Tie};
use crate::error::DevError;

/// The iteration directory's name prefix; a 128-bit random token follows.
pub(crate) const ITERATION_DIR_PREFIX: &str = ".brokkr-iteration-";

/// The directories inside an iteration directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Area {
    Out,
    Scratch,
    Stdout,
    /// Stderr spools, one per call, named as its stdout spool is.
    Stderr,
    /// Hotpath reports, one per marked call; brokkr's own, never a handle.
    Profile,
}

impl Area {
    const ALL: [Self; 5] = [
        Self::Out,
        Self::Scratch,
        Self::Stdout,
        Self::Stderr,
        Self::Profile,
    ];

    /// The areas whose entries are spools, pinned at their making.
    fn spools(self) -> bool {
        matches!(self, Self::Stdout | Self::Stderr)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Out => "out",
            Self::Scratch => "scratch",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Profile => "profile",
        }
    }
}

/// One area directory, held open by descriptor.
#[derive(Debug)]
struct AreaDir {
    area: Area,
    file: File,
    identity: (u64, u64),
}

/// One iteration's owner lease and its directory. Release it with
/// [`Iteration::release`] once the run published or failed.
#[derive(Debug)]
pub(crate) struct Iteration {
    lease: Option<Lease>,
    dir: PathBuf,
    identity: (u64, u64),
    areas: Vec<AreaDir>,
    /// The inputs as the caller spelled them, resolved afresh at each
    /// admission (a recorded lexical form has its `..` folded already).
    inputs: Vec<PathBuf>,
    /// The output and scratch names given out, so none is given twice.
    given: BTreeSet<(Area, String)>,
    /// Every spool made, read-only to every later call, each with a
    /// descriptor kept open until the iteration ends: it pins the inode, so
    /// the identity a later open of the spool is checked against cannot be
    /// reused by a file a child put in its place.
    spools: Vec<PinnedSpool>,
    /// The calls admitted so far: the next call's spools and report are
    /// named by it.
    calls: usize,
}

#[derive(Debug)]
struct PinnedSpool {
    path: PathBuf,
    file: File,
}

impl Iteration {
    /// Admit an iteration reading `inputs` (absolute paths), with its
    /// directory made in `location`. Refused when a reservation conflicts,
    /// as a launch is. The caller must place it where every brokkr removal
    /// that can reach it goes through the gate (`reference/bench-resources.md`).
    pub(crate) fn admit(location: &Path, inputs: &[PathBuf]) -> Result<Self, DevError> {
        let store = super::store().ok_or_else(|| {
            DevError::Config("$HOME is not set: the launch lease store has nowhere to live".into())
        })?;
        let _serial = super::serialize().map_err(|e| io_error("cannot admit an iteration", e))?;
        let scan = store.scan();
        let mut resources = Vec::new();
        for input in inputs {
            resources.push(Resource::new(
                &spell(input)?,
                Access::Read,
                Disposition::Reserve,
            ));
        }
        let token = crate::lockfile::RunToken::mint()
            .ok_or_else(|| DevError::Internal("no entropy for an iteration name".into()))?;
        let dir = location.join(format!("{ITERATION_DIR_PREFIX}{}", token.as_str()));
        resources.push(Resource::new(
            &spell(&dir)?,
            Access::Write,
            Disposition::Private,
        ));
        if let Some(why) = conflict(&scan, &resources) {
            return Err(DevError::Config(why));
        }
        let lease = store
            .create_tied(resources, Tie::Owner)
            .map_err(|e| io_error("cannot create an iteration lease", e))?;
        let index = lease.resources().len() - 1;
        let (lease, identity) = make_private_dir(lease, index, &dir)?;
        match lay_out(&dir, identity) {
            Ok(areas) => Ok(Self {
                lease: Some(lease),
                dir,
                identity,
                areas,
                inputs: inputs.to_vec(),
                given: BTreeSet::new(),
                spools: Vec::new(),
                calls: 0,
            }),
            Err(e) => {
                // The directory is the lease's: the release removes it.
                let released = lease.release();
                Err(io_error(
                    &format!("cannot lay out the iteration directory {}", dir.display()),
                    released.err().unwrap_or(e),
                ))
            }
        }
    }

    /// The iteration directory.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the runner reaches the iteration through its handles"
        )
    )]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// A fresh output path under `out/`, not created: the child writing it
    /// makes it. `name` is one path component, given once per iteration.
    pub(crate) fn output(&mut self, name: &str) -> Result<PathBuf, DevError> {
        self.claim(Area::Out, name)?;
        Ok(self.dir.join(Area::Out.name()).join(name))
    }

    /// A fresh empty directory under `scratch/`, made without following a
    /// link. `name` is one path component, given once per iteration. Its
    /// creation is a write admitted as a call's would be, so a sibling
    /// borrow still reserving `scratch/` refuses it.
    pub(crate) fn scratch(&mut self, name: &str) -> Result<PathBuf, DevError> {
        self.claim(Area::Scratch, name)?;
        let store = super::store().ok_or_else(|| {
            DevError::Config("$HOME is not set: the launch lease store has nowhere to live".into())
        })?;
        let _serial = super::serialize().map_err(|e| io_error("cannot make scratch", e))?;
        let area = self.verified(Area::Scratch)?;
        let path = self.dir.join(Area::Scratch.name()).join(name);
        self.admit_declared(
            &store,
            &[Resource::new(
                &spell(&path)?,
                Access::Write,
                Disposition::Reserve,
            )],
        )?;
        let made = mkdir_at(&area.file, name)
            .and_then(|()| open_dir_at(&area.file, name))
            .map_err(|e| io_error(&format!("cannot make scratch directory {name}"), e))?;
        names_inode(&path, &made)?;
        Ok(path)
    }

    /// Admit one call of `program` declaring `resources` (absolute paths,
    /// each with the access this call needs) and make its stdout spool.
    /// `stdin`, one of `resources` declared for reading, is opened as
    /// [`Iteration::open_file`] opens a file, against the same admission and
    /// inside the same critical section. The borrow's descriptor is armed
    /// for the next spawn of `program` on this thread; pass [`Call::stdout`]
    /// as the child's stdout and [`Call::take_stdin`] as its stdin. With
    /// `report`, an empty hotpath report is made under `profile/`, named as
    /// the spool is and declared written as the spool is (`Call::report`).
    pub(crate) fn call(
        &mut self,
        program: &Path,
        resources: &[(PathBuf, Access)],
        stdin: Option<&Path>,
        report: bool,
    ) -> Result<Call, DevError> {
        let store = super::store().ok_or_else(|| {
            DevError::Config("$HOME is not set: the launch lease store has nowhere to live".into())
        })?;
        let owner = self
            .lease
            .as_ref()
            .ok_or_else(|| DevError::Internal("a call on a released iteration".into()))?
            .id()
            .to_owned();
        let _serial = super::serialize().map_err(|e| io_error("cannot admit a call", e))?;
        let name = self.calls.to_string();
        let spool_path = self.dir.join(Area::Stdout.name()).join(&name);
        let stderr_path = self.dir.join(Area::Stderr.name()).join(&name);
        // Spelled once: the admission judges stdin as this, and it is opened
        // as this.
        let stdin = match stdin {
            Some(path) => {
                if !resources
                    .iter()
                    .any(|(p, a)| p == path && *a == Access::Read)
                {
                    return Err(DevError::Internal(format!(
                        "stdin {} is not declared read by its call",
                        path.display()
                    )));
                }
                Some((path, spell(path)?))
            }
            None => None,
        };
        let mut declared = Vec::new();
        for (path, access) in resources {
            let spelled = match &stdin {
                Some((stdin_path, spelled)) if *stdin_path == path.as_path() => spelled.clone(),
                _ => spell(path)?,
            };
            declared.push(Resource::new(&spelled, *access, Disposition::Reserve));
        }
        // The child writes its spools through stdout and stderr: implicit
        // writes.
        for path in [&spool_path, &stderr_path] {
            declared.push(Resource::new(
                &spell(path)?,
                Access::Write,
                Disposition::Reserve,
            ));
        }
        let report_path = if report {
            let path = self.dir.join(Area::Profile.name()).join(&name);
            declared.push(Resource::new(
                &spell(&path)?,
                Access::Write,
                Disposition::Reserve,
            ));
            Some(path)
        } else {
            None
        };
        self.admit_declared(&store, &declared)?;
        let stdin = match &stdin {
            Some((path, spelled)) => Some(self.open_admitted(path, spelled)?),
            None => None,
        };
        self.calls += 1;
        let file = self.make_spool(Area::Stdout, &name, &spool_path)?;
        let stderr = self.make_spool(Area::Stderr, &name, &stderr_path)?;
        if let Some(path) = &report_path {
            // Made empty and closed: the child writes it by path, and a
            // report still empty after the call is one nothing wrote.
            let profile = self.verified(Area::Profile)?;
            let made = open_at(
                &profile.file,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            )
            .map_err(|e| io_error(&format!("cannot make report {}", path.display()), e))?;
            names_inode(path, &made)?;
        }
        let borrow = store
            .create_tied(declared, Tie::Borrow(owner))
            .map_err(|e| io_error("cannot create a borrow lease", e))?;
        if let Some(fd) = borrow.fd() {
            arm(fd, program);
        }
        Ok(Call {
            lease: Some(borrow),
            spool: Some(Spool {
                path: spool_path,
                file,
            }),
            stderr: Some(Spool {
                path: stderr_path,
                file: stderr,
            }),
            stdin,
            report: report_path,
        })
    }

    /// Make the spool `name` in `area` at `path`, exclusively through the
    /// area's descriptor, and pin it: from now on it is read-only to every
    /// later call, admitted or not.
    fn make_spool(&mut self, area: Area, name: &str, path: &Path) -> Result<File, DevError> {
        let file = open_at(
            &self.verified(area)?.file,
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(|e| io_error(&format!("cannot make spool {}", path.display()), e))?;
        let pin = file
            .try_clone()
            .map_err(|e| io_error(&format!("cannot pin spool {}", path.display()), e))?;
        self.spools.push(PinnedSpool {
            path: path.to_path_buf(),
            file: pin,
        });
        names_inode(path, &file)?;
        Ok(file)
    }

    /// Open the file at `path` for brokkr's own reading (`read()`,
    /// `filesize()`): admitted as a call reading it would be (the read-only
    /// veto, authority, conflicts) inside the store's critical section, then
    /// opened against that admission without following a link, and refused
    /// unless it is a regular file.
    pub(crate) fn open_file(&mut self, path: &Path) -> Result<File, DevError> {
        let store = super::store().ok_or_else(|| {
            DevError::Config("$HOME is not set: the launch lease store has nowhere to live".into())
        })?;
        let _serial = super::serialize().map_err(|e| io_error("cannot open a file", e))?;
        let spelled = spell(path)?;
        self.admit_declared(
            &store,
            &[Resource::new(&spelled, Access::Read, Disposition::Reserve)],
        )?;
        self.open_admitted(path, &spelled)
    }

    /// Replace the output at `path` (`write()`) with `contents`, admitted as
    /// a call writing it would be. Through `out/`'s held descriptor: whatever
    /// entry the name holds is removed first, a directory refused, and the
    /// file is created afresh, exclusively and without following a link, so
    /// an entry a child made a hard link to another file is cut, never
    /// written through.
    pub(crate) fn write_file(&mut self, path: &Path, contents: &[u8]) -> Result<(), DevError> {
        let name = match self.place(path) {
            Some((Area::Out, name)) => name.to_owned(),
            _ => {
                return Err(DevError::Config(format!(
                    "{} is not an output of this iteration: only an output's file is written",
                    path.display()
                )));
            }
        };
        let store = super::store().ok_or_else(|| {
            DevError::Config("$HOME is not set: the launch lease store has nowhere to live".into())
        })?;
        let _serial = super::serialize().map_err(|e| io_error("cannot write a file", e))?;
        self.admit_declared(
            &store,
            &[Resource::new(
                &spell(path)?,
                Access::Write,
                Disposition::Reserve,
            )],
        )?;
        let area = self.verified(Area::Out)?;
        let failed = |e: io::Error| io_error(&format!("cannot write {}", path.display()), e);
        match unlink_at(&area.file, &name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) if e.raw_os_error() == Some(libc::EISDIR) => {
                return Err(DevError::Config(format!(
                    "{} is a directory: write() writes a file",
                    path.display()
                )));
            }
            Err(e) => return Err(failed(e)),
        }
        let mut file = open_at(
            &area.file,
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(failed)?;
        io::Write::write_all(&mut file, contents).map_err(failed)?;
        names_inode(path, &file)
    }

    /// Where `path` sits in the iteration directory, if it is one entry of
    /// an area: the handles carry exactly the paths brokkr made.
    fn place<'p>(&self, path: &'p Path) -> Option<(Area, &'p str)> {
        let name = path.file_name()?.to_str()?;
        let parent = path.parent()?;
        Area::ALL
            .into_iter()
            .find(|area| parent == self.dir.join(area.name()))
            .map(|area| (area, name))
    }

    /// Open `path` for reading, as admitted by `spelled`: an entry of the
    /// iteration through its area's held descriptor (a spool only as the
    /// inode it was made as), anything else by walking its resolved form
    /// without following a link, so a link substituted since the admission
    /// fails the open. Displacement is detected, not prevented: what a
    /// child does to the namespace meanwhile can only make the open fail.
    fn open_admitted(&self, path: &Path, spelled: &Spelled) -> Result<File, DevError> {
        let failed = |e: io::Error| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                DevError::Config(format!(
                    "{} is a link, which brokkr does not follow",
                    path.display()
                ))
            } else {
                io_error(&format!("cannot open {}", path.display()), e)
            }
        };
        let flags = libc::O_RDONLY | libc::O_NONBLOCK;
        let file = match self.place(path) {
            Some((Area::Scratch, _)) => {
                return Err(DevError::Config(format!(
                    "{} is a scratch directory, not a file",
                    path.display()
                )));
            }
            Some((area, name)) => {
                let dir = self.verified(area)?;
                let file = open_at(&dir.file, name, flags).map_err(failed)?;
                if area.spools() {
                    self.check_spool(path, &file)?;
                }
                // After the open too: an area moved since it was verified
                // makes the advertised path name something else.
                self.verified(area)?;
                still_names(path, &file)?;
                file
            }
            None => {
                let file = open_resolved(&spelled.resolved, flags).map_err(failed)?;
                still_names(&spelled.resolved, &file)?;
                file
            }
        };
        let meta = file.metadata().map_err(failed)?;
        if !meta.is_file() {
            return Err(DevError::Config(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        Ok(file)
    }

    /// Fail unless `file`, opened at the spool path `path`, is the spool
    /// this iteration made there.
    fn check_spool(&self, path: &Path, file: &File) -> Result<(), DevError> {
        let pinned = self.spools.iter().find(|s| s.path == path).ok_or_else(|| {
            DevError::Config(format!("{} is no spool of this iteration", path.display()))
        })?;
        let (a, b) = (file.metadata()?, pinned.file.metadata()?);
        if (a.dev(), a.ino()) == (b.dev(), b.ino()) {
            Ok(())
        } else {
            Err(DevError::Config(format!(
                "{} is no longer the spool its call wrote: something replaced it",
                path.display()
            )))
        }
    }

    /// Release the owner lease. The directory goes once no borrow of it is
    /// held; a held one keeps it, which is this call's error, and a later
    /// scan reclaims it.
    pub(crate) fn release(mut self) -> Result<(), DevError> {
        self.release_lease()
    }

    fn release_lease(&mut self) -> Result<(), DevError> {
        let Some(lease) = self.lease.take() else {
            return Ok(());
        };
        let _serial = super::serialize()
            .map_err(|e| io_error("the iteration could not release its lease", e))?;
        lease
            .release()
            .map_err(|e| io_error("the iteration could not release its lease", e))
    }

    /// Close this process's own descriptor without releasing, as a brokkr
    /// that died would.
    #[cfg(test)]
    pub(crate) fn abandon_for_test(mut self) {
        if let Some(mut lease) = self.lease.take() {
            lease.close_own_for_test();
        }
    }

    /// The three admission steps for `declared`, inside the caller's
    /// critical section: the read-only veto, authority, and the ordinary
    /// conflicts against every reservation but this iteration's owner.
    fn admit_declared(
        &self,
        store: &super::store::Store,
        declared: &[Resource],
    ) -> Result<(), DevError> {
        let owner = self
            .lease
            .as_ref()
            .ok_or_else(|| DevError::Internal("an admission on a released iteration".into()))?;
        let spools: Vec<PathBuf> = self.spools.iter().map(|s| s.path.clone()).collect();
        authorize(owner.resources(), &self.inputs, &spools, declared)?;
        let mut scan = store.scan();
        scan.reservations.retain(|r| r.id != owner.id());
        match conflict(&scan, declared) {
            Some(why) => Err(DevError::Config(why)),
            None => Ok(()),
        }
    }

    fn claim(&mut self, area: Area, name: &str) -> Result<(), DevError> {
        let one_component = !name.is_empty()
            && name != "."
            && name != ".."
            && !name.contains('/')
            && !name.contains('\0');
        if !one_component {
            return Err(DevError::Config(format!(
                "{name:?} is not a name for an iteration's {}: it must be one path component",
                area.name()
            )));
        }
        if !self.given.insert((area, name.to_owned())) {
            return Err(DevError::Config(format!(
                "{}/{name} was already given out in this iteration",
                area.name()
            )));
        }
        Ok(())
    }

    /// The area's descriptor, once the iteration directory and the area are
    /// still where their paths say: a displaced one fails the creation.
    fn verified(&self, area: Area) -> Result<&AreaDir, DevError> {
        let dir = self
            .areas
            .iter()
            .find(|a| a.area == area)
            .ok_or_else(|| DevError::Internal(format!("no {} area", area.name())))?;
        let path = self.dir.join(area.name());
        for (path, identity) in [(&self.dir, self.identity), (&path, dir.identity)] {
            let found = std::fs::symlink_metadata(path)
                .map(|m| m.is_dir() && (m.dev(), m.ino()) == identity)
                .unwrap_or(false);
            if !found {
                return Err(DevError::Config(format!(
                    "{} is no longer the directory this iteration made: something replaced it",
                    path.display()
                )));
            }
        }
        Ok(dir)
    }
}

impl Drop for Iteration {
    fn drop(&mut self) {
        if let Err(e) = self.release_lease() {
            crate::output::warn(&e.to_string());
        }
    }
}

/// One admitted call. Release it with [`Call::finish`] once its child has
/// been reaped (or never spawned).
#[derive(Debug)]
pub(crate) struct Call {
    lease: Option<Lease>,
    spool: Option<Spool>,
    stderr: Option<Spool>,
    stdin: Option<File>,
    report: Option<PathBuf>,
}

impl Call {
    /// A descriptor on the spool for the child's stderr.
    pub(crate) fn stderr(&self) -> io::Result<File> {
        match &self.stderr {
            Some(spool) => spool.file.try_clone(),
            None => Err(io::Error::other("the call's stderr spool was taken")),
        }
    }

    /// The stderr spool, kept by the caller apart from the call so it can be
    /// read whether or not the release succeeds. Take it after
    /// [`stderr`](Self::stderr) gave the child its descriptor.
    pub(crate) fn take_stderr_spool(&mut self) -> Option<Spool> {
        self.stderr.take()
    }

    /// The call's hotpath report path under `profile/`, when it asked for
    /// one: read it back with [`Iteration::open_file`] once the call is
    /// finished.
    pub(crate) fn report(&self) -> Option<&Path> {
        self.report.as_deref()
    }

    /// The child's stdin, opened at admission; `None` when the call has none
    /// (or it was taken already).
    pub(crate) fn take_stdin(&mut self) -> Option<File> {
        self.stdin.take()
    }

    /// A descriptor on the spool for the child's stdout.
    pub(crate) fn stdout(&self) -> io::Result<File> {
        match &self.spool {
            Some(spool) => spool.file.try_clone(),
            None => Err(io::Error::other("the call is finished")),
        }
    }

    /// Release the borrow. Its spool comes back only when no process holds
    /// the borrow any more: a borrow still held after the release window is
    /// this call's error, stays a live reservation and pins the iteration.
    pub(crate) fn finish(self) -> Result<Spool, DevError> {
        self.finish_observed(&mut || {})
    }

    /// [`finish`](Self::finish), calling `wait` while the release window
    /// waits for the borrow's last holder: a descendant still writing to
    /// something brokkr drains (the call's marker FIFO) is never left blocked
    /// on it until the window closes.
    pub(crate) fn finish_observed(mut self, wait: &mut dyn FnMut()) -> Result<Spool, DevError> {
        self.release(wait)?;
        self.spool
            .take()
            .ok_or_else(|| DevError::Internal("a call finished twice".into()))
    }

    fn release(&mut self, wait: &mut dyn FnMut()) -> Result<(), DevError> {
        let Some(lease) = self.lease.take() else {
            return Ok(());
        };
        if let Some(fd) = lease.fd() {
            disarm(fd);
        }
        let _serial =
            super::serialize().map_err(|e| io_error("the call could not release its borrow", e))?;
        lease
            .release_with(wait)
            .map_err(|e| io_error("the call could not release its borrow", e))
    }

    /// Close this process's own descriptor without releasing, as a brokkr
    /// that died would.
    #[cfg(test)]
    pub(crate) fn abandon_for_test(mut self) {
        if let Some(mut lease) = self.lease.take() {
            if let Some(fd) = lease.fd() {
                disarm(fd);
            }
            lease.close_own_for_test();
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if let Err(e) = self.release(&mut || {}) {
            crate::output::warn(&e.to_string());
        }
    }
}

/// A finished call's stdout, in full in its spool file.
#[derive(Debug)]
pub(crate) struct Spool {
    path: PathBuf,
    file: File,
}

/// The start of a spool and whether there was more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Preview {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

impl Spool {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The spool's size now, through its own descriptor.
    pub(crate) fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// The first `limit` bytes, read through the spool's own descriptor
    /// (never the path, which a child may have replaced), and whether the
    /// spool holds more. Truncation is a fact about the preview, not a
    /// failure.
    pub(crate) fn preview(&self, limit: usize) -> io::Result<Preview> {
        let len = self.file.metadata()?.len();
        let take = usize::try_from(len).unwrap_or(usize::MAX).min(limit);
        let mut bytes = vec![0; take];
        let mut at = 0;
        while at < take {
            let offset = u64::try_from(at).unwrap_or(u64::MAX);
            let n = self.file.read_at(&mut bytes[at..], offset)?;
            if n == 0 {
                break;
            }
            at += n;
        }
        bytes.truncate(at);
        Ok(Preview {
            truncated: len > u64::try_from(at).unwrap_or(u64::MAX),
            bytes,
        })
    }

    /// The last `limit` bytes as of one observation of the spool's size,
    /// read through its own descriptor. A writer that only appends gives a
    /// coherent snapshot; a file that shrank before the range was read is an
    /// error, never a shorter tail passed off as the end.
    pub(crate) fn tail(&self, limit: usize) -> io::Result<Tail> {
        let size = self.file.metadata()?.len();
        let take = usize::try_from(size).unwrap_or(usize::MAX).min(limit);
        let start = size - u64::try_from(take).unwrap_or(size);
        let mut bytes = vec![0; take];
        let mut at = 0;
        while at < take {
            let offset = start + u64::try_from(at).unwrap_or(u64::MAX);
            let n = self.file.read_at(&mut bytes[at..], offset)?;
            if n == 0 {
                return Err(io::Error::other(format!(
                    "the spool shrank below the {size} bytes observed while its end was read"
                )));
            }
            at += n;
        }
        Ok(Tail {
            bytes,
            size,
            cut: start > 0,
        })
    }
}

/// The end of a spool as observed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) bytes: Vec<u8>,
    /// The size observed.
    pub(crate) size: u64,
    /// Whether bytes before the tail were not read.
    pub(crate) cut: bool,
}

/// Spell an absolute path.
fn spell(path: &Path) -> Result<Spelled, DevError> {
    if !path.is_absolute() {
        return Err(DevError::Internal(format!(
            "an iteration resource must be absolute: {}",
            path.display()
        )));
    }
    Spelled::of(path, Path::new("/"))
        .map_err(|e| io_error(&format!("cannot resolve {}", path.display()), e))
}

/// A call's declared resources against its owner's: no write overlapping an
/// input (as the owner recorded it, and as the caller's spelling resolves
/// now) or a spool, and every resource, as it resolves now, inside an owner
/// resource that allows its access.
fn authorize(
    owner: &[Resource],
    inputs: &[PathBuf],
    spools: &[PathBuf],
    declared: &[Resource],
) -> Result<(), DevError> {
    let mut read_only: Vec<(Spelled, &Path)> = Vec::new();
    for input in owner.iter().filter(|r| r.access == Access::Read) {
        read_only.push((input.spelled(), &input.lexical));
    }
    // As the caller spelled them, resolved now: an input that cannot be
    // resolved refuses the admission rather than weaken the veto.
    for input in inputs {
        read_only.push((spell(input)?, input));
    }
    for spool in spools {
        read_only.push((spell(spool)?, spool));
    }
    for resource in declared {
        let spelled = resource.spelled();
        if resource.access == Access::Write
            && let Some((_, what)) = read_only.iter().find(|(r, _)| r.overlaps(&spelled))
        {
            return Err(DevError::Config(format!(
                "a call may not write {}: it overlaps {}, which is read-only for the run",
                resource.lexical.display(),
                what.display()
            )));
        }
        let authorized = owner.iter().any(|o| {
            (o.access == Access::Write || resource.access == Access::Read)
                && resource.resolved.starts_with(&o.resolved)
        });
        if !authorized {
            return Err(DevError::Config(format!(
                "{} ({}) is not within what the iteration holds for {}",
                resource.lexical.display(),
                resource.resolved.display(),
                match resource.access {
                    Access::Read => "reading",
                    Access::Write => "writing",
                }
            )));
        }
    }
    Ok(())
}

/// Open the just-made iteration directory without following a link, check
/// it is the one the lease recorded, and make its areas inside it.
fn lay_out(dir: &Path, identity: (u64, u64)) -> io::Result<Vec<AreaDir>> {
    let root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    let meta = root.metadata()?;
    if (meta.dev(), meta.ino()) != identity {
        return Err(io::Error::other("it was replaced before it was laid out"));
    }
    let mut areas = Vec::new();
    for area in Area::ALL {
        mkdir_at(&root, area.name())?;
        let file = open_dir_at(&root, area.name())?;
        let meta = file.metadata()?;
        areas.push(AreaDir {
            area,
            file,
            identity: (meta.dev(), meta.ino()),
        });
    }
    root.sync_all()?;
    Ok(areas)
}

/// Fail unless `path` names the inode `made` was opened on.
fn names_inode(path: &Path, made: &File) -> Result<(), DevError> {
    let made = made.metadata()?;
    let named = std::fs::symlink_metadata(path)
        .map(|m| (m.dev(), m.ino()) == (made.dev(), made.ino()))
        .unwrap_or(false);
    if named {
        Ok(())
    } else {
        Err(DevError::Config(format!(
            "{} does not name what this iteration just made there: something replaced it",
            path.display()
        )))
    }
}

/// Fail unless `path`, read without following a link at its end, names the
/// inode `opened` was opened on: a name replaced while brokkr opened it.
fn still_names(path: &Path, opened: &File) -> Result<(), DevError> {
    let opened = opened.metadata()?;
    let named = std::fs::symlink_metadata(path)
        .map(|m| (m.dev(), m.ino()) == (opened.dev(), opened.ino()))
        .unwrap_or(false);
    if named {
        Ok(())
    } else {
        Err(DevError::Config(format!(
            "{} no longer names the file brokkr opened there: something replaced it",
            path.display()
        )))
    }
}

fn c_name(name: impl AsRef<OsStr>) -> io::Result<CString> {
    CString::new(name.as_ref().as_bytes()).map_err(io::Error::other)
}

fn mkdir_at(parent: &File, name: impl AsRef<OsStr>) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: an open directory descriptor and a valid C string for the
    // call's duration.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `openat` below `parent`, never following a link in `name`.
fn open_at(parent: &File, name: impl AsRef<OsStr>, flags: libc::c_int) -> io::Result<File> {
    let name = c_name(name)?;
    let mode: libc::c_uint = 0o600;
    // SAFETY: as for `mkdir_at`; the returned descriptor is owned here.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_dir_at(parent: &File, name: impl AsRef<OsStr>) -> io::Result<File> {
    open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)
}

/// `unlinkat` below `parent`, never following a link and never removing a
/// directory (`EISDIR`).
fn unlink_at(parent: &File, name: impl AsRef<OsStr>) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: as for `mkdir_at`.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Open the absolute, already resolved `path` one component at a time from
/// `/`, following no link anywhere: a link put on the way since it was
/// resolved fails the open (`ELOOP` or `ENOTDIR`).
fn open_resolved(path: &Path, flags: libc::c_int) -> io::Result<File> {
    let mut names = Vec::new();
    for part in path.components() {
        match part {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => names.push(name),
            _ => return Err(io::Error::other("the path is not resolved")),
        }
    }
    let Some((last, parents)) = names.split_last() else {
        return Err(io::Error::other("the root is not a file"));
    };
    let mut dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open("/")?;
    for name in parents {
        dir = open_dir_at(&dir, name)?;
    }
    open_at(&dir, last, flags)
}
