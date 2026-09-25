//! Crash-safe replacement of a file: small hand-curated ones (`brokkr.toml`,
//! piners' `pins.toml`/`lints.toml`, the corpus digest, the hash cache) through
//! [`replace`], and streamed ones too large to hold in memory (tool and dataset
//! downloads) through [`Staged`].
//!
//! A plain `std::fs::write` truncates first and fills second, so a kill (or a
//! full disk) between the two leaves a truncated file no later run can load.
//! Both paths write a sibling temp file, fsync it, rename it over the target
//! and fsync the directory, so a reader sees the old bytes or the new ones,
//! never a prefix. This module is the one copy of that sequence; a hand-rolled
//! temp-and-rename elsewhere is a bug.

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
    let staged = Staged::per_process(path)?;
    std::fs::File::create(staged.tmp_path())?.write_all(contents)?;
    staged.commit()
}

/// A sibling temp file that becomes the target on [`Staged::commit`], for a
/// writer that produces the bytes itself (a `curl -o`, say) rather than
/// handing [`replace`] a buffer. Dropped uncommitted, it removes the temp file.
///
/// Target resolution, permission carry-over and the fsync/rename/dir-fsync
/// sequence are the ones [`replace`] documents - `replace` is built on this.
pub(crate) struct Staged {
    target: PathBuf,
    dir: PathBuf,
    tmp: PathBuf,
    committed: bool,
}

impl Staged {
    /// Temp file `.<name>.tmp-<pid>`: unique per process, so two processes
    /// replacing the same file never write into one temp file. A kill leaves
    /// it behind under a name no later run reuses - fine for small files.
    pub(crate) fn per_process(path: &Path) -> std::io::Result<Self> {
        Self::with_suffix(path, &format!("tmp-{}", std::process::id()))
    }

    /// Temp file `.<name>.partial`: the same name on every attempt, so an
    /// interrupted multi-gigabyte download is overwritten by the retry rather
    /// than leaking one partial per killed process. Only for writers already
    /// serialised by something else (the global brokkr lock, for downloads) -
    /// two concurrent writers would share the file.
    pub(crate) fn stable(path: &Path) -> std::io::Result<Self> {
        Self::with_suffix(path, "partial")
    }

    /// As [`stable`](Self::stable), but for a writer that infers its output
    /// format from the file name (pbfhogg's `-o x.osm.pbf`): when the target's
    /// name ends in `keep` the temp file is `.<rest>.partial<keep>`, so the
    /// writer sees the same format suffix. Otherwise identical to `stable`.
    pub(crate) fn stable_keeping(path: &Path, keep: &str) -> std::io::Result<Self> {
        let mut staged = Self::stable(path)?;
        let name = staged
            .target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(rest) = name.strip_suffix(keep).filter(|r| !r.is_empty()) {
            staged.tmp = staged.dir.join(format!(".{rest}.partial{keep}"));
        }
        Ok(staged)
    }

    fn with_suffix(path: &Path, suffix: &str) -> std::io::Result<Self> {
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
        let tmp = dir.join(format!(".{name}.{suffix}"));
        Ok(Self {
            target,
            dir,
            tmp,
            committed: false,
        })
    }

    /// Where the writer puts the new bytes.
    pub(crate) fn tmp_path(&self) -> &Path {
        &self.tmp
    }

    /// Make the temp file the target: carry the target's permissions over,
    /// fsync, rename, fsync the directory. On failure the temp file is removed
    /// (by the drop) and the target is untouched.
    pub(crate) fn commit(mut self) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new().write(true).open(&self.tmp)?;
        if let Ok(meta) = std::fs::metadata(&self.target) {
            f.set_permissions(meta.permissions())?;
        }
        f.sync_all()?;
        drop(f);
        std::fs::rename(&self.tmp, &self.target)?;
        self.committed = true;
        // Best effort: the directory fsync makes the rename durable, but
        // failing it does not un-write the file.
        if let Ok(d) = std::fs::File::open(&self.dir) {
            d.sync_all().ok();
        }
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            std::fs::remove_file(&self.tmp).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::{Staged, replace};

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

    #[test]
    fn an_uncommitted_stage_leaves_the_target_and_no_temp() {
        let dir = crate::test_scratch::scratch("atomic_write", "uncommitted");
        let path = dir.join("planet.osm.pbf");
        std::fs::write(&path, "old").unwrap();
        {
            let staged = Staged::stable(&path).unwrap();
            // Distinct from the `.with_extension("tmp")` shape, which mapped
            // `a.osm.pbf` and `a.osm.gz` onto one `a.osm.tmp`.
            assert_eq!(
                staged.tmp_path().file_name().unwrap(),
                ".planet.osm.pbf.partial"
            );
            std::fs::write(staged.tmp_path(), "half").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_kept_suffix_survives_into_the_temp_name() {
        let dir = crate::test_scratch::scratch("atomic_write", "keeping");
        let pbf = Staged::stable_keeping(&dir.join("dk-merged.osm.pbf"), ".osm.pbf").unwrap();
        assert_eq!(
            pbf.tmp_path().file_name().unwrap(),
            ".dk-merged.partial.osm.pbf"
        );
        // A name without the suffix falls back to the plain stable shape.
        let other = Staged::stable_keeping(&dir.join("a.png"), ".osm.pbf").unwrap();
        assert_eq!(other.tmp_path().file_name().unwrap(), ".a.png.partial");
    }

    #[test]
    fn a_committed_stage_replaces_the_target() {
        let dir = crate::test_scratch::scratch("atomic_write", "committed");
        let path = dir.join("tool.jar");
        let staged = Staged::stable(&path).unwrap();
        std::fs::write(staged.tmp_path(), "whole").unwrap();
        staged.commit().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "whole");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }
}
