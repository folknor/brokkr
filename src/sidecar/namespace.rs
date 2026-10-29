//! Where marker FIFOs live, how they are named, and how a FIFO a dead brokkr
//! left is told from a live one.
//!
//! A FIFO is `.sidecar-<token>.fifo`, the token a fresh 128-bit random hex
//! string, in the directory its creator was handed (the project's `.brokkr`
//! state root). A name is never reused and never removed before creation, so
//! two brokkrs that share the tree but not a PID namespace cannot take each
//! other's FIFO.
//!
//! A live FIFO always has a reader: its creator opens the read end before it
//! releases the namespace lock, reopens a new read end before dropping the
//! old one, and unlinks the name before the reader closes. So the sweep
//! removes a FIFO only when a non-blocking write open fails with `ENXIO`, no
//! reader at all. The one window that invariant leaves, between `mkfifo` and
//! the reader's open, is covered by `.sidecar.lock` beside the FIFOs: each
//! creator holds it shared across that window, the sweep holds it exclusive
//! across discovery, probe and unlink. The lock file is permanent; nothing
//! removes it. A probe that succeeds proves some reader exists, not that the
//! creating brokkr lives, and keeping such a FIFO is the conservative answer.
//! A brokkr from before this protocol takes no lock, so only its creation
//! window stays exposed to a concurrent sweep.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::DevError;

const PREFIX: &str = ".sidecar-";
const SUFFIX: &str = ".fifo";

/// The namespace lock's file name. Outside the FIFO envelope, so the sweep
/// never matches it.
pub(crate) const LOCK_FILE: &str = ".sidecar.lock";

/// How often a waiting acquisition retries.
const RETRY: Duration = Duration::from_millis(10);

/// The FIFO name for `token`.
pub(crate) fn fifo_name(token: &str) -> String {
    format!("{PREFIX}{token}{SUFFIX}")
}

/// Whether `name` has the reserved envelope: the prefix, a nonempty token,
/// the suffix. The token is never decoded, so a pid-named FIFO an older
/// brokkr left matches too.
fn is_fifo_name(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
        .is_some_and(|token| !token.is_empty())
}

/// How a held namespace lock is shared.
#[derive(Clone, Copy)]
pub(crate) enum Access {
    /// A creator: names are unique, so creators never conflict.
    Create,
    /// The sweep, excluding every creation window.
    Sweep,
}

/// One acquisition of a directory's namespace lock, on its own open file
/// description, so dropping one guard never releases another's.
pub(crate) struct NamespaceLock {
    _file: File,
}

impl NamespaceLock {
    /// Take `dir`'s lock. `wait` retries while another holds it, giving up
    /// with [`DevError::Interrupted`] once shutdown is requested; without it a
    /// held lock is `Ok(None)`.
    pub(crate) fn acquire(
        dir: &Path,
        access: Access,
        wait: bool,
    ) -> Result<Option<Self>, DevError> {
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| io_error("cannot open", &path, &e))?;
        let regular = file
            .metadata()
            .map_err(|e| io_error("cannot inspect", &path, &e))?
            .is_file();
        if !regular {
            return Err(DevError::Config(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let operation = match access {
            Access::Create => libc::LOCK_SH,
            Access::Sweep => libc::LOCK_EX,
        } | libc::LOCK_NB;
        loop {
            // SAFETY: an owned open descriptor, no pointers.
            if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                return Ok(Some(Self { _file: file }));
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(io_error("cannot lock", &path, &e));
            }
            if !wait {
                return Ok(None);
            }
            if crate::shutdown::should_unwind() {
                return Err(DevError::Interrupted);
            }
            std::thread::sleep(RETRY);
        }
    }
}

/// The FIFOs in `dir` no process reads: reserved name, FIFO type, and a
/// non-blocking write open refused with `ENXIO`. Call it under the sweep's
/// [`NamespaceLock`]. An unreadable directory has none; a probe failing any
/// other way keeps the FIFO.
pub(crate) fn abandoned(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_fifo()))
        .filter(|entry| entry.file_name().to_str().is_some_and(is_fifo_name))
        .map(|entry| entry.path())
        .filter(|path| has_no_reader(path))
        .collect()
}

fn has_no_reader(path: &Path) -> bool {
    match OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        // A reader holds it; the probe writes nothing and closes at once.
        Ok(_) => false,
        Err(e) => e.raw_os_error() == Some(libc::ENXIO),
    }
}

fn io_error(action: &str, path: &Path, source: &io::Error) -> DevError {
    DevError::Io(io::Error::new(
        source.kind(),
        format!("{action} {}: {source}", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn mkfifo(path: &Path) {
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    fn open_reader(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .unwrap()
    }

    #[test]
    fn the_envelope_takes_any_nonempty_token_and_nothing_else() {
        assert!(is_fifo_name(&fifo_name("0123abcd")));
        assert!(is_fifo_name(".sidecar-1394754.fifo"));
        assert!(!is_fifo_name(".sidecar-.fifo"));
        assert!(!is_fifo_name(LOCK_FILE));
        assert!(!is_fifo_name(".sidecar-x.fifo.new"));
    }

    #[test]
    fn only_a_reserved_fifo_without_a_reader_is_abandoned() {
        let dir = crate::test_scratch::scratch("sidecar_namespace", "abandoned");
        let dead = dir.join(fifo_name("dead"));
        let live = dir.join(fifo_name("live"));
        let foreign = dir.join("other.fifo");
        let plain = dir.join(fifo_name("plain"));
        mkfifo(&dead);
        mkfifo(&live);
        mkfifo(&foreign);
        std::fs::write(&plain, b"").unwrap();
        let reader = open_reader(&live);

        assert_eq!(abandoned(&dir), vec![dead.clone()]);

        drop(reader);
        let mut found = abandoned(&dir);
        found.sort();
        let mut expected = vec![dead, live];
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn a_probe_leaves_a_live_reader_its_records() {
        use std::io::{Read as _, Write as _};
        let dir = crate::test_scratch::scratch("sidecar_namespace", "probe_inert");
        let path = dir.join(fifo_name("t"));
        mkfifo(&path);
        let mut reader = open_reader(&path);
        let mut writer = OpenOptions::new().write(true).open(&path).unwrap();
        writer.write_all(b"1 A\n").unwrap();
        assert!(abandoned(&dir).is_empty());
        drop(writer);
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"1 A\n");
    }

    #[test]
    fn the_sweep_excludes_creators_and_creators_share() {
        let dir = crate::test_scratch::scratch("sidecar_namespace", "lock");
        let first = NamespaceLock::acquire(&dir, Access::Create, false).unwrap();
        assert!(first.is_some());
        assert!(
            NamespaceLock::acquire(&dir, Access::Create, false)
                .unwrap()
                .is_some()
        );
        assert!(
            NamespaceLock::acquire(&dir, Access::Sweep, false)
                .unwrap()
                .is_none()
        );
        drop(first);
        let sweep = NamespaceLock::acquire(&dir, Access::Sweep, false).unwrap();
        assert!(sweep.is_some());
        assert!(
            NamespaceLock::acquire(&dir, Access::Create, false)
                .unwrap()
                .is_none()
        );
    }
}
