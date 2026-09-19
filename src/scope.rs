//! Scope helpers for `brokkr check`'s diagnostic output.
//!
//! Every diagnostic a phase reports is an error, and every one of them is
//! shown - there is no cap and no flag to raise one. Errors in files with
//! unstaged changes - the files being edited right now - come first, then the
//! rest, which is the whole of what this module decides.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let modified = git_paths(project_root, &["diff", "--name-only", "--relative", "-z"])?;
    let untracked = git_paths(project_root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Some(modified.into_iter().chain(untracked).collect())
}

/// Run a git command that prints NUL-separated paths.
fn git_paths(project_root: &Path, args: &[&str]) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(
        output
            .stdout
            .split(|b| *b == 0)
            .filter(|raw| !raw.is_empty())
            .filter_map(|raw| std::str::from_utf8(raw).ok())
            .map(PathBuf::from)
            .collect(),
    )
}

/// What kind of work is uncommitted in the working tree.
///
/// Drives `check`'s prose-only shortcut: editing documentation cannot break a
/// build, so a tree whose only uncommitted change is markdown does not need the
/// clippy and test phases to prove it still compiles - the last full run
/// already did, on the same code.
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

/// Extensions the prose-only shortcut treats as documentation.
const PROSE_EXTENSIONS: [&str; 2] = ["md", "markdown"];

/// Classify the working tree. Staged, unstaged and untracked-not-ignored paths
/// all count: what matters is whether anything not yet committed could change
/// how the code builds, and an untracked `.rs` file certainly can.
pub fn dirt(project_root: &Path) -> Dirt {
    let Some(output) = Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return Dirt::Unknown;
    };
    classify_status(&output.stdout)
}

/// Classify the bytes of `git status --porcelain=v1 -z`. Split out so the
/// record format - including the second path a rename record carries - is
/// testable without a repository.
fn classify_status(stdout: &[u8]) -> Dirt {
    let mut seen = false;
    let mut prose_only = true;
    // A rename/copy record is followed by its origin path as a bare field.
    // That path is part of the change too: `git mv notes.md src/lib.rs` is not
    // a documentation edit.
    let mut expect_origin = false;
    for field in stdout.split(|b| *b == 0) {
        if field.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(field) else {
            // A path we cannot read is a path we cannot vouch for.
            return Dirt::Code;
        };
        let path = if expect_origin {
            expect_origin = false;
            text
        } else {
            // `XY <path>`: two status columns, a space, then the path.
            let Some(rest) = text.get(3..) else {
                return Dirt::Code;
            };
            expect_origin = text.starts_with('R') || text.starts_with('C');
            rest
        };
        seen = true;
        let is_prose = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| PROSE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !is_prose {
            prose_only = false;
        }
    }
    if !seen {
        Dirt::Clean
    } else if prose_only {
        Dirt::ProseOnly
    } else {
        Dirt::Code
    }
}

/// Order a diagnostic list for display.
///
/// Items whose path is in `unstaged` come first - an error in the file being
/// edited is the one to fix first - then every other item; each group keeps
/// its input order. `unstaged = None` (no git) leaves the order unchanged.
/// Nothing is ever dropped.
pub fn prioritize<T, F>(
    items: Vec<T>,
    get_path: F,
    unstaged: Option<&HashSet<PathBuf>>,
) -> Vec<T>
where
    F: Fn(&T) -> &Path,
{
    let (in_unstaged, elsewhere): (Vec<T>, Vec<T>) = match unstaged {
        Some(set) => items.into_iter().partition(|item| set.contains(get_path(item))),
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
        let status = b" M docs/guide.md\0?? notes/todo.markdown\0A  README.MD\0";
        assert_eq!(classify_status(status), Dirt::ProseOnly);
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
