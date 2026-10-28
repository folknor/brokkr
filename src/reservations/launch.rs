//! Admission and release of one launch: the lease, the private directory and
//! the descriptor the child inherits.

use std::cell::RefCell;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use super::paths::Spelled;
use super::store::{Access, Disposition, Lease, Resource};
use crate::error::DevError;

/// The argv value standing for the launch's private directory until the
/// launch exists. Recorded `cli_args` keep it, so a row does not change with
/// the random name each launch gets. Spelled with no shell metacharacter, so
/// a pasted `cli_args` line is not a redirect.
pub(crate) const PRIVATE_DIR_PLACEHOLDER: &str = "@brokkr-private-dir@";

/// The private directory's name prefix; a 128-bit random token follows.
pub(crate) const PRIVATE_DIR_PREFIX: &str = ".brokkr-pbfhogg-";

/// What one launch uses, declared with its argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchSpec {
    cwd: PathBuf,
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
    private_in: Option<PathBuf>,
}

impl LaunchSpec {
    /// `reads` and `writes` relative to `cwd`, the directory the child runs
    /// in. `private_in`: the directory the private directory is made in, when
    /// the argv carries [`PRIVATE_DIR_PLACEHOLDER`].
    pub(crate) fn new(
        cwd: PathBuf,
        reads: Vec<PathBuf>,
        writes: Vec<PathBuf>,
        private_in: Option<PathBuf>,
    ) -> Self {
        Self { cwd, reads, writes, private_in }
    }

    #[cfg(test)]
    pub(crate) fn private_in(&self) -> Option<&Path> {
        self.private_in.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn reads(&self) -> &[PathBuf] {
        &self.reads
    }

    #[cfg(test)]
    pub(crate) fn writes(&self) -> &[PathBuf] {
        &self.writes
    }

    /// Admit one launch of `program` with `args` in `cwd`: refuse when an
    /// existing reservation conflicts, make the lease and the private
    /// directory, and arm the lease descriptor for the next spawn of
    /// `program` on this thread. `disposable` are files the launcher adds to
    /// this one launch beside the argv (a profiler's report, at a fresh
    /// path): written by the child and removed with the lease, by its release
    /// or by a later scan, like the private directory.
    pub(crate) fn launch(
        &self,
        program: &Path,
        args: &[String],
        cwd: &Path,
        disposable: &[PathBuf],
    ) -> Result<Launched, DevError> {
        if cwd != self.cwd {
            return Err(DevError::Internal(format!(
                "a launch declared for {} was run in {}",
                self.cwd.display(),
                cwd.display()
            )));
        }
        let store = super::store().ok_or_else(|| {
            DevError::Config("$HOME is not set: the launch lease store has nowhere to live".into())
        })?;
        let _serial = super::serialize().map_err(|e| io_error("cannot admit a launch", e))?;
        let scan = store.scan();
        let spell = |path: &Path| {
            Spelled::of(path, &self.cwd)
                .map_err(|e| io_error(&format!("cannot resolve launch path {}", path.display()), e))
        };
        let mut resources: Vec<Resource> = Vec::new();
        for (path, access) in self.reads.iter().map(|p| (p, Access::Read)).chain(self.writes.iter().map(|p| (p, Access::Write))) {
            resources.push(Resource::new(&spell(path)?, access, Disposition::Reserve));
        }
        for path in disposable {
            resources.push(Resource::new(&spell(path)?, Access::Write, Disposition::Private));
        }
        let private = match &self.private_in {
            Some(location) => {
                check_placement(location, &self.writes, &self.cwd)?;
                let token = crate::lockfile::RunToken::mint()
                    .ok_or_else(|| DevError::Internal("no entropy for a private directory name".into()))?;
                let dir = location.join(format!("{PRIVATE_DIR_PREFIX}{}", token.as_str()));
                // Its argv spelling first: nothing after the lease exists may
                // fail without releasing it.
                let spelled = crate::utf8_path::path_string(&dir)?;
                resources.push(Resource::new(&spell(&dir)?, Access::Write, Disposition::Private));
                Some((dir, spelled))
            }
            None => None,
        };
        refuse_conflicts(&scan, &resources)?;
        let mut lease = store.create(resources).map_err(|e| io_error("cannot create a launch lease", e))?;
        if let Some((dir, _)) = &private {
            let index = lease.resources().len() - 1;
            match std::fs::create_dir(dir) {
                Err(e) => {
                    // Never created by this launch (an existing path among
                    // them): `forget` first, so the release cannot remove it.
                    let forgotten = lease.forget(index);
                    let released = lease.release();
                    return Err(io_error(
                        &format!("cannot create the private directory {}", dir.display()),
                        forgotten.and(released).err().unwrap_or(e),
                    ));
                }
                Ok(()) => {
                    if let Err(e) = record_private(&mut lease, index, dir) {
                        // Created by this launch and named by its record: the
                        // release removes it with the lease, or a later scan
                        // does.
                        let released = lease.release();
                        return Err(io_error(
                            &format!("cannot record the private directory {}", dir.display()),
                            released.err().unwrap_or(e),
                        ));
                    }
                }
            }
        }
        let args = match &private {
            Some((_, dir)) => args
                .iter()
                .map(|a| if a == PRIVATE_DIR_PLACEHOLDER { dir.clone() } else { a.clone() })
                .collect(),
            None => args.to_vec(),
        };
        if let Some(fd) = lease.fd() {
            arm(fd, program);
        }
        Ok(Launched { args, lease: Some(lease) })
    }
}

/// One admitted launch. Release it with [`Launched::finish`] once its child
/// has been reaped (or never spawned).
#[derive(Debug)]
pub(crate) struct Launched {
    pub(crate) args: Vec<String>,
    lease: Option<Lease>,
}

impl Launched {
    /// A launch with nothing declared: the argv runs as given.
    pub(crate) fn bare(args: Vec<String>) -> Self {
        Self { args, lease: None }
    }

    pub(crate) fn arg_refs(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    /// Close this process's own descriptor on the lease, as a test of what
    /// the child's inherited one holds alone.
    #[cfg(test)]
    pub(crate) fn close_own_hold_for_test(&mut self) {
        if let Some(lease) = self.lease.as_mut() {
            lease.close_own_for_test();
        }
    }

    /// Release the lease. A lease another process still holds is kept as a
    /// live reservation and is this launch's error.
    pub(crate) fn finish(mut self) -> Result<(), DevError> {
        self.release()
    }

    fn release(&mut self) -> Result<(), DevError> {
        let Some(lease) = self.lease.take() else {
            return Ok(());
        };
        if let Some(fd) = lease.fd() {
            disarm(fd);
        }
        let _serial = super::serialize().map_err(|e| io_error("the launch could not release its lease", e))?;
        lease.release().map_err(|e| io_error("the launch could not release its lease", e))
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        if let Err(e) = self.release() {
            crate::output::warn(&e.to_string());
        }
    }
}

#[allow(clippy::needless_pass_by_value)] // Called with the error a `map_err` hands over.
fn io_error(context: &str, e: io::Error) -> DevError {
    DevError::Io(io::Error::new(e.kind(), format!("{context}: {e}")))
}

/// Refuse when any reservation's resource overlaps one of `ours` and either
/// side writes it. Unknown leases refuse outright.
fn refuse_conflicts(scan: &super::store::Scan, ours: &[Resource]) -> Result<(), DevError> {
    match conflict(scan, ours) {
        Some(why) => Err(DevError::Config(why)),
        None => Ok(()),
    }
}

fn conflict(scan: &super::store::Scan, ours: &[Resource]) -> Option<String> {
    if let Some((id, why)) = scan.unknown.first() {
        return Some(format!(
            "lease {id} in the launch lease store cannot be established ({why}); nothing is \
             admitted until it is resolved"
        ));
    }
    for reservation in &scan.reservations {
        for theirs in &reservation.resources {
            for mine in ours {
                if (theirs.access == Access::Write || mine.access == Access::Write)
                    && theirs.spelled().overlaps(&mine.spelled())
                {
                    return Some(format!(
                        "{} overlaps {} of lease {}, still {}",
                        mine.lexical.display(),
                        theirs.lexical.display(),
                        reservation.id,
                        if reservation.retained.is_some() { "kept after a failed cleanup" } else { "held" },
                    ));
                }
            }
        }
    }
    None
}

/// A write reservation this process holds with no child: a staged write's
/// temp file and target, from before its first byte until its commit or
/// drop (`atomic_write::Staged`).
#[derive(Debug)]
pub(crate) struct Hold {
    lease: Option<Lease>,
}

/// Reserve `paths` for writing, refused like a launch that writes them.
pub(crate) fn hold(paths: &[&Path]) -> io::Result<Hold> {
    let Some(store) = super::store() else {
        // No store to hold in ($HOME unset): the gate's own refusals still
        // apply, and no launch can be admitted either.
        super::guard::mutate(paths, &[], || Ok::<_, io::Error>(()))?;
        return Ok(Hold { lease: None });
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let _serial = super::serialize()?;
    let scan = store.scan();
    let resources = paths
        .iter()
        .map(|path| Spelled::of(path, &cwd).map(|s| Resource::new(&s, Access::Write, Disposition::Reserve)))
        .collect::<io::Result<Vec<_>>>()?;
    if let Some(why) = conflict(&scan, &resources) {
        return Err(super::guard::busy(format!("refusing to write: {why}")));
    }
    Ok(Hold { lease: Some(store.create(resources)?) })
}

impl Hold {
    /// The next spawn of `program` on this thread is the writer: it inherits
    /// the reservation (with the parent-death signal and the orphan token, at
    /// the spawn choke point), so the reservation outlives this process as
    /// long as the writer does.
    pub(crate) fn lend_to(&self, program: &Path) {
        if let Some(fd) = self.lease.as_ref().and_then(Lease::fd) {
            arm(fd, program);
        }
    }

    /// Release the reservation and run `op`, a mutation of `targets`, under
    /// the gate, with no launch admitted in between.
    pub(crate) fn release_then<T>(mut self, targets: &[&Path], op: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let lease = self.lease.take();
        if let Some(fd) = lease.as_ref().and_then(Lease::fd) {
            disarm(fd);
        }
        let _serial = super::serialize()?;
        if let Some(lease) = lease {
            lease.release()?;
        }
        super::guard::refuse(targets, &[])?;
        op()
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else { return };
        if let Some(fd) = lease.fd() {
            disarm(fd);
        }
        // Without the critical section the lease is dropped unreleased: its
        // descriptor closes, so a later scan finds it free and reclaims it.
        let released = super::serialize().and_then(|_serial| lease.release());
        if let Err(e) = released {
            crate::output::warn(&format!("a staged write could not release its reservation: {e}"));
        }
    }
}

/// The private directory's location must exist and sit on the same mount as
/// every output's directory, or a rename from it could not publish.
fn check_placement(location: &Path, writes: &[PathBuf], cwd: &Path) -> Result<(), DevError> {
    let here = mount_identity(location).map_err(|e| {
        io_error(&format!("cannot place a private directory in {}", location.display()), e)
    })?;
    for output in writes {
        let absolute = cwd.join(output);
        let Some(parent) = absolute.parent() else { continue };
        let there = mount_identity(parent).map_err(|e| {
            io_error(&format!("cannot inspect the directory of output {}", output.display()), e)
        })?;
        if !here.same_mount(&there) {
            return Err(DevError::Config(format!(
                "output {} is on another mount than {}, where its private directory would go; \
                 pbfhogg publishes by rename, so one launch cannot write across mounts",
                output.display(),
                location.display()
            )));
        }
    }
    Ok(())
}

/// Make the just-created private directory durable and record its identity.
fn record_private(lease: &mut Lease, index: usize, dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    let meta = std::fs::symlink_metadata(dir)?;
    lease.set_identity(index, (meta.dev(), meta.ino()))
}

struct MountIdentity {
    dev: u64,
    mount: Option<u64>,
}

impl MountIdentity {
    fn same_mount(&self, other: &Self) -> bool {
        self.dev == other.dev && (self.mount.is_none() || other.mount.is_none() || self.mount == other.mount)
    }
}

/// The device and, where the kernel reports it, the mount id of `path`.
fn mount_identity(path: &Path) -> io::Result<MountIdentity> {
    use std::os::unix::ffi::OsStrExt as _;
    let meta = std::fs::metadata(path)?;
    if !meta.is_dir() {
        return Err(io::Error::other(format!("{} is not a directory", path.display())));
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: a zeroed statx is a valid output buffer; the path is a valid C
    // string for the call's duration.
    let mut stx: libc::statx = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statx(libc::AT_FDCWD, c_path.as_ptr(), 0, libc::STATX_MNT_ID, &raw mut stx) };
    let mount = (rc == 0 && stx.stx_mask & libc::STATX_MNT_ID != 0).then_some(stx.stx_mnt_id);
    Ok(MountIdentity { dev: meta.dev(), mount })
}

// ---------------------------------------------------------------------------
// The descriptor handoff
// ---------------------------------------------------------------------------

struct Armed {
    fd: RawFd,
    program: PathBuf,
}

thread_local! {
    /// The lease descriptor the next spawn of its program on this thread
    /// inherits. Thread-local, so a spawn on another thread cannot take it.
    static ARMED: RefCell<Option<Armed>> = const { RefCell::new(None) };
}

fn arm(fd: RawFd, program: &Path) {
    ARMED.with(|slot| *slot.borrow_mut() = Some(Armed { fd, program: program.to_path_buf() }));
}

/// Clear the slot if it still holds `fd`, a lease about to be released whose
/// child was never spawned; another lease armed since is left armed.
fn disarm(fd: RawFd) {
    ARMED.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|armed| armed.fd == fd) {
            *slot = None;
        }
    });
}

/// The lease descriptor armed for `program`, taken: the spawn choke point
/// makes it inheritable for that one child. `None` for any other program.
pub(crate) fn take_armed(program: &std::ffi::OsStr) -> Option<RawFd> {
    ARMED.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|armed| armed.program.as_os_str() == program) {
            slot.take().map(|armed| armed.fd)
        } else {
            None
        }
    })
}

/// Make `fd` survive the exec of `cmd`'s child only, keeping its other
/// descriptor flags. A failure aborts that spawn.
pub(crate) fn inherit_in_child(cmd: &mut std::process::Command, fd: RawFd) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: fcntl is async-signal-safe and touches only the child's copy of
    // the descriptor table between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The full cycle against a real child spawned through the choke point
    /// (`process::spawn::spawn`): the launch makes the private directory and
    /// substitutes it; with the parent's own descriptor closed, only the
    /// child's inherited one can keep the lease held, and a conflicting launch
    /// is refused while it runs; the release after it exits removes the
    /// directory.
    #[test]
    fn a_launch_lends_its_lease_to_the_child_and_releases_after_it() {
        let _serial = crate::test_scratch::process_global_lock();
        let root = crate::test_scratch::scratch("reservations", "launch-cycle");
        let out = root.join("out.pbf");
        let spec = LaunchSpec::new(root.clone(), Vec::new(), vec![out.clone()], Some(root.clone()));
        let program = Path::new("/bin/sh");
        let args = vec![
            "-c".to_owned(),
            "sleep 0.5; test -d \"$1\"".to_owned(),
            "sh".to_owned(),
            PRIVATE_DIR_PLACEHOLDER.to_owned(),
        ];
        let mut launched = spec.launch(program, &args, &root, &[]).unwrap();
        let dir = PathBuf::from(&launched.args[3]);
        assert!(dir.starts_with(&root) && dir.is_dir(), "{}", dir.display());
        let mut cmd = std::process::Command::new(program);
        cmd.args(&launched.args);
        let mut child = crate::process::spawn::spawn(cmd, None).unwrap();
        assert!(take_armed(program.as_os_str()).is_none(), "the spawn took the descriptor");
        launched.close_own_hold_for_test();

        let rival = LaunchSpec::new(root.clone(), vec![out.clone()], Vec::new(), None);
        assert!(
            rival.launch(program, &[], &root, &[]).is_err(),
            "the child's inherited descriptor alone keeps the lease held"
        );

        assert!(child.wait().unwrap().success(), "the child saw its private directory");
        launched.finish().unwrap();
        assert!(!dir.exists(), "released after the child exited");
        assert!(rival.launch(program, &[], &root, &[]).unwrap().finish().is_ok());
    }

    /// A disposable file the launcher names is reserved while the launch
    /// lives and removed with its lease.
    #[test]
    fn disposable_files_are_reserved_then_removed_with_the_launch() {
        let _serial = crate::test_scratch::process_global_lock();
        let root = crate::test_scratch::scratch("reservations", "launch-extra");
        let report = root.join("report.json");
        let program = Path::new("/bin/true");
        let spec = LaunchSpec::new(root.clone(), Vec::new(), Vec::new(), None);
        let launched = spec.launch(program, &[], &root, std::slice::from_ref(&report)).unwrap();
        std::fs::write(&report, b"{}").unwrap();
        let err = super::super::guard::remove_file(&report).unwrap_err();
        assert!(super::super::guard::is_refusal(&err), "{err}");
        launched.finish().unwrap();
        assert!(!report.exists(), "removed with the lease");
    }

    #[test]
    fn an_armed_descriptor_goes_only_to_its_own_program() {
        arm(42, Path::new("/x/pbfhogg"));
        assert_eq!(take_armed(std::ffi::OsStr::new("/x/osmium")), None);
        assert_eq!(take_armed(std::ffi::OsStr::new("/x/pbfhogg")), Some(42));
        assert_eq!(take_armed(std::ffi::OsStr::new("/x/pbfhogg")), None, "taken once");
    }
}
