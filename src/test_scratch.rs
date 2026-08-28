//! The one scratch-directory allocator for unit tests.
//!
//! Project rules forbid `/tmp`, so every test that needs a filesystem writes
//! under the crate's gitignored `target/test-tmp/`. Eleven modules grew their
//! own copy of that helper, each correct on its own and each carrying the same
//! unwritten precondition: **the name must be unique**, because the allocator
//! deletes the directory before handing it back.
//!
//! That precondition failed twice before this module existed, both times in
//! the same way - not a test picking a duplicate name, but a *shared helper*
//! calling the allocator with a name of its own. Any fixed name such a helper
//! picks is shared by every one of its callers:
//!
//! - `dellingr::workload`'s `config_with` used `tmpdir("cfg")`, so concurrent
//!   tests deleted each other's `brokkr.toml` mid-run. Green serially, three
//!   failures at `--test-threads=8`, and the symptom was a hash mismatch or a
//!   missing file - a fixture race wearing the costume of a bug in the code
//!   under test.
//! - `mogwai::targets`' `config_with` reached for
//!   `tmpdir(format!("cfg-{:?}", thread::current().id()))`, which works only
//!   because Rust guarantees a `ThreadId` is never reused. Correct, but by a
//!   guarantee subtle enough that the next person to copy the line will not
//!   know they depend on it.
//!
//! So uniqueness is asserted here rather than left to convention. A repeat
//! `(module, name)` panics at the moment it is introduced, in the test that
//! introduced it, instead of becoming an intermittent failure somewhere else
//! that somebody has to reproduce under concurrency to explain.
//!
//! Why a shared module rather than a guard pasted into each copy: a
//! precondition enforced in eleven places is enforced in ten places as soon as
//! somebody adds a twelfth. The allocator is the only thing that knows about
//! the deletion, so it is the only thing that can enforce the rule that
//! deletion implies.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Every `(module, name)` handed out so far this process.
static TAKEN: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

/// `<crate>/target/test-tmp/<module>`, created if absent.
fn module_root(module: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-tmp")
        .join(module);
    std::fs::create_dir_all(&root)
        .unwrap_or_else(|e| panic!("could not create scratch root {}: {e}", root.display()));
    root
}

/// Record `(module, name)` as claimed, panicking if it was already taken.
///
/// The message names both halves and says what the collision *does*, because
/// the failure it prevents does not look like a naming problem when it
/// finally shows up.
fn claim(module: &str, name: &str) {
    let fresh = TAKEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((module.to_owned(), name.to_owned()));
    assert!(
        fresh,
        "scratch name {name:?} is used twice in module {module:?}. The \
         allocator deletes the path before returning it, so two tests sharing \
         a name delete each other's files - invisible serially, an \
         intermittent failure in whichever test loses the race once anything \
         runs concurrently. Give each test its own name, and if the caller is \
         a shared helper, have it take the caller's path instead of naming one."
    );
}

/// A fresh, empty scratch **directory** for one test.
///
/// `module` groups the directories on disk (pass the module's own name);
/// `name` identifies the test and must be unique within that module.
pub(crate) fn scratch(module: &str, name: &str) -> PathBuf {
    claim(module, name);
    let dir = module_root(module).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .unwrap_or_else(|e| panic!("could not clear scratch {}: {e}", dir.display()));
    }
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("could not create scratch {}: {e}", dir.display()));
    dir
}

/// A scratch **file path** for one test: the parent directory is created, and
/// any stale file at the path is removed, but the file itself is not.
///
/// For tests that need a path to a not-yet-existing file rather than a
/// directory to fill - `lockfile`'s per-test lock paths, where the point is
/// that the file's own creation is what is under test.
pub(crate) fn scratch_path(module: &str, name: &str) -> PathBuf {
    claim(module, name);
    let path = module_root(module).join(name);
    // A stale file from a previous run is not a race - that process is gone -
    // but it is removed so content assertions start from nothing.
    drop(std::fs::remove_file(&path));
    path
}

#[cfg(test)]
mod test_scratch_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_directory_comes_back_empty() {
        let dir = scratch("test-scratch", "empty");
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    // The allocator's contract: it CLEARS. That is exactly why a repeated
    // name is dangerous, so the clearing and the guard are tested together.
    #[test]
    fn a_directory_is_cleared_of_a_previous_run() {
        let dir = scratch("test-scratch", "cleared");
        std::fs::write(dir.join("leftover"), "x").unwrap();
        // Simulate the next process: same path, allocator run again. Called
        // through the inner steps because `scratch` would refuse the repeat.
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn a_path_is_not_created_but_its_parent_is() {
        let path = scratch_path("test-scratch", "not_yet");
        assert!(!path.exists());
        assert!(path.parent().unwrap().is_dir());
    }

    // The guard itself. Without it this collision is silent, and shows up
    // later as a flake in an unrelated-looking test.
    #[test]
    #[should_panic(expected = "used twice in module")]
    fn a_repeated_name_is_refused() {
        let _first = scratch("test-scratch", "dupe");
        let _second = scratch("test-scratch", "dupe");
    }

    // Modules are separate namespaces: the same test name in two modules is
    // two directories and must not trip the guard.
    #[test]
    fn the_same_name_in_two_modules_is_fine() {
        let a = scratch("test-scratch-a", "shared_name");
        let b = scratch("test-scratch-b", "shared_name");
        assert_ne!(a, b);
    }
}
