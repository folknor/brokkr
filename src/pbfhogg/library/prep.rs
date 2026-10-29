//! Dataset page-cache preparation (`docs/commands/library.md`, Isolation
//! scenarios): before each arm of a library against CLI measurement, outside
//! both timed intervals, every filesystem is synced, the arm's dataset files
//! and the previous arm's retained outputs are advised out of the page cache,
//! and then every dataset file is scanned to have no resident page.
//!
//! What it establishes is narrow and stated so: input page-cache residency
//! checked clear before launch, assuming no competing reader. A zero scan
//! does not prove no read in flight (the kernel reports a page under read as
//! not resident), and nothing here resets device, controller, remote or
//! filesystem metadata caches.

use std::path::{Path, PathBuf};

/// The preparation this brokkr performs, as stored rows name it.
pub(crate) const PROCEDURE: &str = "dataset-page-cache-v1";

/// Bytes of a file one scan window maps.
const WINDOW_BYTES: u64 = 1 << 30;

/// Why a preparation did not establish its state.
#[derive(Debug)]
pub(crate) enum NotEstablished {
    /// The residency of a dataset file could not be shown clear.
    Residency(String),
    /// A stop request arrived between scan windows.
    Interrupted,
}

impl std::fmt::Display for NotEstablished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Residency(why) => write!(f, "dataset page-cache preparation not established: {why}"),
            Self::Interrupted => f.write_str("dataset page-cache preparation interrupted"),
        }
    }
}

/// Prepare one arm: sync every filesystem, advise `datasets` and every
/// regular file under `retained` out of the page cache, then scan each of
/// `datasets` for resident pages, all evictions before any scan.
pub(crate) fn prepare(datasets: &[PathBuf], retained: &[PathBuf]) -> Result<(), NotEstablished> {
    // SAFETY: sync takes no arguments and cannot fail.
    unsafe { libc::sync() };
    let mut files: Vec<PathBuf> = datasets.to_vec();
    for path in retained {
        collect_files(path, &mut files).map_err(NotEstablished::Residency)?;
    }
    for file in &files {
        evict(file).map_err(NotEstablished::Residency)?;
    }
    for file in datasets {
        if crate::shutdown::should_unwind() {
            return Err(NotEstablished::Interrupted);
        }
        let resident = resident_pages(file)?;
        if resident > 0 {
            let why = if observable(file) {
                format!(
                    "{} still has {} in the page cache after eviction",
                    file.display(),
                    crate::output::count(usize::try_from(resident).unwrap_or(usize::MAX), "resident page")
                )
            } else {
                format!(
                    "the page-cache residency of {} is not observable: brokkr neither owns it \
                     nor may write it, and the kernel then reports every page resident",
                    file.display()
                )
            };
            return Err(NotEstablished::Residency(why));
        }
    }
    Ok(())
}

/// Every regular file at or under `path`, never following a link; an absent
/// path holds none.
fn collect_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: cannot inspect: {e}", path.display())),
    };
    if meta.file_type().is_file() {
        out.push(path.to_path_buf());
    } else if meta.file_type().is_dir() {
        let entries =
            std::fs::read_dir(path).map_err(|e| format!("{}: cannot list: {e}", path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: cannot list: {e}", path.display()))?;
            collect_files(&entry.path(), out)?;
        }
    }
    Ok(())
}

fn open(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{}: cannot open: {e}", path.display()))
}

/// Advise the whole of `path` out of the page cache.
fn evict(path: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd as _;
    let file = open(path)?;
    // SAFETY: a valid descriptor; offset and length 0 cover the whole file.
    let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "{}: cannot advise it out of the page cache: {}",
            path.display(),
            std::io::Error::from_raw_os_error(rc)
        ))
    }
}

/// The pages of `path` the page cache holds, scanned in bounded windows of
/// an untouched read-only mapping, interruptible between windows.
fn resident_pages(path: &Path) -> Result<u64, NotEstablished> {
    use std::os::fd::AsRawFd as _;
    let fail = |why: String| NotEstablished::Residency(why);
    let file = open(path).map_err(fail)?;
    let len = file
        .metadata()
        .map_err(|e| fail(format!("{}: cannot inspect: {e}", path.display())))?
        .len();
    // SAFETY: sysconf with a valid name.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| fail("the page size is unknown".to_owned()))?;
    let window = WINDOW_BYTES - WINDOW_BYTES % page;
    let mut resident = 0u64;
    let mut offset = 0u64;
    while offset < len {
        if crate::shutdown::should_unwind() {
            return Err(NotEstablished::Interrupted);
        }
        let span = (len - offset).min(window);
        let map_len = usize::try_from(span)
            .map_err(|_| fail(format!("{}: a window does not fit memory", path.display())))?;
        let pages = usize::try_from(span.div_ceil(page))
            .map_err(|_| fail(format!("{}: a window does not fit memory", path.display())))?;
        let map_offset = libc::off_t::try_from(offset)
            .map_err(|_| fail(format!("{}: offset {offset} is out of range", path.display())))?;
        // SAFETY: a read-only shared mapping of a range inside the file,
        // never touched, unmapped below.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                map_offset,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(fail(format!(
                "{}: cannot map it to scan: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        let mut vec = vec![0u8; pages];
        // SAFETY: `addr` maps `map_len` bytes and `vec` holds one byte per
        // page of it.
        let rc = unsafe { libc::mincore(addr, map_len, vec.as_mut_ptr()) };
        let error = std::io::Error::last_os_error();
        // SAFETY: the mapping made above, unmapped once.
        unsafe { libc::munmap(addr, map_len) };
        if rc != 0 {
            return Err(fail(format!(
                "{}: cannot read its page-cache residency: {error}",
                path.display()
            )));
        }
        resident += vec.iter().filter(|b| **b & 1 == 1).count() as u64;
        offset += span;
    }
    Ok(resident)
}

/// Whether the kernel reports the true residency of `path` to brokkr: it
/// does to the owner, to a process that may write the file and to a
/// privileged one, and reports every page resident to anyone else.
fn observable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: geteuid cannot fail.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        return true;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.uid() == euid) {
        return true;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a NUL-terminated path.
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_prepared_file_has_no_resident_page() {
        let dir = crate::test_scratch::scratch("pbfhogg-library-prep", "evict");
        let file = dir.join("input.osm.pbf");
        std::fs::write(&file, vec![7u8; 3 * 4096 + 17]).unwrap();
        // Read it so its pages are cached, then prepare.
        assert_eq!(std::fs::read(&file).unwrap().len(), 3 * 4096 + 17);
        match prepare(std::slice::from_ref(&file), &[dir.join("absent")]) {
            Ok(()) => assert_eq!(resident_pages(&file).unwrap(), 0),
            // A filesystem that keeps file pages in memory (tmpfs) cannot be
            // prepared; the refusal must say so with its evidence.
            Err(NotEstablished::Residency(why)) => {
                assert!(why.contains("resident page"), "{why}");
            }
            Err(NotEstablished::Interrupted) => panic!("interrupted"),
        }
    }

    #[test]
    fn an_empty_file_scans_clear_and_retained_directories_are_walked() {
        let dir = crate::test_scratch::scratch("pbfhogg-library-prep", "walk");
        let empty = dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(resident_pages(&empty).unwrap(), 0);
        std::fs::create_dir_all(dir.join("out/sub")).unwrap();
        std::fs::write(dir.join("out/sub/a"), b"x").unwrap();
        let mut files = Vec::new();
        collect_files(&dir.join("out"), &mut files).unwrap();
        assert_eq!(files, [dir.join("out/sub/a")]);
    }
}
