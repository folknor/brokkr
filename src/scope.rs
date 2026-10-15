//! Scope helpers for `brokkr check`'s diagnostic output.
//!
//! Every diagnostic a phase reports is an error, and every one of them is
//! shown - there is no cap and no flag to raise one. Errors in files with
//! unstaged changes - the files being edited right now - come first, then the
//! rest, which is the whole of what this module decides.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
pub(crate) fn test_git_tree(module: &str, name: &str) -> PathBuf {
    let root = crate::test_scratch::scratch(module, name);
    let status = crate::output::child::command_status(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["init", "--quiet"])
            .current_dir(&root),
    )
    .expect("git is available");
    assert!(status.success(), "git init failed in {}", root.display());
    root
}

/// Files with unstaged changes: modified in the working tree relative to the
/// index, plus untracked files git does not ignore (a new file is unstaged
/// too). Paths are relative to `project_root`, the directory cargo and the
/// scanners report paths against.
///
/// Staged and committed changes do not count: the priority is the edit in
/// progress, and a file staged for commit is one the author has already
/// signed off on.
///
/// `None` when git cannot be asked (not a repository); callers then treat
/// every error as a candidate.
pub fn unstaged_files(project_root: &Path) -> Option<HashSet<PathBuf>> {
    status_snapshot(project_root).and_then(|(_, unstaged)| unstaged)
}

/// What kind of work is uncommitted in the working tree.
///
/// Drives `check`'s prose-only shortcut: editing documentation cannot break a
/// build, so a tree whose only uncommitted change is markdown does not need the
/// clippy and test phases to prove it still compiles - the last full run
/// already did, on the same code.
///
/// Markdown a Rust source compiles in (`include_str!`'d `man` pages, a
/// `#![doc = include_str!("README.md")]` crate's doctests) still counts as
/// prose here. That is deliberate, not a bug: markdown is never dirt, the
/// cost of a missed doctest or rustdoc break is one later full run, and
/// `--force-rust` asks for the build when a docs edit warrants it. A scan of
/// every source for `include_str!` targets was tried and reverted. The
/// case-insensitive markdown extensions come from the same table gremlins
/// scans, so prose-only changes still receive the text safety scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirt {
    /// Not a git repo, or git could not be asked. Never shortens a run: the
    /// shortcut is an inference from evidence, and there is none here.
    Unknown,
    /// Nothing uncommitted. A full run - a clean tree is exactly the state a
    /// complete check is *for*, and shortening it would make the common
    /// pre-commit invocation prove nothing.
    Clean,
    /// Every uncommitted path is markdown.
    ProseOnly,
    /// At least one uncommitted path is not markdown.
    Code,
}

/// Classify the working tree. Staged, unstaged and untracked-not-ignored paths
/// all count: what matters is whether anything not yet committed could change
/// how the code builds, and an untracked `.rs` file certainly can.
pub fn dirt(project_root: &Path) -> Dirt {
    status_snapshot(project_root)
        .map(|(dirt, _)| dirt)
        .unwrap_or(Dirt::Unknown)
}

fn status_snapshot(project_root: &Path) -> Option<(Dirt, Option<HashSet<PathBuf>>)> {
    let state = crate::git::TreeState::read(project_root)?;
    Some((state.for_check(), state.unstaged_files()))
}

/// One check invocation's view of the working tree. Git paths are enumerated
/// once, status supplies both dirt and diagnostic priority, and file bytes are
/// cached on first read so every convention scanner judges the same contents.
pub(crate) struct TreeSnapshot {
    root: PathBuf,
    files: Vec<PathBuf>,
    contents: Mutex<HashMap<PathBuf, Option<Arc<[u8]>>>>,
    unstaged: Option<HashSet<PathBuf>>,
}

impl TreeSnapshot {
    pub(crate) fn load(project_root: &Path) -> Result<Self, crate::error::DevError> {
        let files = crate::gremlins::tracked_files(project_root)?;
        let unstaged = status_snapshot(project_root).and_then(|(_, unstaged)| unstaged);
        Ok(Self {
            root: project_root.to_path_buf(),
            files,
            contents: Mutex::new(HashMap::new()),
            unstaged,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn files(&self) -> &[PathBuf] {
        &self.files
    }

    pub(crate) fn unstaged_files(&self) -> Option<&HashSet<PathBuf>> {
        self.unstaged.as_ref()
    }

    /// The cache key for `rel` (relative to the root, or absolute): the
    /// file's canonical identity, so `a/../b.rs`, a symlinked spelling and the
    /// git spelling of one file share one entry - every reader of this
    /// snapshot sees the same bytes. A path that cannot be canonicalized
    /// (deleted, say) keys by its joined spelling.
    fn key(&self, rel: &Path) -> PathBuf {
        let joined = self.root.join(rel);
        std::fs::canonicalize(&joined).unwrap_or(joined)
    }

    /// Read one path from this snapshot, relative to its root or absolute.
    /// `None` means git listed a file that has since been deleted; every other
    /// read failure is returned. Both the bytes and a missing-file result are
    /// cached, by file identity, so scanners share one read.
    pub(crate) fn read_file(
        &self,
        rel: &Path,
    ) -> Result<Option<Arc<[u8]>>, crate::error::DevError> {
        let key = self.key(rel);
        let mut contents = self
            .contents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = contents.get(&key) {
            return Ok(cached.clone());
        }
        let bytes = match std::fs::read(&key) {
            Ok(bytes) => Some(Arc::from(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(crate::error::DevError::Io(std::io::Error::new(
                    error.kind(),
                    format!("cannot read {}: {error}", rel.display()),
                )));
            }
        };
        contents.insert(key, bytes.clone());
        Ok(bytes)
    }

    pub(crate) fn write_file(
        &self,
        rel: &Path,
        bytes: Vec<u8>,
    ) -> Result<(), crate::error::DevError> {
        let key = self.key(rel);
        let mut contents = self
            .contents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fs::write(&key, &bytes).map_err(crate::error::DevError::Io)?;
        contents.insert(key, Some(Arc::from(bytes)));
        Ok(())
    }
}

/// Classify the bytes of `git status --porcelain=v1 -z`. Split out so the
/// record format - including the second path a rename record carries - is
/// testable without a repository.
#[cfg(test)]
fn classify_status(stdout: &[u8]) -> Dirt {
    parse_status(stdout).0
}

#[cfg(test)]
fn parse_status(stdout: &[u8]) -> (Dirt, Option<HashSet<PathBuf>>) {
    crate::git::TreeState::check_status(stdout)
}

/// Order a diagnostic list for display.
///
/// Items whose path is in `unstaged` come first - an error in the file being
/// edited is the one to fix first - then every other item; each group keeps
/// its input order. `unstaged = None` (no git) leaves the order unchanged.
/// Nothing is ever dropped.
pub fn prioritize<T, F>(items: Vec<T>, get_path: F, unstaged: Option<&HashSet<PathBuf>>) -> Vec<T>
where
    F: Fn(&T) -> &Path,
{
    let (in_unstaged, elsewhere): (Vec<T>, Vec<T>) = match unstaged {
        Some(set) => items
            .into_iter()
            .partition(|item| set.contains(get_path(item))),
        None => (Vec::new(), items),
    };
    in_unstaged.into_iter().chain(elsewhere).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn item(path: &str) -> (PathBuf, &str) {
        (p(path), path)
    }

    fn shown<'a>(ordered: &[(PathBuf, &'a str)]) -> Vec<&'a str> {
        ordered.iter().map(|t| t.1).collect()
    }

    /// The rule: an error in the file being edited comes first, the rest
    /// follow, and none of them is dropped.
    #[test]
    fn unstaged_errors_come_first() {
        let unstaged: HashSet<PathBuf> = ["b", "d"].iter().map(|s| p(s)).collect();
        let items = vec![item("a"), item("b"), item("c"), item("d"), item("e")];
        let ordered = prioritize(items, |t| t.0.as_path(), Some(&unstaged));
        assert_eq!(shown(&ordered), vec!["b", "d", "a", "c", "e"]);
    }

    /// An unstaged set that matches nothing leaves the input order alone.
    #[test]
    fn no_unstaged_match_keeps_the_input_order() {
        let unstaged: HashSet<PathBuf> = [p("z")].into_iter().collect();
        let items = vec![item("a"), item("b"), item("c"), item("d")];
        let ordered = prioritize(items, |t| t.0.as_path(), Some(&unstaged));
        assert_eq!(shown(&ordered), vec!["a", "b", "c", "d"]);
    }

    /// No git means no scope information, so the order is left untouched.
    #[test]
    fn no_git_keeps_the_input_order() {
        let items = vec![item("a"), item("b"), item("c")];
        let ordered = prioritize(items, |t| t.0.as_path(), None);
        assert_eq!(shown(&ordered), vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_status_is_a_clean_tree() {
        assert_eq!(classify_status(b""), Dirt::Clean);
    }

    #[test]
    fn markdown_only_changes_are_prose() {
        let status = b" M docs/guide.md\0?? notes/todo.MARKDOWN\0A  README.MD\0";
        assert_eq!(classify_status(status), Dirt::ProseOnly);
    }

    #[test]
    fn status_snapshot_prioritizes_worktree_and_untracked_paths_only() {
        let status = b" M src/edit.rs\0M  src/staged.rs\0?? notes/new.md\0";
        let (dirt, unstaged) = parse_status(status);
        assert_eq!(dirt, Dirt::Code);
        assert_eq!(
            unstaged.unwrap(),
            [p("src/edit.rs"), p("notes/new.md")].into_iter().collect()
        );
    }

    #[test]
    fn snapshot_reuses_file_bytes_and_updates_them_after_a_write() {
        let root = test_git_tree("scope", "tree_snapshot");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "before").unwrap();
        let tree = TreeSnapshot::load(&root).unwrap();

        let first = tree.read_file(Path::new("src/lib.rs")).unwrap().unwrap();
        std::fs::write(root.join("src/lib.rs"), "external change").unwrap();
        let repeated = tree.read_file(Path::new("src/lib.rs")).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &repeated));
        assert_eq!(repeated.as_ref(), b"before");

        tree.write_file(Path::new("src/lib.rs"), b"fixed".to_vec())
            .unwrap();
        let fixed = tree.read_file(Path::new("src/lib.rs")).unwrap().unwrap();
        assert_eq!(fixed.as_ref(), b"fixed");
    }

    #[test]
    fn one_code_file_is_enough_to_make_it_code() {
        let status = b" M docs/guide.md\0 M src/lib.rs\0";
        assert_eq!(classify_status(status), Dirt::Code);
    }

    /// A path with no extension (`Makefile`, `justfile`) is not prose.
    #[test]
    fn extensionless_paths_are_code() {
        assert_eq!(classify_status(b" M Makefile\0"), Dirt::Code);
    }

    /// A rename record carries a second path, and it counts: moving a note on
    /// top of a source file is not a documentation edit.
    #[test]
    fn a_rename_weighs_both_of_its_paths() {
        let renamed = b"R  docs/b.md\0docs/a.md\0";
        assert_eq!(classify_status(renamed), Dirt::ProseOnly);

        let out_of_prose = b"R  src/lib.rs\0notes.md\0";
        assert_eq!(classify_status(out_of_prose), Dirt::Code);

        let into_prose = b"R  docs/a.md\0src/lib.rs\0";
        assert_eq!(classify_status(into_prose), Dirt::Code);
    }

    /// An origin path that happens to look like a status record must be read
    /// as a path, not re-parsed - otherwise its first three bytes vanish.
    #[test]
    fn an_origin_path_is_not_reparsed_as_a_record() {
        // Re-parsing would strip "xy." and leave an extensionless "md".
        let status = b"R  docs/b.md\0xy.md\0";
        assert_eq!(classify_status(status), Dirt::ProseOnly);
    }
}
