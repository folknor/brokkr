//! Scope + limit helpers for `brokkr check`'s diagnostic output.
//!
//! Every diagnostic a phase reports is an error, and at most `--limit` of them
//! are shown. Which ones: if any error sits in a file with unstaged changes -
//! the file being edited right now - only those are candidates; otherwise every
//! error is. The rest are counted in a trailer, split by where they are.

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

/// Result of partitioning a diagnostic list into displayed vs hidden.
pub struct Partition<T> {
    pub displayed: Vec<T>,
    /// Whether the display was narrowed to files with unstaged changes.
    pub focused: bool,
    /// Hidden by the cap, in files with unstaged changes.
    pub hidden_unstaged: usize,
    /// Hidden in every other file: past the cap, or passed over because the
    /// unstaged files had errors of their own.
    pub hidden_elsewhere: usize,
}

/// Choose at most `limit` items to display, in input order.
///
/// If any item's path is in `unstaged`, only those items are candidates - an
/// error in the file being edited is the one to fix first, and errors elsewhere
/// would only push it off the screen. Otherwise every item is a candidate.
/// `unstaged = None` (no git) makes every item a candidate.
pub fn partition<T, F>(
    items: Vec<T>,
    get_path: F,
    limit: usize,
    unstaged: Option<&HashSet<PathBuf>>,
) -> Partition<T>
where
    F: Fn(&T) -> &Path,
{
    let (in_unstaged, elsewhere): (Vec<T>, Vec<T>) = match unstaged {
        Some(set) => items.into_iter().partition(|item| set.contains(get_path(item))),
        None => (Vec::new(), items),
    };
    if in_unstaged.is_empty() {
        let hidden_elsewhere = elsewhere.len().saturating_sub(limit);
        let displayed = elsewhere.into_iter().take(limit).collect();
        return Partition { displayed, focused: false, hidden_unstaged: 0, hidden_elsewhere };
    }
    let hidden_unstaged = in_unstaged.len().saturating_sub(limit);
    Partition {
        displayed: in_unstaged.into_iter().take(limit).collect(),
        focused: true,
        hidden_unstaged,
        hidden_elsewhere: elsewhere.len(),
    }
}

/// The trailer summarising what [`partition`] hid. `None` when nothing is.
///
/// A focused display says so, because it lists fewer errors than the run has:
/// `+2 more in unstaged files, +31 in other files`. An unfocused one is a plain
/// cap: `+31 more`.
pub fn format_trailer<T>(part: &Partition<T>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if part.focused {
        if part.hidden_unstaged > 0 {
            parts.push(format!("+{} more in unstaged files", part.hidden_unstaged));
        }
        if part.hidden_elsewhere > 0 {
            parts.push(format!("+{} in other files", part.hidden_elsewhere));
        }
    } else if part.hidden_elsewhere > 0 {
        parts.push(format!("+{} more", part.hidden_elsewhere));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("{} (--triage to see all)", parts.join(", ")))
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

    fn shown<'a>(part: &Partition<(PathBuf, &'a str)>) -> Vec<&'a str> {
        part.displayed.iter().map(|t| t.1).collect()
    }

    #[test]
    fn without_unstaged_errors_every_error_competes_for_the_cap() {
        let unstaged: HashSet<PathBuf> = [p("z")].into_iter().collect();
        let items = vec![item("a"), item("b"), item("c"), item("d")];
        let part = partition(items, |t| t.0.as_path(), 2, Some(&unstaged));
        assert_eq!(shown(&part), vec!["a", "b"]);
        assert!(!part.focused);
        assert_eq!(part.hidden_elsewhere, 2);
        assert_eq!(format_trailer(&part).unwrap(), "+2 more (--triage to see all)");
    }

    /// The rule: an error in the file being edited is shown alone, and the
    /// others are counted, not listed.
    #[test]
    fn unstaged_errors_are_shown_alone() {
        let unstaged: HashSet<PathBuf> = ["b", "d"].iter().map(|s| p(s)).collect();
        let items = vec![item("a"), item("b"), item("c"), item("d"), item("e")];
        let part = partition(items, |t| t.0.as_path(), 20, Some(&unstaged));
        assert_eq!(shown(&part), vec!["b", "d"]);
        assert!(part.focused);
        assert_eq!(part.hidden_elsewhere, 3);
        assert_eq!(format_trailer(&part).unwrap(), "+3 in other files (--triage to see all)");
    }

    /// The cap binds the unstaged errors too: no class of error is exempt.
    #[test]
    fn the_cap_applies_to_unstaged_errors() {
        let unstaged: HashSet<PathBuf> = ["a", "b", "c"].iter().map(|s| p(s)).collect();
        let items = vec![item("a"), item("b"), item("c"), item("d")];
        let part = partition(items, |t| t.0.as_path(), 2, Some(&unstaged));
        assert_eq!(shown(&part), vec!["a", "b"]);
        assert_eq!(part.hidden_unstaged, 1);
        assert_eq!(part.hidden_elsewhere, 1);
        assert_eq!(
            format_trailer(&part).unwrap(),
            "+1 more in unstaged files, +1 in other files (--triage to see all)"
        );
    }

    #[test]
    fn no_git_caps_everything() {
        let items = vec![item("a"), item("b"), item("c")];
        let part = partition(items, |t| t.0.as_path(), 2, None);
        assert_eq!(shown(&part), vec!["a", "b"]);
        assert_eq!(part.hidden_elsewhere, 1);
    }

    #[test]
    fn everything_fits_and_says_nothing() {
        let items = vec![item("a"), item("b")];
        let part = partition(items, |t| t.0.as_path(), 10, None);
        assert_eq!(part.displayed.len(), 2);
        assert!(format_trailer(&part).is_none());
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
