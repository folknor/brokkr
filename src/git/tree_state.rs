//! One parsed view of a checkout's porcelain status, with named answers for
//! the different questions callers ask about its changes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        let output = Command::new("git")
            .args([
                "status",
                "--porcelain=v1",
                "-z",
                "--no-renames",
                "--untracked-files=all",
            ])
            .current_dir(root)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let entries = parse_porcelain_z(&output.stdout, included_configs)?;
        Some(Self {
            root: root.to_owned(),
            entries,
        })
    }

    /// Whether a source or build-input change would make a commit hash
    /// misdescribe the measurement. Documentation, brokkr-owned state,
    /// project config outputs, approved visual baselines, and an unchanged
    /// toolchain pin temporarily moved by brokkr are exempt.
    pub(crate) fn for_measurement(&self) -> bool {
        let unchanged_toolchain_moves = unchanged_toolchain_moves(&self.root, &self.entries);
        self.entries.iter().any(|entry| {
            entry.paths.iter().any(|path| {
                !unchanged_toolchain_moves.contains(&path.path)
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
    /// count, while brokkr's own state and an unchanged moved pin do not.
    pub(crate) fn for_record(&self) -> bool {
        let unchanged_toolchain_moves = unchanged_toolchain_moves(&self.root, &self.entries);
        self.entries.iter().any(|entry| {
            entry.paths.iter().any(|path| {
                !unchanged_toolchain_moves.contains(&path.path)
                    && path.class != PathClass::BrokkrStore
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
    } else if is_approved_baseline(path) {
        PathClass::ApprovedBaseline
    } else {
        PathClass::Source
    }
}

fn is_approved_baseline(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        (components.next(), components.next(), components.next(), components.next()),
        (
            Some(std::path::Component::Normal(root)),
            Some(std::path::Component::Normal(_snapshot)),
            Some(std::path::Component::Normal(file)),
            None
        ) if root == "snapshots" && file == "approved.png"
    )
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
    crate::toolchain::FILES
        .iter()
        .any(|name| path == Path::new(name))
}

fn is_toolchain_sidecar(path: &Path) -> bool {
    crate::toolchain::FILES
        .iter()
        .any(|name| path == Path::new(&format!("{name}{}", crate::toolchain::SUFFIX)))
}

/// Paths from a toolchain pin and its sidecar are exempt only when the sidecar
/// still contains the committed bytes. A hand edit made during the locked
/// window must remain visible as dirty.
fn unchanged_toolchain_moves(root: &Path, entries: &[Entry]) -> HashSet<PathBuf> {
    let mut unchanged = HashSet::new();
    for name in crate::toolchain::FILES {
        let original = PathBuf::from(name);
        let sidecar = PathBuf::from(format!("{name}{}", crate::toolchain::SUFFIX));
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

        let tree = Command::new("git")
            .args(["ls-tree", "HEAD", "--", name])
            .current_dir(root)
            .output();
        let Ok(tree) = tree else { continue };
        if !tree.status.success() {
            continue;
        }
        let tree_line = String::from_utf8_lossy(&tree.stdout);
        let Some(mode) = tree_line.split_whitespace().next() else {
            continue;
        };
        let head = Command::new("git")
            .args(["show", &format!("HEAD:./{name}")])
            .current_dir(root)
            .output();
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
