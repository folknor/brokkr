//! Page-cache preparation of a script bench call's inputs (`cold` and
//! `warm`, `docs/commands/bench.md`), and the prerequisites a preflight
//! checks of them (`page_cache()`).
//!
//! Everything acts on a descriptor brokkr opened, never on a path again.
//! Every file of a call is prepared before any is scanned, so preparing one
//! cannot silently undo the observation of another. What a scan reports is
//! what brokkr accumulated while it queried every page of each file, not
//! one simultaneous snapshot; the kernel reports a page under read as not
//! resident, and nothing here keeps the cache as it was found once the
//! child runs.
//!
//! Residency is believed only where the kernel reports it truthfully. It
//! does to a process that owns the file or is capable over it, or that may
//! write it, and reports every page resident to anyone else, which would
//! pass a warm check on a fabricated answer. brokkr asks the one condition
//! it can establish on the descriptor, that its effective credentials may
//! write the file, and refuses the rest: conservative, since an owner of a
//! read-only file is refused although the kernel would answer it. It is
//! asked before and again after the scan, so eligibility lost at either end
//! is caught; that permissions stay as they are in between is assumed.

use std::fs::File;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{FileExt as _, MetadataExt as _};

use crate::bench::store::{CacheFile, CacheMode};

/// Bytes of a file one scan window maps.
const SCAN_WINDOW: u64 = 1 << 30;

/// Bytes one warming read asks for.
const READ_WINDOW: usize = 8 << 20;

/// Why a preparation or a check did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Interrupted,
    Failed(String),
}

/// One file a call prepares: every path it was listed under, the mode, and
/// the descriptor brokkr opened it through.
pub(crate) struct Target {
    pub(crate) paths: Vec<String>,
    pub(crate) mode: CacheMode,
    pub(crate) file: File,
}

/// What became of one file, for a failure's account.
enum State {
    NotAttempted,
    Prepared,
    Scanned(u64),
    Failed(String),
}

/// Prepare every target, then scan each, then check eligibility and sizes
/// again. `stop` is asked between windows. Returns each file's account and
/// the scan interval as instants, or a failure whose reason accounts for
/// every file.
pub(crate) fn prepare(
    targets: &[Target],
    stop: &dyn Fn() -> bool,
) -> Result<(Vec<CacheFile>, std::time::Instant, std::time::Instant), Failure> {
    let mut states: Vec<State> = targets.iter().map(|_| State::NotAttempted).collect();
    let account = |states: &[State], why: String| account(targets, states, &why);
    let mut sizes = Vec::with_capacity(targets.len());
    for (idx, target) in targets.iter().enumerate() {
        let step = observable(&target.file)
            .and_then(|()| size(&target.file))
            .and_then(|size| {
                match target.mode {
                    CacheMode::Cold => evict(&target.file),
                    CacheMode::Warm => warm(&target.file, size, stop),
                }
                .map(|()| size)
            });
        match step {
            Ok(size) => {
                sizes.push(size);
                states[idx] = State::Prepared;
            }
            Err(Failure::Interrupted) => return Err(Failure::Interrupted),
            Err(Failure::Failed(why)) => {
                states[idx] = State::Failed(why);
                return Err(account(&states, "a file could not be prepared".to_owned()));
            }
        }
    }
    let scan_from = std::time::Instant::now();
    let mut resident = Vec::with_capacity(targets.len());
    for (idx, target) in targets.iter().enumerate() {
        match resident_bytes(&target.file, sizes[idx], stop) {
            Ok(bytes) => {
                resident.push(bytes);
                states[idx] = State::Scanned(bytes);
            }
            Err(Failure::Interrupted) => return Err(Failure::Interrupted),
            Err(Failure::Failed(why)) => {
                states[idx] = State::Failed(why);
                return Err(account(&states, "a file could not be scanned".to_owned()));
            }
        }
    }
    let scan_to = std::time::Instant::now();
    for (idx, target) in targets.iter().enumerate() {
        let after = observable(&target.file).and_then(|()| size(&target.file));
        match after {
            Ok(now) if now == sizes[idx] => {}
            Ok(now) => {
                states[idx] = State::Failed(format!(
                    "its size changed from {} to {now} bytes during preparation",
                    sizes[idx]
                ));
                return Err(account(&states, "a file changed".to_owned()));
            }
            Err(Failure::Interrupted) => return Err(Failure::Interrupted),
            Err(Failure::Failed(why)) => {
                states[idx] = State::Failed(format!("after the scan, {why}"));
                return Err(account(
                    &states,
                    "a file's residency stopped being observable".to_owned(),
                ));
            }
        }
    }
    let mut files = Vec::with_capacity(targets.len());
    for (idx, target) in targets.iter().enumerate() {
        let (bytes, found) = (sizes[idx], resident[idx]);
        if let Some(why) = unestablished(target.mode, bytes, found) {
            states[idx] = State::Failed(why);
            return Err(account(
                &states,
                "a file is not in the state asked".to_owned(),
            ));
        }
        files.push(CacheFile {
            paths: target.paths.clone(),
            mode: target.mode,
            bytes,
            resident: found,
        });
    }
    Ok((files, scan_from, scan_to))
}

/// A failure naming `why` and what became of every file.
fn account(targets: &[Target], states: &[State], why: &str) -> Failure {
    let files: Vec<String> = targets
        .iter()
        .zip(states)
        .map(|(target, state)| {
            let shown = format!("{} {}", target.mode.name(), target.paths.join(", "));
            match state {
                State::NotAttempted => format!("{shown}: not attempted"),
                State::Prepared => format!("{shown}: prepared, not scanned"),
                State::Scanned(resident) => format!("{shown}: {resident} bytes resident"),
                State::Failed(why) => format!("{shown}: {why}"),
            }
        })
        .collect();
    Failure::Failed(format!(
        "page-cache preparation not established: {why} ({})",
        files.join("; ")
    ))
}

/// Why `found` resident bytes of `bytes` is not `mode`, stating what was
/// seen and what can cause it, never one cause as fact.
fn unestablished(mode: CacheMode, bytes: u64, found: u64) -> Option<String> {
    match mode {
        CacheMode::Cold if found != 0 => Some(format!(
            "{found} of its {bytes} bytes stayed resident after it was advised out: a filesystem \
             whose pages are its storage (tmpfs), a mapping or another reader can keep them"
        )),
        CacheMode::Warm if found != bytes => Some(format!(
            "{found} of its {bytes} bytes were resident after it was read through: memory \
             pressure, a memory limit on brokkr or other warm files can reclaim them"
        )),
        _ => None,
    }
}

/// The prerequisites of preparing `file` in `mode` that brokkr can detect
/// without preparing it: its residency is observable, a mapping of it can be
/// queried, and for cold, its filesystem is not one whose pages are its
/// storage. Passing them is not support: the preparation establishes and
/// checks everything again.
pub(crate) fn prerequisites(file: &File, mode: CacheMode) -> Result<(), String> {
    observable(file).map_err(reason)?;
    if mode == CacheMode::Cold
        && let Some(kind) = page_storage(file)?
    {
        return Err(format!(
            "it is on {kind}, whose pages are its storage: it cannot be advised out of the page \
             cache"
        ));
    }
    let len = size(file).map_err(reason)?;
    let page = page_size().map_err(reason)?;
    if len > 0 {
        scan_window(file, 0, len.min(page), page).map_err(reason)?;
    }
    Ok(())
}

fn reason(failure: Failure) -> String {
    match failure {
        Failure::Interrupted => "interrupted".to_owned(),
        Failure::Failed(why) => why,
    }
}

/// Whether the kernel reports `file`'s residency to brokkr truthfully, as
/// far as brokkr can establish it: its effective credentials may write it.
fn observable(file: &File) -> Result<(), Failure> {
    // SAFETY: a valid descriptor and a NUL-terminated empty path, which
    // AT_EMPTY_PATH makes name the descriptor itself.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::W_OK,
            libc::AT_EMPTY_PATH | libc::AT_EACCESS,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM | libc::EROFS | libc::ETXTBSY) => {
            Err(Failure::Failed(format!(
                "brokkr cannot establish that its page-cache residency is observable: it believes \
                 a residency only where it may write the file, and may not ({error}); the kernel \
                 reports every page resident to a process that neither owns the file, is capable \
                 over it nor may write it, so making it writable by brokkr's user lets brokkr \
                 observe it"
            )))
        }
        _ => Err(Failure::Failed(format!(
            "brokkr could not establish that its page-cache residency is observable: \
             faccessat2: {error}"
        ))),
    }
}

fn size(file: &File) -> Result<u64, Failure> {
    file.metadata()
        .map(|m| m.size())
        .map_err(|e| Failure::Failed(format!("cannot inspect it: {e}")))
}

fn page_size() -> Result<u64, Failure> {
    // SAFETY: sysconf with a valid name.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| Failure::Failed("the page size is unknown".to_owned()))
}

/// The filesystem `file` is on when its pages are its storage.
fn page_storage(file: &File) -> Result<Option<&'static str>, String> {
    // SAFETY: zeroed is a valid statfs to be filled.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid descriptor and a statfs to fill.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &raw mut stat) } != 0 {
        return Err(format!(
            "cannot inspect its filesystem: {}",
            std::io::Error::last_os_error()
        ));
    }
    /// linux/magic.h's, which the libc crate does not carry.
    const RAMFS_MAGIC: i64 = 0x8584_58f6;
    #[allow(clippy::unnecessary_cast)] // f_type's width differs by target.
    Ok(match stat.f_type as i64 {
        x if x == libc::TMPFS_MAGIC as i64 => Some("tmpfs"),
        RAMFS_MAGIC => Some("ramfs"),
        _ => None,
    })
}

/// Write back what is dirty, then advise the whole file out of the cache.
fn evict(file: &File) -> Result<(), Failure> {
    file.sync_data().map_err(|e| {
        Failure::Failed(format!("cannot write it back before advising it out: {e}"))
    })?;
    // SAFETY: a valid descriptor; offset and length 0 cover the whole file.
    let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Failure::Failed(format!(
            "cannot advise it out of the page cache: {}",
            std::io::Error::from_raw_os_error(rc)
        )))
    }
}

/// Read the whole of `size` bytes through, positionally, so no shared
/// offset moves; a file that ends early is refused.
fn warm(file: &File, size: u64, stop: &dyn Fn() -> bool) -> Result<(), Failure> {
    let mut buffer = vec![0u8; READ_WINDOW];
    let mut offset = 0u64;
    while offset < size {
        if stop() {
            return Err(Failure::Interrupted);
        }
        let want = usize::try_from((size - offset).min(READ_WINDOW as u64)).unwrap_or(READ_WINDOW);
        match file.read_at(&mut buffer[..want], offset) {
            Ok(0) => {
                return Err(Failure::Failed(format!(
                    "it ended at {offset} of the {size} bytes it held when prepared"
                )));
            }
            Ok(n) => offset += n as u64,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(Failure::Failed(format!("cannot read it through: {e}")));
            }
        }
    }
    Ok(())
}

/// The bytes of the first `size` the page cache holds, each resident page
/// counted up to the file's end, scanned in bounded windows of an untouched
/// read-only mapping.
fn resident_bytes(file: &File, size: u64, stop: &dyn Fn() -> bool) -> Result<u64, Failure> {
    let page = page_size()?;
    let window = SCAN_WINDOW - SCAN_WINDOW % page;
    let mut resident = 0u64;
    let mut offset = 0u64;
    while offset < size {
        if stop() {
            return Err(Failure::Interrupted);
        }
        let span = (size - offset).min(window);
        let pages = scan_window(file, offset, span, page)?;
        for (idx, held) in pages.iter().enumerate() {
            if held & 1 == 1 {
                let start = offset + idx as u64 * page;
                resident += (size - start).min(page);
            }
        }
        offset += span;
    }
    Ok(resident)
}

/// One window's residency vector: a byte per page of `span` bytes at
/// `offset`, a page-aligned offset.
fn scan_window(file: &File, offset: u64, span: u64, page: u64) -> Result<Vec<u8>, Failure> {
    let fail = |why: String| Failure::Failed(why);
    let map_len =
        usize::try_from(span).map_err(|_| fail("a scan window does not fit memory".to_owned()))?;
    let pages = usize::try_from(span.div_ceil(page))
        .map_err(|_| fail("a scan window does not fit memory".to_owned()))?;
    let map_offset = libc::off_t::try_from(offset)
        .map_err(|_| fail(format!("offset {offset} is out of range")))?;
    // SAFETY: a read-only shared mapping of a range inside the file, never
    // touched, unmapped below.
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
            "cannot map it to scan its residency: {}",
            std::io::Error::last_os_error()
        )));
    }
    let mut vec = vec![0u8; pages];
    // SAFETY: `addr` maps `map_len` bytes and `vec` holds one byte per page.
    let rc = unsafe { libc::mincore(addr, map_len, vec.as_mut_ptr()) };
    let error = std::io::Error::last_os_error();
    // SAFETY: the mapping made above, unmapped once.
    unsafe { libc::munmap(addr, map_len) };
    if rc != 0 {
        return Err(fail(format!(
            "cannot read its page-cache residency: {error}"
        )));
    }
    Ok(vec)
}

/// A file's identity, for grouping and alias checks.
pub(crate) fn inode(file: &File) -> std::io::Result<(u64, u64)> {
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
