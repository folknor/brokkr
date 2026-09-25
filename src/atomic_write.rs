//! Crash-safe replacement of a small, hand-curated file (`brokkr.toml`,
//! piners' `pins.toml`/`lints.toml`).
//!
//! A plain `std::fs::write` truncates first and fills second, so a kill (or a
//! full disk) between the two leaves a truncated file no later run can load.
//! [`replace`] writes a sibling temp file, fsyncs it, renames it over the
//! target and fsyncs the directory, so a reader sees the old bytes or the new
//! ones, never a prefix.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Atomically replace `path` with `contents`.
///
/// An existing target is resolved through any symlink first, so the link
/// survives and its target is what gets replaced, and the target's permissions
/// carry over. The temp file is `.<name>.tmp-<pid>` beside the target - hidden
/// and extension-less, so no `*.toml` directory scan ever picks it up - and is
/// removed on any failure before the rename.
pub(crate) fn replace(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let target = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e),
    };
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));

    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        if let Ok(meta) = std::fs::metadata(&target) {
            f.set_permissions(meta.permissions())?;
        }
        f.sync_all()?;
        std::fs::rename(&tmp, &target)
    })();
    if let Err(e) = written {
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }
    // Best effort: the directory fsync makes the rename durable, but failing
    // it does not un-write the file.
    if let Ok(d) = std::fs::File::open(&dir) {
        d.sync_all().ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::replace;

    #[test]
    fn replaces_contents_and_leaves_no_temp_behind() {
        let dir = crate::test_scratch::scratch("atomic_write", "replace");
        let path = dir.join("pins.toml");
        std::fs::write(&path, "old = 1\n").unwrap();

        replace(&path, b"new = 2\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = 2\n");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != "pins.toml")
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn creates_a_missing_file() {
        let dir = crate::test_scratch::scratch("atomic_write", "create");
        let path = dir.join("new.toml");
        replace(&path, b"a = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 1\n");
    }

    #[test]
    fn a_symlink_survives_and_its_target_is_replaced() {
        let dir = crate::test_scratch::scratch("atomic_write", "symlink");
        let real = dir.join("real.toml");
        let link = dir.join("brokkr.toml");
        std::fs::write(&real, "old\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        replace(&link, b"new\n").unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new\n");
    }
}
