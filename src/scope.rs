//! Scope + limit helpers for `brokkr check`'s gremlins/clippy phases.
//!
//! When a phase produces a large pile of diagnostics, dumping all of them
//! at once is useless. This module computes the set of files changed on
//! the current branch and partitions diagnostics so that every hit in a
//! branch-touched file is shown in full and only unscoped hits get capped
//! at `limit`. The unscoped overflow count is rolled up into a trailer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Files modified on the current branch vs its upstream base.
///
/// Returns `None` when no useful scope can be computed - not in a git
/// repo, detached HEAD, no upstream, branch is identical to base, etc.
/// Callers treat `None` as "scope unavailable" and fall back to simple
/// capping.
pub fn changed_files(project_root: &Path) -> Option<HashSet<PathBuf>> {
    let base = branch_base(project_root)?;
    let output = Command::new("git")
        .args(["diff", "--name-only", "-z", &format!("{base}...HEAD")])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut set = HashSet::new();
    for raw in output.stdout.split(|b| *b == 0) {
        if raw.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(raw) {
            set.insert(PathBuf::from(s));
        }
    }
    // Also include files modified in the working tree but not yet committed,
    // so iterating on uncommitted changes still gets scope treatment.
    if let Ok(wt) = Command::new("git")
        .args(["diff", "--name-only", "-z", "HEAD"])
        .current_dir(project_root)
        .output()
    {
        for raw in wt.stdout.split(|b| *b == 0) {
            if raw.is_empty() {
                continue;
            }
            if let Ok(s) = std::str::from_utf8(raw) {
                set.insert(PathBuf::from(s));
            }
        }
    }
    if set.is_empty() { None } else { Some(set) }
}

/// Try a few candidate base refs. First hit wins. `None` if nothing
/// resolves (detached HEAD, new repo, no upstream).
fn branch_base(project_root: &Path) -> Option<String> {
    // Upstream of the current branch (most reliable).
    if let Some(up) = run_git(project_root, &["rev-parse", "--abbrev-ref", "@{upstream}"])
        && let Some(base) = merge_base(project_root, &up)
    {
        return Some(base);
    }
    // Fallbacks.
    for candidate in ["origin/master", "origin/main", "master", "main"] {
        if let Some(base) = merge_base(project_root, candidate) {
            return Some(base);
        }
    }
    None
}

fn merge_base(project_root: &Path, other: &str) -> Option<String> {
    run_git(project_root, &["merge-base", "HEAD", other])
}

fn run_git(project_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&output.stdout).ok()?.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
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
    pub hidden_unscoped: usize,
}

/// Partition `items` so every scoped (branch-touched) hit is shown in
/// full, followed by up to `limit` unscoped hits. Both halves retain
/// their input order.
///
/// `scope` = `None` means "no scope available" (all hits are treated as
/// unscoped and the cap applies); `Some(set)` uses [`HashSet`] membership.
///
/// See [`partition_pinned`] for the cap's one exemption.
pub fn partition<T, F>(
    items: Vec<T>,
    get_path: F,
    limit: usize,
    scope: Option<&HashSet<PathBuf>>,
) -> Partition<T>
where
    F: Fn(&T) -> &Path,
{
    partition_pinned(items, get_path, |_| false, limit, scope)
}

/// [`partition`] with an exemption: an item `is_pinned` says yes to is
/// displayed whatever the cap, even when it is unscoped and past `limit`.
///
/// The cap exists to keep a wall of WARNINGS in untouched files from burying
/// the run. It must never hide a FAILURE - a hard error elided as overflow
/// reads as "not in this run" to anyone who trusts the list, and the trailer
/// only says how many were hidden, not that one of them was fatal. Pinned
/// items keep their input position and do not consume the cap.
pub fn partition_pinned<T, F, P>(
    items: Vec<T>,
    get_path: F,
    is_pinned: P,
    limit: usize,
    scope: Option<&HashSet<PathBuf>>,
) -> Partition<T>
where
    F: Fn(&T) -> &Path,
    P: Fn(&T) -> bool,
{
    let (scoped, rest): (Vec<T>, Vec<T>) = match scope {
        Some(set) => items.into_iter().partition(|item| set.contains(get_path(item))),
        None => (Vec::new(), items),
    };
    // Pinned unscoped items bypass the cap entirely; only the remainder
    // competes for `limit` slots.
    let (pinned, unscoped): (Vec<T>, Vec<T>) = rest.into_iter().partition(&is_pinned);
    let scoped: Vec<T> = scoped.into_iter().chain(pinned).collect();

    let mut displayed: Vec<T> = Vec::with_capacity(scoped.len() + limit.min(unscoped.len()));
    displayed.extend(scoped);

    let mut unscoped_iter = unscoped.into_iter();
    for item in unscoped_iter.by_ref().take(limit) {
        displayed.push(item);
    }
    let hidden_unscoped = unscoped_iter.count();

    Partition {
        displayed,
        hidden_unscoped,
    }
}

/// Build the trailer line summarising hidden unscoped hits. `None` when
/// nothing is hidden.
pub fn format_trailer(hidden_unscoped: usize) -> Option<String> {
    if hidden_unscoped == 0 {
        return None;
    }
    Some(format!("+{hidden_unscoped} in unchanged files (--triage to see)"))
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

    #[test]
    fn no_scope_caps_and_counts_unscoped() {
        let items = vec![item("a"), item("b"), item("c"), item("d")];
        let part = partition(items, |t| t.0.as_path(), 2, None);
        assert_eq!(part.displayed.len(), 2);
        assert_eq!(part.hidden_unscoped, 2);
    }

    #[test]
    fn scope_prefers_scoped_hits() {
        let scope: HashSet<PathBuf> = ["b", "d"].iter().map(|s| p(s)).collect();
        let items = vec![item("a"), item("b"), item("c"), item("d"), item("e")];
        let part = partition(items, |t| t.0.as_path(), 3, Some(&scope));
        // 2 scoped (b, d) + 3 unscoped (a, c, e), limit only caps unscoped.
        assert_eq!(part.displayed.len(), 5);
        let displayed_paths: Vec<&str> = part.displayed.iter().map(|t| t.1).collect();
        assert_eq!(displayed_paths, vec!["b", "d", "a", "c", "e"]);
        assert_eq!(part.hidden_unscoped, 0);
    }

    #[test]
    fn scoped_always_shown_in_full() {
        let scope: HashSet<PathBuf> = ["a", "b", "c", "d"].iter().map(|s| p(s)).collect();
        let items = vec![item("a"), item("b"), item("c"), item("d"), item("e")];
        let part = partition(items, |t| t.0.as_path(), 2, Some(&scope));
        // All 4 scoped show in full; the 1 unscoped fits within limit=2.
        assert_eq!(part.displayed.len(), 5);
        assert_eq!(part.hidden_unscoped, 0);
    }

    #[test]
    fn limit_caps_unscoped_only() {
        let scope: HashSet<PathBuf> = ["a"].iter().map(|s| p(s)).collect();
        let items = vec![item("a"), item("b"), item("c"), item("d"), item("e")];
        let part = partition(items, |t| t.0.as_path(), 2, Some(&scope));
        // 1 scoped + 2 unscoped (b, c); d, e hidden.
        assert_eq!(part.displayed.len(), 3);
        let displayed_paths: Vec<&str> = part.displayed.iter().map(|t| t.1).collect();
        assert_eq!(displayed_paths, vec!["a", "b", "c"]);
        assert_eq!(part.hidden_unscoped, 2);
    }

    #[test]
    fn everything_fits() {
        let items = vec![item("a"), item("b")];
        let part = partition(items, |t| t.0.as_path(), 10, None);
        assert_eq!(part.displayed.len(), 2);
        assert_eq!(part.hidden_unscoped, 0);
    }

    #[test]
    fn trailer_unscoped_only() {
        let s = format_trailer(7).unwrap();
        assert_eq!(s, "+7 in unchanged files (--triage to see)");
    }

    #[test]
    fn trailer_none_when_nothing_hidden() {
        assert!(format_trailer(0).is_none());
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

#[cfg(test)]
mod pinned_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// The defect: the cap is a warning-volume control, and it used to drop
    /// an unscoped ERROR as overflow - a failure the reader never sees while
    /// the trailer only counts it.
    #[test]
    fn a_pinned_item_survives_past_the_limit() {
        // (path, is_error). The error sorts last, well past limit=2.
        let items = vec![
            (p("a"), false),
            (p("b"), false),
            (p("c"), false),
            (p("d"), true),
        ];
        let part = partition_pinned(items, |t| t.0.as_path(), |t| t.1, 2, None);
        let shown: Vec<&str> = part
            .displayed
            .iter()
            .map(|t| t.0.to_str().unwrap())
            .collect();
        assert_eq!(shown, vec!["d", "a", "b"]);
        // Only the two capped warnings are hidden; the error is not one.
        assert_eq!(part.hidden_unscoped, 1);
    }

    /// Pinned items must not consume cap slots - otherwise a run with many
    /// errors would silently shrink the warning window as well.
    #[test]
    fn pinned_items_do_not_consume_the_cap() {
        let items = vec![
            (p("e1"), true),
            (p("e2"), true),
            (p("w1"), false),
            (p("w2"), false),
        ];
        let part = partition_pinned(items, |t| t.0.as_path(), |t| t.1, 2, None);
        assert_eq!(part.displayed.len(), 4);
        assert_eq!(part.hidden_unscoped, 0);
    }
}
