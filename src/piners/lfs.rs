//! Git-LFS pointer guard for pinned feed files.
//!
//! The `pineforge-engine` corpus submodule ships its 1m base feed through Git
//! LFS (`.gitattributes` routes `data/ohlcv_*.csv` to `filter=lfs`). A
//! checkout without an LFS smudge leaves a ~134-byte *pointer* in place of the
//! real bytes, whose first line is
//! `version https://git-lfs.github.com/spec/v1`. Hashing that pointer would
//! poison the pin (a `--reseed` that stamps the pointer hash) or fail every
//! future verify against the real hash - so every path that hashes a pinned
//! file first calls [`ensure_materialized`], which hard-errors with a
//! `git lfs pull` instruction rather than hashing a pointer.
//!
//! Scope: only the engine submodule's feed is LFS; the bench feed lives in a
//! different submodule and is plaintext. The guard is a cheap content sniff
//! (it fires only on an actual pointer), so it is safe to call on every pinned
//! file regardless of which submodule owns it. Because it hard-errors *before*
//! hashing, the large one-time LFS fetch is an out-of-band pre-warm the user
//! runs, and never counts against the corpus runtime ceiling.

use std::io::Read;
use std::path::Path;

use crate::error::DevError;

/// The signature every Git-LFS pointer file begins with.
const LFS_POINTER_PREFIX: &[u8] = b"version https://git-lfs.github.com/spec/v1";

/// Hard-error if `path` is an unmaterialized Git-LFS pointer rather than the
/// real feed bytes.
///
/// Reads only the first bytes of the file (a pointer is well under 1 KB). The
/// error names the file and the owning git root so the caller knows where to
/// run `git lfs pull`. A read/open error here is swallowed - the hasher that
/// follows surfaces it with its own richer context.
pub fn ensure_materialized(path: &Path) -> Result<(), DevError> {
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(());
    };
    let mut head = Vec::with_capacity(LFS_POINTER_PREFIX.len());
    if file
        .take(LFS_POINTER_PREFIX.len() as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return Ok(());
    }
    if !head.starts_with(LFS_POINTER_PREFIX) {
        return Ok(());
    }

    let where_to = git_root(path).map_or_else(
        || "run `git lfs pull` in the owning submodule".to_owned(),
        |root| format!("run `git lfs pull` in {}", root.display()),
    );
    Err(DevError::Preflight(vec![format!(
        "piners: {} is an unmaterialized Git-LFS pointer, not the real feed bytes.\n  \
         Hashing the pointer would poison the pin, so brokkr refuses. {where_to}, then retry.\n  \
         (the one-time LFS fetch is large; pre-warm it before a gated run - it runs \
         out-of-band and does not count against the runtime ceiling)",
        path.display()
    )]))
}

/// Walk `path`'s ancestors for the nearest `.git` entry (a directory in a
/// plain clone, a gitlink file inside a submodule) - the owning repo/submodule
/// root, used only for the error message. Best-effort.
fn git_root(path: &Path) -> Option<&Path> {
    path.ancestors().find(|a| a.join(".git").exists())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brokkr_piners_lfs_{}_{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn errors_on_lfs_pointer() {
        let p = tmp("pointer.csv");
        std::fs::write(
            &p,
            "version https://git-lfs.github.com/spec/v1\n\
             oid sha256:0123456789abcdef\nsize 176000000\n",
        )
        .unwrap();
        let err = ensure_materialized(&p).unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(format!("{err:?}").contains("git lfs pull"));
    }

    #[test]
    fn passes_on_real_csv() {
        let p = tmp("real.csv");
        std::fs::write(&p, "timestamp,open,high,low,close,volume\n1,2,3,4,5,6\n").unwrap();
        let r = ensure_materialized(&p);
        std::fs::remove_file(&p).ok();
        assert!(r.is_ok());
    }

    #[test]
    fn passes_on_missing_file() {
        // A missing file is the hasher's error to report, not ours.
        assert!(ensure_materialized(Path::new("/no/such/feed.csv")).is_ok());
    }
}
