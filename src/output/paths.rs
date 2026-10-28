//! Operator-facing path display.
//!
//! A path inside the project tree prints relative to the directory brokkr was
//! invoked from: `.brokkr/piners/corpus/run-7` when the config sits in cwd,
//! `../.brokkr/...` in the layout where it sits one level up. Every other path
//! prints absolute, unchanged.
//!
//! The decision is lexical over [`Path`] components, never over rendered text:
//! a formatted message can carry captured child output or opaque values that
//! merely look like paths, so only a caller that holds a `Path` it means as a
//! location may ask for it. Containment is by spelling, not by resolved target:
//! a symlink inside the project keeps its in-project spelling (which resolves
//! the same from cwd), and a remainder with a `..` keeps its absolute form
//! rather than being simplified across a possible symlink.
//!
//! Data (stored values, argv, `data_stdout`, JSON) keeps `Path::display`.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// Where relative paths are anchored: the project root, and its spelling from
/// the invocation directory (empty when they coincide, `..` when the config
/// was found one level up).
struct Anchor {
    project_root: PathBuf,
    base: PathBuf,
    /// The invocation directory's own name when it is the root's child.
    cwd_name: Option<PathBuf>,
}

static ANCHOR: OnceLock<Anchor> = OnceLock::new();

/// Fix the anchor once at startup. `project_root` is the detected root (or cwd
/// when none was detected). A root that is neither cwd nor its parent - which
/// project detection never produces - leaves every path absolute.
pub fn set_path_anchor(cwd: &Path, project_root: &Path) {
    if let Some(anchor) = anchor_for(cwd, project_root) {
        ANCHOR.set(anchor).ok();
    }
}

fn anchor_for(cwd: &Path, project_root: &Path) -> Option<Anchor> {
    let (base, cwd_name) = if cwd == project_root {
        (PathBuf::new(), None)
    } else if cwd.parent() == Some(project_root) {
        (PathBuf::from(".."), cwd.file_name().map(PathBuf::from))
    } else {
        return None;
    };
    Some(Anchor {
        project_root: project_root.to_owned(),
        base,
        cwd_name,
    })
}

/// `path` for an operator line: relative to the invocation directory when it
/// lies inside the project, absolute otherwise.
pub fn path(path: &Path) -> PathDisplay<'_> {
    PathDisplay { path }
}

pub struct PathDisplay<'a> {
    path: &'a Path,
}

impl fmt::Display for PathDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match ANCHOR.get().and_then(|anchor| relative(anchor, self.path)) {
            Some(rel) => write!(f, "{}", rel.display()),
            None => write!(f, "{}", self.path.display()),
        }
    }
}

fn relative(anchor: &Anchor, path: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(&anchor.project_root).ok()?;
    if !rest.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    // In the one-level-up layout `base` is `..` and cwd is the root's child
    // named by the remainder's first component: a path under cwd drops that
    // round trip (`target/x`, not `../sub/target/x`).
    let joined = match anchor.cwd_name.as_deref() {
        Some(name) if rest.starts_with(name) => rest.strip_prefix(name).ok()?.to_path_buf(),
        // `join("")` would append a trailing separator (`../`).
        _ if rest.as_os_str().is_empty() => anchor.base.clone(),
        _ => anchor.base.join(rest),
    };
    if joined.as_os_str().is_empty() {
        return Some(PathBuf::from("."));
    }
    // A leading `-` or `~` would read as an option or a home directory when
    // pasted into a shell; `./` keeps it a plain path.
    let lead = joined.as_os_str().as_encoded_bytes().first().copied();
    if matches!(lead, Some(b'-' | b'~')) {
        return Some(Path::new(".").join(joined));
    }
    Some(joined)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn show(cwd: &str, root: &str, path: &str) -> String {
        let anchor = anchor_for(Path::new(cwd), Path::new(root)).unwrap();
        relative(&anchor, Path::new(path))
            .map_or_else(|| path.to_owned(), |p| p.display().to_string())
    }

    #[test]
    fn project_paths_are_relative_to_the_invocation_directory() {
        assert_eq!(show("/p", "/p", "/p/state/run-1"), "state/run-1");
        assert_eq!(show("/p/sub", "/p", "/p/state/run-1"), "../state/run-1");
        assert_eq!(show("/p/sub", "/p", "/p/sub/target/x"), "target/x");
        assert_eq!(show("/p/sub", "/p", "/p/subway/x"), "../subway/x");
        assert_eq!(show("/p", "/p", "/p"), ".");
        assert_eq!(show("/p/sub", "/p", "/p"), "..");
        assert_eq!(show("/p/sub", "/p", "/p/sub"), ".");
    }

    #[test]
    fn everything_else_stays_absolute() {
        // A sibling sharing the root's text as a prefix is not inside it.
        assert_eq!(show("/p", "/p", "/p2/file"), "/p2/file");
        assert_eq!(show("/p", "/p", "/elsewhere/file"), "/elsewhere/file");
        // `..` in the remainder could cross a symlink: keep the spelling.
        assert_eq!(show("/p", "/p", "/p/link/../file"), "/p/link/../file");
        // A relative input is not anchored at the project.
        assert_eq!(show("/p", "/p", "file"), "file");
    }

    #[test]
    fn shell_sensitive_leads_get_a_dot_slash() {
        assert_eq!(show("/p", "/p", "/p/-report"), "./-report");
        assert_eq!(show("/p", "/p", "/p/~u/file"), "./~u/file");
        assert_eq!(show("/p/sub", "/p", "/p/-report"), "../-report");
    }

    #[test]
    fn an_unrelated_root_anchors_nothing() {
        assert!(anchor_for(Path::new("/a/b/c"), Path::new("/a")).is_none());
    }
}
