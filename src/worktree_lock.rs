//! The root `Cargo.lock` of a `--commit` worktree.
//!
//! A project that does not track its lockfile (pbfhogg ignores it) would
//! otherwise build a worktree against whatever cargo resolved when the slot
//! was cut: the newest compatible versions on that day, frozen for as long as
//! the slot survives and replaced by another day's on a re-cut. The live tree
//! builds against its own lock, so a `--commit` arm compared with it differs
//! in dependency versions as well as in code (brokkr#105). [`seed`] copies the
//! live tree's lock into such a worktree on every admission, cut or reuse, so
//! both start from the same file. Cargo may still amend each copy to fit its
//! manifests; a seed only equalizes the starting bytes.
//!
//! Only the conventional layout is managed: the root `Cargo.toml` is its own
//! workspace root (a `[workspace]` table, or a `[package]` naming no other
//! workspace with no ancestor directory declaring one) on both sides, and the lock is the default `Cargo.lock` beside
//! it. A `resolver.lockfile-path` override is not detected.

use std::path::Path;

use crate::error::DevError;

const LOCK: &str = "Cargo.lock";

/// The seed's staging file, in the slot beside the checkout: on the same
/// filesystem, so the rename is atomic, and outside the checkout, so a
/// killed copy cannot leave a file its pristine check would see.
const STAGING: &str = ".brokkr-lock-seed";

/// Where a build root's `Cargo.lock` came from, observed before the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The live tree's own.
    Live,
    /// The worktree's commit tracks it; never touched.
    Tracked,
    /// Copied from the live tree's at this admission.
    Seeded,
    /// Untracked, the live tree has none, and the slot already held one.
    Retained,
    /// Untracked, the live tree has none, and neither does the slot.
    Fresh,
    /// Not the conventional root-lock layout on one side.
    Unmanaged,
}

/// Whether `root/Cargo.toml` is its own workspace root, so its lock is
/// `root/Cargo.lock`. A missing or unreadable manifest is not.
pub fn conventional(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        return false;
    };
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return false;
    };
    if manifest.contains_key("workspace") {
        return true;
    }
    // A package naming no workspace may still be an implicit member of one
    // above it, whose lock cargo uses instead; any ancestor declaring a
    // workspace makes the layout unmanaged rather than guessed at.
    manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .is_some_and(|package| !package.contains_key("workspace"))
        && !root.ancestors().skip(1).any(declares_workspace)
}

/// Whether `dir/Cargo.toml` has a `[workspace]` table.
fn declares_workspace(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("Cargo.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .is_some_and(|manifest| manifest.contains_key("workspace"))
}

/// The live tree's lock source: [`Source::Live`] for the conventional
/// layout, else [`Source::Unmanaged`].
pub fn live(root: &Path) -> Source {
    if conventional(root) {
        Source::Live
    } else {
        Source::Unmanaged
    }
}

/// What occupies a lock path: nothing, a regular file, or something else,
/// which is refused rather than written through or counted as a lock.
fn present(path: &Path) -> Result<bool, DevError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => Ok(true),
        Ok(_) => Err(DevError::Refused(format!(
            "{} is not a regular file; refusing to use it as a lockfile",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Whether the commit checked out at `checkout` tracks its root lock.
fn tracked(checkout: &Path) -> Result<bool, DevError> {
    let output = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["ls-tree", "--name-only", "HEAD", "--", LOCK])
            .current_dir(checkout),
    )
    .map_err(|error| DevError::Spawn {
        program: "git".into(),
        error,
    })?;
    if !output.status.success() {
        return Err(DevError::Subprocess {
            program: "git".into(),
            ending: crate::error::ProcessEnding::from_status(&output.status),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(!output.stdout.is_empty())
}

/// Bring the worktree's root lock in line with the live tree's, and say
/// where it came from. Run after admission and before any cargo call in the
/// checkout. The live lock is read now, so an edit to it after this point
/// does not reach this build.
pub fn seed(live_root: &Path, slot: &Path, checkout: &Path) -> Result<Source, DevError> {
    if !conventional(live_root) || !conventional(checkout) {
        return Ok(Source::Unmanaged);
    }
    if tracked(checkout)? {
        return Ok(Source::Tracked);
    }
    let dest = checkout.join(LOCK);
    let held = present(&dest)?;
    let live_lock = live_root.join(LOCK);
    if !present(&live_lock)? {
        return Ok(if held {
            Source::Retained
        } else {
            Source::Fresh
        });
    }
    let bytes = std::fs::read(&live_lock)?;
    // Created exclusively, so a leftover at the staging path, a symlink
    // above all, is never written through.
    let staging = slot.join(STAGING);
    match std::fs::remove_file(&staging) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&staging, &dest)?;
    Ok(Source::Seeded)
}

/// The xxh3-128 digest of `root/Cargo.lock` once a build there succeeded:
/// `None` for an unmanaged layout (`managed` false), whose root file cargo
/// may not have read, a missing file, or a read that failed, which is warned
/// about rather than failing the build it describes.
pub fn digest(root: &Path, managed: bool) -> Option<String> {
    if !managed {
        return None;
    }
    let path = root.join(LOCK);
    match std::fs::read(&path) {
        Ok(bytes) => Some(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            crate::output::warn(&format!("lockfile: cannot read {}: {e}", path.display()));
            None
        }
    }
}

/// One line naming a worktree's lock source, for the run log.
pub fn describe(source: Source) -> &'static str {
    match source {
        Source::Live => "lockfile: the live tree's",
        Source::Tracked => "lockfile: the commit's own Cargo.lock",
        Source::Seeded => "lockfile: seeded from the live tree's Cargo.lock",
        Source::Retained => {
            "lockfile: untracked and the live tree has none; using the one the worktree holds"
        }
        Source::Fresh => "lockfile: untracked and the live tree has none; cargo resolves afresh",
        Source::Unmanaged => {
            "lockfile: not a root-workspace layout on both sides; left to cargo, not seeded"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str, manifest: &str) -> std::path::PathBuf {
        let dir = crate::test_scratch::scratch("worktree_lock", name);
        std::fs::write(dir.join("Cargo.toml"), manifest).expect("manifest");
        dir
    }

    #[test]
    fn a_root_workspace_or_standalone_package_is_conventional() {
        assert!(conventional(&tree("ws", "[workspace]\nmembers = []\n")));
        assert!(conventional(&tree("pkg", "[package]\nname = \"a\"\n")));
        assert!(!conventional(&tree(
            "member",
            "[package]\nname = \"a\"\nworkspace = \"..\"\n"
        )));
        assert!(!conventional(&tree("none", "[dependencies]\n")));
        let outer = tree("outer", "[workspace]\nmembers = [\"inner\"]\n");
        let inner = outer.join("inner");
        std::fs::create_dir(&inner).expect("inner");
        std::fs::write(inner.join("Cargo.toml"), "[package]\nname = \"a\"\n").expect("member");
        assert!(
            !conventional(&inner),
            "an implicit member is not its own root"
        );
        let empty = crate::test_scratch::scratch("worktree_lock", "missing");
        assert!(!conventional(&empty));
    }

    #[test]
    fn digest_is_absent_for_an_unmanaged_layout_or_a_missing_file() {
        let dir = tree("digest", "[workspace]\n");
        assert_eq!(digest(&dir, true), None);
        std::fs::write(dir.join(LOCK), b"version = 4\n").expect("lock");
        assert_eq!(digest(&dir, false), None);
        let d = digest(&dir, true).expect("digest");
        assert_eq!(d.len(), 32);
    }

    #[test]
    fn a_lock_path_that_is_not_a_regular_file_is_refused() {
        let dir = tree("present", "[workspace]\n");
        assert!(!present(&dir.join(LOCK)).expect("missing"));
        std::fs::create_dir(dir.join(LOCK)).expect("dir");
        assert!(present(&dir.join(LOCK)).is_err());
    }
}
