//! Path identity for overlap tests: two spellings of every path, so a
//! reservation and a mutation are compared however each was written.
//!
//! The resolved form is what the kernel would reach: components are walked
//! in order from the root, and every symlink met - in the path or inside any
//! link's target - is expanded where it stands, so a symlink is followed
//! before a later `..` pops it (`link/../x` names `x` beside the link's
//! target, not beside the link). Each link expanded is kept in `via`, since
//! removing it, or a directory holding it, cuts the path. The part that does
//! not exist yet is appended, `..` there popping the path built so far. The
//! lexical form is
//! absolute with `.` and `..` folded textually; it is kept only as a second
//! spelling, which can widen an overlap but never narrows one. Two paths
//! overlap when any form of one equals, lies inside or encloses any form of
//! the other. A resolution error other than a missing component is an error,
//! never a weaker identity. Bind-mount aliases are outside this identity (see
//! the module docs).

use std::io;
use std::path::{Component, Path, PathBuf};

/// Both forms of one path, and every symlink resolution went through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spelled {
    pub(crate) lexical: PathBuf,
    pub(crate) resolved: PathBuf,
    /// Each symlink traversed on the way, as the resolved path of the link
    /// itself: removing one, or a directory holding one, cuts the path as
    /// surely as removing its end.
    pub(crate) via: Vec<PathBuf>,
}

impl Spelled {
    /// Spell `path`, relative paths against `cwd`.
    pub(crate) fn of(path: &Path, cwd: &Path) -> io::Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        let (resolved, via) = resolve(&absolute)?;
        Ok(Self {
            lexical: lexical(&absolute),
            resolved,
            via,
        })
    }

    /// Spell an already absolute path recorded earlier.
    pub(crate) fn recorded(lexical: PathBuf, resolved: PathBuf, via: Vec<PathBuf>) -> Self {
        Self {
            lexical,
            resolved,
            via,
        }
    }

    /// Whether either path's forms equal, contain or enclose the other's, or
    /// one encloses a symlink the other's resolution goes through.
    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        let mine = [&self.lexical, &self.resolved];
        let theirs = [&other.lexical, &other.resolved];
        let ends = mine
            .iter()
            .any(|a| theirs.iter().any(|b| a.starts_with(b) || b.starts_with(a)));
        let cuts = |forms: &[&PathBuf; 2], links: &[PathBuf]| {
            links
                .iter()
                .any(|link| forms.iter().any(|form| link.starts_with(form)))
        };
        ends || cuts(&theirs, &self.via) || cuts(&mine, &other.via)
    }
}

/// `.` dropped and `..` popping its textual parent.
fn lexical(absolute: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The kernel's walk: components in order from the root, each symlink met -
/// in the path itself or in any link's target, at any depth - recorded where
/// it lives and replaced by its target's components (an absolute target
/// restarting from the root). The part that does not exist yet is appended
/// as written. More than 40 link expansions is a loop, as for the kernel.
fn resolve(absolute: &Path) -> io::Result<(PathBuf, Vec<PathBuf>)> {
    use std::collections::VecDeque;
    enum Step {
        Up,
        Name(std::ffi::OsString),
    }
    fn steps(path: &Path) -> impl Iterator<Item = Step> + '_ {
        path.components().filter_map(|part| match part {
            Component::ParentDir => Some(Step::Up),
            Component::Normal(name) => Some(Step::Name(name.to_os_string())),
            Component::RootDir | Component::Prefix(_) | Component::CurDir => None,
        })
    }
    let mut pending: VecDeque<Step> = steps(absolute).collect();
    let mut out = PathBuf::from("/");
    let mut via = Vec::new();
    let mut existing = true;
    let mut expansions = 0;
    while let Some(step) = pending.pop_front() {
        let name = match step {
            Step::Up => {
                out.pop();
                continue;
            }
            Step::Name(name) => name,
        };
        let candidate = out.join(&name);
        if !existing {
            out = candidate;
            continue;
        }
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                expansions += 1;
                if expansions > 40 {
                    return Err(io::Error::other(format!(
                        "too many symbolic links resolving {}",
                        absolute.display()
                    )));
                }
                let target = std::fs::read_link(&candidate)?;
                via.push(candidate);
                if target.is_absolute() {
                    out = PathBuf::from("/");
                }
                for step in steps(&target).collect::<Vec<_>>().into_iter().rev() {
                    pending.push_front(step);
                }
            }
            Ok(_) => out = candidate,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                existing = false;
                out = candidate;
            }
            Err(e) => return Err(e),
        }
    }
    Ok((out, via))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn lexical_form_folds_dots() {
        assert_eq!(
            lexical(Path::new("/w/p/a/./b/../c")),
            PathBuf::from("/w/p/a/c")
        );
        assert_eq!(lexical(Path::new("/x/../y")), PathBuf::from("/y"));
    }

    #[test]
    fn overlap_is_equality_or_containment_either_way() {
        let cwd = Path::new("/");
        let dir = Spelled::of(Path::new("/nonexistent-brokkr/a"), cwd).unwrap();
        let inside = Spelled::of(Path::new("/nonexistent-brokkr/a/b.pbf"), cwd).unwrap();
        let sibling = Spelled::of(Path::new("/nonexistent-brokkr/ab"), cwd).unwrap();
        assert!(dir.overlaps(&inside) && inside.overlaps(&dir));
        assert!(dir.overlaps(&dir));
        assert!(!dir.overlaps(&sibling), "a name prefix is not containment");
    }

    /// A symlinked ancestor compares by where it points, for a path below it
    /// that does not exist yet, and a `..` after the link pops its target.
    #[test]
    fn symlinks_are_followed_before_parent_components_pop() {
        let root = crate::test_scratch::scratch("reservations", "symlink");
        let real = root.join("data").join("sub");
        std::fs::create_dir_all(&real).unwrap();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let link = work.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical_real = std::fs::canonicalize(&real).unwrap();

        let via_link = Spelled::of(&link.join("out.pbf"), &root).unwrap();
        let direct = Spelled::of(&real.join("out.pbf"), &root).unwrap();
        assert!(via_link.overlaps(&direct));
        assert_eq!(via_link.resolved, canonical_real.join("out.pbf"));

        // `link/../out.pbf` is `data/out.pbf`, beside the link's target.
        let popped = Spelled::of(&link.join("..").join("out.pbf"), &root).unwrap();
        let beside_target = Spelled::of(&root.join("data").join("out.pbf"), &root).unwrap();
        assert_eq!(popped.resolved, beside_target.resolved);
        assert!(popped.overlaps(&beside_target));
    }

    /// A path through a chain of links is cut by removing the directory
    /// holding an intermediate link, though neither end is inside it.
    #[test]
    fn removing_an_intermediate_link_overlaps_the_path_through_it() {
        let root = crate::test_scratch::scratch("reservations", "symlink-chain");
        let data = root.join("data").join("sub");
        std::fs::create_dir_all(&data).unwrap();
        let aliases = root.join("aliases");
        std::fs::create_dir_all(&aliases).unwrap();
        std::os::unix::fs::symlink(&data, aliases.join("second")).unwrap();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::os::unix::fs::symlink(aliases.join("second"), work.join("link")).unwrap();

        let input = Spelled::of(&work.join("link").join("in.pbf"), &root).unwrap();
        assert!(
            input.via.iter().any(|v| v.ends_with("work/link")),
            "{:?}",
            input.via
        );
        assert!(
            input.via.iter().any(|v| v.ends_with("aliases/second")),
            "{:?}",
            input.via
        );
        let removal = Spelled::of(&aliases, &root).unwrap();
        assert!(input.overlaps(&removal) && removal.overlaps(&input));
        let unrelated = Spelled::of(&root.join("other"), &root).unwrap();
        assert!(!input.overlaps(&unrelated));
    }

    /// A link inside a link target's parent directories is recorded too:
    /// `work/link -> parents/parent/sub`, `parents/parent -> data`.
    #[test]
    fn a_link_in_a_targets_parent_is_part_of_the_path() {
        let root = crate::test_scratch::scratch("reservations", "symlink-parent");
        let data = root.join("data").join("sub");
        std::fs::create_dir_all(&data).unwrap();
        let parents = root.join("parents");
        std::fs::create_dir_all(&parents).unwrap();
        std::os::unix::fs::symlink(root.join("data"), parents.join("parent")).unwrap();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::os::unix::fs::symlink(parents.join("parent").join("sub"), work.join("link")).unwrap();

        let input = Spelled::of(&work.join("link").join("in.pbf"), &root).unwrap();
        assert_eq!(
            input.resolved,
            std::fs::canonicalize(&data).unwrap().join("in.pbf")
        );
        let removal = Spelled::of(&parents, &root).unwrap();
        assert!(
            input.overlaps(&removal),
            "removing the directory holding the inner link cuts the path"
        );
    }
}
