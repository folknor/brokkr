//! One parsed view of a checkout's porcelain status, with named answers for
//! the different questions callers ask about its changes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Entry {
    index: u8,
    worktree: u8,
    paths: Vec<ClassifiedPath>,
}

#[derive(Debug)]
struct ClassifiedPath {
    path: PathBuf,
    class: PathClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathClass {
    Source,
    BrokkrStore,
    Prose,
    RootConfig,
    IncludedConfig,
    ApprovedBaseline,
    CargoLock,
    ToolchainOriginal,
    ToolchainSidecar,
}

/// The parsed status for one repository root. `read` runs porcelain once;
/// policy methods below interpret the same entries for measurement, record,
/// reuse, and deletion decisions.
pub(crate) struct TreeState {
    root: PathBuf,
    entries: Vec<Entry>,
}

impl TreeState {
    /// Read `git status --porcelain=v1 -z`, or return `None` when git cannot
    /// provide a trustworthy answer. NUL framing preserves paths containing
    /// spaces, newlines, quotes, and non-UTF-8 bytes.
    pub(crate) fn read(root: &Path) -> Option<Self> {
        Self::read_with_measurement_inputs(root, &HashSet::new())
    }

    /// Read the status snapshot with the composed config paths needed by the
    /// measurement policy.
    pub(crate) fn read_for_measurement(
        root: &Path,
        included_config_files: &[PathBuf],
    ) -> Option<Self> {
        let canonical_root = std::fs::canonicalize(root).ok()?;
        let included_configs: HashSet<PathBuf> = included_config_files
            .iter()
            .filter_map(|path| path.strip_prefix(&canonical_root).ok().map(Path::to_owned))
            .collect();
        Self::read_with_measurement_inputs(root, &included_configs)
    }

    fn read_with_measurement_inputs(
        root: &Path,
        included_configs: &HashSet<PathBuf>,
    ) -> Option<Self> {
        let output = crate::output::child::command_output(
            crate::process::spawn::ChildPolicy::plain_command("git")
                .args([
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--no-renames",
                    "--untracked-files=all",
                ])
                .current_dir(root),
        )
        .ok()?;
        if !output.status.success() {
            return None;
        }
        Self::from_porcelain(root, &output.stdout, included_configs)
    }

    fn from_porcelain(
        root: &Path,
        bytes: &[u8],
        included_configs: &HashSet<PathBuf>,
    ) -> Option<Self> {
        Some(Self {
            root: root.to_owned(),
            entries: parse_porcelain_z(bytes, included_configs)?,
        })
    }

    /// Check's shortcut considers every path, including brokkr state and
    /// config. Measurement and record exemptions do not certify a prose edit.
    pub(crate) fn for_check(&self) -> crate::scope::Dirt {
        if self.entries.is_empty() {
            crate::scope::Dirt::Clean
        } else if self
            .entries
            .iter()
            .flat_map(|entry| &entry.paths)
            .all(|path| {
                path.path.to_str().is_some()
                    && path
                        .path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(crate::globs::is_prose_extension)
            })
        {
            crate::scope::Dirt::ProseOnly
        } else {
            crate::scope::Dirt::Code
        }
    }

    /// Worktree edits and untracked files are diagnostic priority. Staged-only
    /// entries remain visible to dirt policies but do not get edit priority.
    pub(crate) fn unstaged_files(&self) -> Option<HashSet<PathBuf>> {
        if self
            .entries
            .iter()
            .flat_map(|entry| &entry.paths)
            .any(|path| path.path.to_str().is_none())
        {
            return None;
        }
        Some(
            self.entries
                .iter()
                .filter(|entry| entry.worktree != b' ')
                .flat_map(|entry| entry.paths.iter().map(|path| path.path.clone()))
                .collect(),
        )
    }

    #[cfg(test)]
    pub(crate) fn check_status(bytes: &[u8]) -> (crate::scope::Dirt, Option<HashSet<PathBuf>>) {
        Self::from_porcelain(Path::new("."), bytes, &HashSet::new())
            .map(|state| (state.for_check(), state.unstaged_files()))
            .unwrap_or((crate::scope::Dirt::Code, None))
    }

    /// Whether a source or build-input change would make a commit hash
    /// misdescribe the measurement. Documentation, brokkr-owned state,
    /// project config outputs, approved visual baselines, and brokkr's own
    /// locked-window tree edits over unchanged committed files (a moved
    /// toolchain pin, a filtered cargo config) are exempt.
    pub(crate) fn for_measurement(&self) -> bool {
        let tree_edits = brokkr_tree_edits(&self.root, &self.entries);
        self.entries.iter().any(|entry| {
            entry.paths.iter().any(|path| {
                !tree_edits.contains(&path.path)
                    && !matches!(
                        path.class,
                        PathClass::BrokkrStore
                            | PathClass::Prose
                            | PathClass::RootConfig
                            | PathClass::IncludedConfig
                            | PathClass::ApprovedBaseline
                    )
            })
        })
    }

    /// Whether an invocation record should say the working tree was dirty.
    /// This is broader than measurement cleanliness: prose and project config
    /// count, while brokkr's own state and its own tree edits do not.
    pub(crate) fn for_record(&self) -> bool {
        let tree_edits = brokkr_tree_edits(&self.root, &self.entries);
        self.entries.iter().any(|entry| {
            entry.paths.iter().any(|path| {
                !tree_edits.contains(&path.path) && path.class != PathClass::BrokkrStore
            })
        })
    }

    /// Whether a persistent worktree contains anything that makes reusing it
    /// under its commit unsafe. Cargo's untracked lockfile and brokkr's moved
    /// toolchain sidecars are the two known generated states.
    pub(crate) fn for_reuse(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| !(entry.paths.len() == 1 && is_generated_reuse_entry(entry)))
    }

    /// Whether deleting the checkout could destroy uncommitted work. This is
    /// deliberately the conservative question: every status entry counts.
    pub(crate) fn for_deletion(&self) -> bool {
        !self.entries.is_empty()
    }
}

fn parse_porcelain_z(bytes: &[u8], included_configs: &HashSet<PathBuf>) -> Option<Vec<Entry>> {
    let mut fields = bytes.split(|byte| *byte == 0).peekable();
    let mut entries = Vec::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        if field.len() < 4 || field[2] != b' ' {
            return None;
        }
        let path = path_from_bytes(&field[3..])?;
        let mut paths = vec![ClassifiedPath {
            class: classify_path(&path, included_configs),
            path,
        }];
        let index = field[0];
        let worktree = field[1];
        if index == b'R' || index == b'C' || worktree == b'R' || worktree == b'C' {
            let path = path_from_bytes(fields.next()?)?;
            paths.push(ClassifiedPath {
                class: classify_path(&path, included_configs),
                path,
            });
        }
        entries.push(Entry {
            index,
            worktree,
            paths,
        });
    }
    Some(entries)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Option<PathBuf> {
    Some(PathBuf::from(std::str::from_utf8(bytes).ok()?))
}

fn is_brokkr_store(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == crate::clean::layout::PROJECT_STATE_DIR)
}

fn classify_path(path: &Path, included_configs: &HashSet<PathBuf>) -> PathClass {
    if is_brokkr_store(path) {
        PathClass::BrokkrStore
    } else if is_toolchain_original(path) {
        PathClass::ToolchainOriginal
    } else if is_toolchain_sidecar(path) {
        PathClass::ToolchainSidecar
    } else if path == Path::new("Cargo.lock") {
        PathClass::CargoLock
    } else if path.extension().is_some_and(|ext| ext == "md") {
        PathClass::Prose
    } else if path == Path::new("brokkr.toml") {
        PathClass::RootConfig
    } else if included_configs.contains(path) {
        PathClass::IncludedConfig
    } else if crate::sluggrs::cmd::is_approved_baseline(path) {
        PathClass::ApprovedBaseline
    } else {
        PathClass::Source
    }
}

fn is_generated_reuse_entry(entry: &Entry) -> bool {
    let Some(path) = entry.paths.first() else {
        return false;
    };
    (entry.index == b'?' && entry.worktree == b'?' && path.class == PathClass::CargoLock)
        || (entry.index == b'?'
            && entry.worktree == b'?'
            && path.class == PathClass::ToolchainSidecar)
        || (entry.index == b' '
            && entry.worktree == b'D'
            && path.class == PathClass::ToolchainOriginal)
}

fn is_toolchain_original(path: &Path) -> bool {
    crate::toolchain::moved_paths().any(|(original, _)| path == Path::new(original))
}

fn is_toolchain_sidecar(path: &Path) -> bool {
    crate::toolchain::moved_paths().any(|(_, sidecar)| path == sidecar.as_path())
}

/// Every path brokkr's locked-window tree edits account for.
fn brokkr_tree_edits(root: &Path, entries: &[Entry]) -> HashSet<PathBuf> {
    let mut paths = unchanged_toolchain_moves(root, entries);
    paths.extend(unchanged_cargo_config_strips(root, entries));
    paths
}

/// The committed blob at `path` (relative to `root`) as (mode, bytes), when
/// HEAD has one.
fn head_blob(root: &Path, path: &str) -> Option<(String, Vec<u8>)> {
    let tree = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["ls-tree", "HEAD", "--", path])
            .current_dir(root),
    )
    .ok()?;
    if !tree.status.success() {
        return None;
    }
    let mode = String::from_utf8_lossy(&tree.stdout)
        .split_whitespace()
        .next()?
        .to_owned();
    let head = crate::output::child::command_output(
        crate::process::spawn::ChildPolicy::plain_command("git")
            .args(["show", &format!("HEAD:./{path}")])
            .current_dir(root),
    )
    .ok()?;
    head.status.success().then_some((mode, head.stdout))
}

/// A filtered cargo config and its moved original are exempt only in the
/// exact shape brokkr's install leaves: the live file modified in the
/// worktree and nothing else, the original untracked (or ignored, when it is
/// absent from status but present on disk), no transaction scratch beside
/// them, the original's bytes and executable bit equal to HEAD's, and the
/// live file verifying as brokkr's untouched install over that original (its
/// bytes and its mode). Any edit to either file stays dirty.
fn unchanged_cargo_config_strips(root: &Path, entries: &[Entry]) -> HashSet<PathBuf> {
    use crate::cargo_config_strip::transaction::{BASES, DISPLACED, INSTALLING, ORIGINAL};
    let status = |path: &Path| {
        entries
            .iter()
            .find(|entry| entry.paths.iter().any(|p| p.path == path))
            .map(|entry| (entry.index, entry.worktree))
    };
    let mut unchanged = HashSet::new();
    for base in BASES {
        let live = PathBuf::from(".cargo").join(base);
        let original = PathBuf::from(".cargo").join(format!("{base}{ORIGINAL}"));
        let original_untracked = match status(&original) {
            Some(state) => state == (b'?', b'?'),
            None => root.join(&original).is_file(),
        };
        if status(&live) != Some((b' ', b'M')) || !original_untracked {
            continue;
        }
        if [INSTALLING, DISPLACED]
            .iter()
            .any(|suffix| root.join(".cargo").join(format!("{base}{suffix}")).exists())
        {
            continue;
        }
        let Some(live_str) = live.to_str() else {
            continue;
        };
        let Some((mode, head)) = head_blob(root, live_str) else {
            continue;
        };
        let Ok(moved) = std::fs::read(root.join(&original)) else {
            continue;
        };
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(root.join(&original))
                .ok()
                .is_some_and(|meta| meta.permissions().mode() & 0o111 != 0)
        };
        if moved == head
            && (mode == "100755") == executable
            && crate::cargo_config_strip::verifies(&root.join(&live), &root.join(&original))
        {
            unchanged.insert(live);
            unchanged.insert(original);
        }
    }
    unchanged
}

/// Paths from a toolchain pin and its sidecar are exempt only when the sidecar
/// still contains the committed bytes. A hand edit made during the locked
/// window must remain visible as dirty.
fn unchanged_toolchain_moves(root: &Path, entries: &[Entry]) -> HashSet<PathBuf> {
    let mut unchanged = HashSet::new();
    for (name, sidecar) in crate::toolchain::moved_paths() {
        let original = PathBuf::from(name);
        let original_status = entries
            .iter()
            .find(|entry| entry.paths.iter().any(|path| path.path == original));
        let sidecar_status = entries
            .iter()
            .find(|entry| entry.paths.iter().any(|path| path.path == sidecar));
        let (Some(original_status), Some(sidecar_status)) = (original_status, sidecar_status)
        else {
            continue;
        };
        if original_status.index != b' '
            || original_status.worktree != b'D'
            || sidecar_status.index != b'?'
            || sidecar_status.worktree != b'?'
        {
            continue;
        }

        let tree = crate::output::child::command_output(
            crate::process::spawn::ChildPolicy::plain_command("git")
                .args(["ls-tree", "HEAD", "--", name])
                .current_dir(root),
        );
        let Ok(tree) = tree else { continue };
        if !tree.status.success() {
            continue;
        }
        let tree_line = String::from_utf8_lossy(&tree.stdout);
        let Some(mode) = tree_line.split_whitespace().next() else {
            continue;
        };
        let head = crate::output::child::command_output(
            crate::process::spawn::ChildPolicy::plain_command("git")
                .args(["show", &format!("HEAD:./{name}")])
                .current_dir(root),
        );
        let Ok(head) = head else { continue };
        if !head.status.success() {
            continue;
        }
        let moved = std::fs::read(root.join(&sidecar));
        let Ok(moved) = moved else { continue };
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(root.join(&sidecar))
                .ok()
                .is_some_and(|meta| meta.permissions().mode() & 0o111 != 0)
        };
        #[cfg(not(unix))]
        let executable = false;
        if moved == head.stdout && (mode == "100755") == executable {
            unchanged.insert(original);
            unchanged.insert(sidecar);
        }
    }
    unchanged
}
