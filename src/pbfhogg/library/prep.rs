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
            Self::Residency(why) => {
                write!(f, "dataset page-cache preparation not established: {why}")
            }
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
    let targets = datasets
        .iter()
        .map(|path| {
            open(path).map(|file| crate::bench::cache::Target {
                paths: vec![path.display().to_string()],
                mode: crate::bench::store::CacheMode::Cold,
                file,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(NotEstablished::Residency)?;
    let mut files = Vec::new();
    for path in retained {
        collect_files(path, &mut files).map_err(NotEstablished::Residency)?;
    }
    for file in &files {
        evict(file).map_err(NotEstablished::Residency)?;
    }
    crate::bench::cache::prepare(&targets, &crate::shutdown::should_unwind).map_err(|failure| {
        match failure {
            crate::bench::cache::Failure::Interrupted => NotEstablished::Interrupted,
            crate::bench::cache::Failure::Failed(why) => NotEstablished::Residency(why),
        }
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prepared_file_has_no_resident_byte() -> Result<(), Box<dyn std::error::Error>> {
        let dir = crate::test_scratch::scratch("pbfhogg-library-prep", "evict");
        let file = dir.join("input.osm.pbf");
        std::fs::write(&file, vec![7u8; 3 * 4096 + 17])?;
        assert_eq!(std::fs::read(&file)?.len(), 3 * 4096 + 17);
        match prepare(std::slice::from_ref(&file), &[dir.join("absent")]) {
            Ok(()) => {}
            // Filesystems whose pages are their storage cannot be made cold.
            Err(NotEstablished::Residency(why)) => {
                assert!(why.contains("stayed resident"), "{why}");
                assert!(why.contains(&file.display().to_string()), "{why}");
            }
            Err(NotEstablished::Interrupted) => panic!("interrupted"),
        }
        Ok(())
    }

    #[test]
    fn an_empty_file_scans_clear_and_retained_directories_are_walked()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = crate::test_scratch::scratch("pbfhogg-library-prep", "walk");
        let empty = dir.join("empty");
        std::fs::write(&empty, b"")?;
        assert!(prepare(&[empty], &[]).is_ok());
        std::fs::create_dir_all(dir.join("out/sub"))?;
        std::fs::write(dir.join("out/sub/a"), b"x")?;
        let mut files = Vec::new();
        collect_files(&dir.join("out"), &mut files)?;
        assert_eq!(files, [dir.join("out/sub/a")]);
        Ok(())
    }

    #[test]
    fn ownership_does_not_establish_observability() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_scratch::scratch("pbfhogg-library-prep", "readonly");
        let file = dir.join("input");
        std::fs::write(&file, b"x")?;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444))?;
        // Effective credentials with DAC override can still write this file.
        // Ask the same descriptor predicate rather than guessing from a uid.
        let descriptor = open(&file)?;
        if crate::bench::cache::prerequisites(&descriptor, crate::bench::store::CacheMode::Warm)
            .is_ok()
        {
            return Ok(());
        }
        match prepare(std::slice::from_ref(&file), &[]) {
            Err(NotEstablished::Residency(why)) => {
                assert!(why.contains("residency is observable"), "{why}");
                assert!(why.contains(&file.display().to_string()), "{why}");
                assert!(!why.contains("stayed resident"), "{why}");
            }
            other => panic!("expected an observability refusal, got {other:?}"),
        }
        Ok(())
    }
}
