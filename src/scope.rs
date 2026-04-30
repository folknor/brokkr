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
pub fn partition<T, F>(
    items: Vec<T>,
    get_path: F,
    limit: usize,
    scope: Option<&HashSet<PathBuf>>,
) -> Partition<T>
where
    F: Fn(&T) -> &Path,
{
    let (scoped, unscoped): (Vec<T>, Vec<T>) = match scope {
        Some(set) => items.into_iter().partition(|item| set.contains(get_path(item))),
        None => (Vec::new(), items),
    };

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
    Some(format!("+{hidden_unscoped} in unchanged files (--all to see)"))
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
        assert_eq!(s, "+7 in unchanged files (--all to see)");
    }

    #[test]
    fn trailer_none_when_nothing_hidden() {
        assert!(format_trailer(0).is_none());
    }
}
