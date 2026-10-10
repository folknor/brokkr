//! Git worktrees for retroactive runs (`--commit`).
//!
//! Creates a persistent worktree at a specific commit so we can build old
//! code while keeping data paths and the results DB in the main tree.
//! Worktrees are reused across runs and garbage-collected via `brokkr clean
//! --worktrees`.
//!
//! ## Placement
//!
//! Every worktree lives under one fixed container, `~/.brokkr/worktrees/`:
//!
//! ```text
//! ~/.brokkr/worktrees/<checkout key>/<short hash>/   the slot
//!     <checkout name>/                                the git worktree
//!     <dep>  ->  <live parent>/<dep>                  one per path dep leaving the repo
//!     .brokkr-target  ->  <effective target>/brokkr-worktrees/<checkout key>/<short hash>
//! ```
//!
//! **One container that exists before the run starts** is the only shape a
//! sandboxed agent's writable roots can express: those are bind mounts of
//! existing directories, so a worktree cut as a sibling of the project
//! (`~/Programs/.brokkr-worktree-*`, the old layout) writes a new entry into
//! `~/Programs` itself, which no grant short of all of `~/Programs` covers.
//! `~/.brokkr` is already granted, since the lock lives there.
//!
//! Not `<project>/.brokkr/worktrees/`, which would be a single grant too: cargo
//! searches *upward* from a package for its workspace root, so a checked-out
//! crate without a `[workspace]` of its own would find the live project's
//! `Cargo.toml` above it and refuse to build. Nothing above `~/.brokkr` has a
//! manifest.
//!
//! **The checkout key** is the checkout's directory name plus a hash of its
//! canonical path, so `~/Programs/foo` and `~/Programs/PRs/foo` keep separate
//! worktrees - what the parent directory used to do for siblings.
//!
//! **The checkout sits one level down in its slot** so relative path
//! dependencies (`../pbfhogg`) keep resolving: `<slot>/<name>/../pbfhogg` is
//! `<slot>/pbfhogg`, a symlink to the live sibling. The links are derived from
//! the commit's own manifests ([`outside_path_deps`]); a dependency climbing
//! more than one level out of the repo is refused rather than guessed at.
//!
//! **The target dir is not in the slot.** It lives under the live project's
//! *effective* target dir (`cargo metadata`'s `target_directory`, which already
//! accounts for `CARGO_TARGET_DIR`, `build.target-dir` and a `target` symlink),
//! so a worktree build lands on the same disk as every other build of the
//! project, and the free-space gate covers it. Each worktree still gets its own
//! directory there: sharing one with the live tree is how a reused worktree
//! once re-linked a stale binary (see `build::cargo_build_observed`). The
//! `.brokkr-target` link is how a build in the worktree finds it
//! ([`isolated_target_dir`]).

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::error::DevError;
use crate::output;

/// The container under `~/.brokkr` every worktree lives in.
const CONTAINER: &str = "worktrees";

/// The slot entry linking to the worktree's isolated target dir.
const TARGET_LINK: &str = ".brokkr-target";

/// The directory under the project's effective target dir that holds the
/// worktrees' isolated target dirs.
const TARGET_SUBDIR: &str = "brokkr-worktrees";

/// Name prefix of the legacy sibling layout
/// (`<parent>/.brokkr-worktree-<project>-<short>`). Only `clean --worktrees`
/// still looks for these, so worktrees cut before the move are not orphaned.
pub const LEGACY_PREFIX: &str = ".brokkr-worktree-";

/// `~/.brokkr/worktrees`.
fn container() -> Result<PathBuf, DevError> {
    let home = std::env::var("HOME")
        .map_err(|_| DevError::Config("$HOME is not set - cannot locate the worktree container".into()))?;
    Ok(PathBuf::from(home).join(".brokkr").join(CONTAINER))
}

/// The checkout's directory name (`piners`).
fn checkout_name(git_root: &Path) -> String {
    git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_owned()
}

/// `<name>-<hash of the canonical path>`: unique per checkout, readable at a
/// glance.
fn checkout_key(git_root: &Path) -> String {
    let canonical = std::fs::canonicalize(git_root).unwrap_or_else(|_| git_root.to_owned());
    let hash = xxhash_rust::xxh3::xxh3_64(canonical.as_os_str().as_encoded_bytes());
    format!("{}-{:08x}", checkout_name(git_root), hash >> 32)
}

/// The directory holding every slot of one checkout.
fn checkout_dir(git_root: &Path) -> Result<PathBuf, DevError> {
    Ok(container()?.join(checkout_key(git_root)))
}

/// The git worktree inside a slot.
pub fn checkout_in(git_root: &Path, slot: &Path) -> PathBuf {
    slot.join(checkout_name(git_root))
}

/// A slot's name in `.brokkr/worktrees.toml`: `<checkout key>/<short hash>`.
pub fn record_name(slot: &Path) -> Option<String> {
    let short = slot.file_name()?.to_str()?;
    let key = slot.parent()?.file_name()?.to_str()?;
    Some(format!("{key}/{short}"))
}

/// The record-name prefix shared by every slot of `git_root`: `<key>/`.
pub fn record_prefix(git_root: &Path) -> String {
    format!("{}/", checkout_key(git_root))
}

/// The target dir a build in `build_root` must use, when `build_root` is a
/// worktree brokkr cut: the slot's [`TARGET_LINK`]. `None` for every other
/// tree, which keeps whatever target dir its own config gives it.
///
/// Used by [`crate::build::cargo_build_observed`] and `brokkr bench` to pin
/// `CARGO_TARGET_DIR`, which overrides any inherited `build.target-dir` or
/// exported `CARGO_TARGET_DIR` that would otherwise put the worktree's build
/// in the live tree's target.
pub fn isolated_target_dir(build_root: &Path) -> Option<PathBuf> {
    let container = container().ok()?;
    let slot = build_root.parent()?;
    (slot.parent()?.parent()? == container).then(|| slot.join(TARGET_LINK))
}

/// A persistent git worktree checked out at a specific commit.
///
/// Created on demand by `Worktree::create` and reused on subsequent runs at
/// the same commit. Use `brokkr clean --worktrees` to garbage collect.
pub struct Worktree {
    /// Absolute path to the git worktree (inside its slot).
    pub path: PathBuf,
    /// Short commit hash ([`crate::git::short_of`]).
    pub commit: String,
    /// First line of the commit message.
    pub subject: String,
    /// The slot's key in `.brokkr/worktrees.toml` ([`record_name`]).
    pub record_name: String,
}

impl Worktree {
    /// Create a worktree of `git_root` at the given commit ref (hash, branch,
    /// tag, HEAD~N, etc.), or reuse the one already there.
    ///
    /// Reuse requires the checkout to be **pristine** ([`is_pristine`]). The
    /// container is writable to sandboxed agents, so a worktree can be edited
    /// between runs, and a run reusing an edited one would measure modified
    /// code under the commit's name. That fails the run with the path rather
    /// than resetting it: nobody should be working in a worktree, but if
    /// someone is, deleting their work is not brokkr's call.
    ///
    /// `before_cut` runs only when a worktree is about to be *cut* - never on
    /// reuse - and is handed the slot about to be (re)created. It is the
    /// retention hook ([`crate::worktree_record::enforce`]): eviction belongs to
    /// growth, and a reuse does not grow anything. Passing the slot lets
    /// eviction leave it out of the count and out of the victim list, so the
    /// worktree about to be replaced is never evicted as a bystander.
    ///
    /// A cut is gated on the container's free space, like a build is on its
    /// target's: the checkout is written there, which may be a different disk
    /// from the target.
    ///
    /// The caller must hold the global lock: this removes and creates
    /// directories another brokkr may be building in.
    pub fn create(
        git_root: &Path,
        commit_ref: &str,
        before_cut: impl FnOnce(&Path),
    ) -> Result<Self, DevError> {
        // Validate the commit exists and resolve to a full hash for comparison.
        // The slot name is the fixed-width abbreviation, so the same commit
        // names the same worktree whatever width (or ref) it was asked for by,
        // and however large the repo has grown since.
        let crate::git::CommitId { full: full_hash, short } =
            crate::git::resolve_commit(git_root, commit_ref)?;
        let subject = run_git(git_root, &["log", "-1", "--format=%s", &full_hash])?;

        let slot = checkout_dir(git_root)?.join(&short);
        let checkout = checkout_in(git_root, &slot);
        let record_name = record_name(&slot)
            .ok_or_else(|| DevError::Config(format!("bad worktree slot {}", slot.display())))?;

        // Reuse path: if a worktree already exists here and its HEAD matches
        // the requested commit, skip remove + re-add.
        if checkout.exists()
            && let Ok(head) = run_git(&checkout, &["rev-parse", "HEAD"])
            && head == full_hash
        {
            if !is_pristine(&checkout) {
                return Err(DevError::Refused(format!(
                    "worktree at {} has been modified since brokkr cut it, so a run would \
                     measure changed code under {short}'s name; refusing to reuse it. Remove \
                     it with `brokkr clean --worktrees` (or move any work out and delete the \
                     directory), then rerun",
                    checkout.display()
                )));
            }
            output::run_msg(&format!("reusing worktree for {short} ({subject})"));
            // Idempotent: repairs a slot whose furnishing was interrupted.
            furnish(git_root, &slot, &checkout)?;
            return Ok(Self {
                path: checkout,
                commit: short,
                subject,
                record_name,
            });
        }

        // Stale (different commit, git lost track of the dir, or HEAD could
        // not be read at all). A failed `rev-parse` is not proof of staleness -
        // it can be transient - so replacement goes through the same dirty
        // rule eviction obeys: `is_dirty` reads "git could not answer" as
        // dirty, and a dirty tree is refused rather than force-removed. The
        // cost of a wrong refusal is one rerun; the cost of a wrong removal is
        // somebody's uncommitted work.
        let stale = slot.exists();
        if stale && checkout.exists() && is_dirty(&checkout) {
            return Err(DevError::Refused(format!(
                "worktree at {} is not checked out at {short} and has uncommitted work \
                 (or git could not read it); refusing to replace it. Commit or move the \
                 work, remove the directory by hand, then rerun",
                checkout.display()
            )));
        }

        crate::disk_gate::check_paths(&[("worktrees", container()?)])?;

        before_cut(&slot);

        if stale {
            output::run_msg(&format!("removing stale worktree at {}", slot.display()));
            remove_one(git_root, &slot)?;
        }

        output::run_msg(&format!("creating worktree for {short} ({subject})"));
        output::run_msg(
            "  worktree is persistent - run `brokkr clean --worktrees` to remove",
        );

        std::fs::create_dir_all(&slot)?;
        let checkout_str = checkout.display().to_string();
        run_git(
            git_root,
            &["worktree", "add", "--detach", &checkout_str, &full_hash],
        )?;
        furnish(git_root, &slot, &checkout)?;

        Ok(Self {
            path: checkout,
            commit: short,
            subject,
            record_name,
        })
    }
}

/// Create the slot's links: the isolated target dir and one link per path
/// dependency that leaves the repo. Skips any that already exist.
fn furnish(git_root: &Path, slot: &Path, checkout: &Path) -> Result<(), DevError> {
    let target_link = slot.join(TARGET_LINK);
    if target_link.symlink_metadata().is_err() {
        let info = crate::build::project_info(Some(git_root))?;
        let base = std::fs::canonicalize(&info.target_dir).unwrap_or(info.target_dir);
        let (Some(key), Some(short)) = (
            slot.parent().and_then(Path::file_name),
            slot.file_name(),
        ) else {
            return Err(DevError::Config(format!("bad worktree slot {}", slot.display())));
        };
        let dir = base.join(TARGET_SUBDIR).join(key).join(short);
        std::fs::create_dir_all(&dir)?;
        std::os::unix::fs::symlink(&dir, &target_link)?;
    }

    let live_parent = git_root
        .parent()
        .ok_or_else(|| DevError::Config("project root has no parent directory".into()))?;
    let own_name = checkout_name(git_root);
    for dep in outside_path_deps(checkout)? {
        if dep == own_name || dep == TARGET_LINK {
            continue;
        }
        let link = slot.join(&dep);
        if link.symlink_metadata().is_ok() {
            continue;
        }
        std::os::unix::fs::symlink(live_parent.join(&dep), &link)?;
    }
    Ok(())
}

/// The sibling directories the checkout's path dependencies point at: for
/// every `path = "..."` dependency in every tracked `Cargo.toml` that resolves
/// one level above the repo root, the first component out there (`pbfhogg` for
/// `../pbfhogg/crates/x`). A dependency climbing further than one level is an
/// error: the slot is the only level brokkr controls.
fn outside_path_deps(checkout: &Path) -> Result<Vec<String>, DevError> {
    let listed = run_git(checkout, &["ls-files"])?;
    let mut out: Vec<String> = Vec::new();
    for rel in listed.lines() {
        let rel = Path::new(rel);
        if rel.file_name().and_then(|n| n.to_str()) != Some("Cargo.toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(checkout.join(rel)) else {
            continue;
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            continue;
        };
        let mut paths = Vec::new();
        collect_dep_paths(&table, &mut paths);
        let manifest_dir = rel.parent().unwrap_or(Path::new(""));
        for dep in paths {
            match escape_of(manifest_dir, &dep) {
                Escape::Inside => {}
                Escape::Sibling(name) => {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
                Escape::TooFar => {
                    return Err(DevError::Refused(format!(
                        "{} has a path dependency '{dep}' reaching more than one level above \
                         the repository; a --commit worktree can only provide siblings of \
                         the checkout",
                        rel.display()
                    )));
                }
            }
        }
    }
    Ok(out)
}

/// Every dependency `path` in a manifest: the `*dependencies` tables at any
/// depth (`[target.'cfg(..)'.dependencies]`, `[workspace.dependencies]`) and
/// `[patch.<registry>]`.
fn collect_dep_paths(table: &toml::Table, out: &mut Vec<String>) {
    const DEP_KEYS: [&str; 5] = [
        "dependencies",
        "dev-dependencies",
        "dev_dependencies",
        "build-dependencies",
        "build_dependencies",
    ];
    for (key, value) in table {
        let Some(sub) = value.as_table() else {
            continue;
        };
        if DEP_KEYS.contains(&key.as_str()) {
            push_entry_paths(sub, out);
        } else if key == "patch" {
            for registry in sub.values().filter_map(toml::Value::as_table) {
                push_entry_paths(registry, out);
            }
        } else {
            collect_dep_paths(sub, out);
        }
    }
}

fn push_entry_paths(deps: &toml::Table, out: &mut Vec<String>) {
    for entry in deps.values() {
        if let Some(path) = entry.get("path").and_then(toml::Value::as_str) {
            out.push(path.to_owned());
        }
    }
}

/// Where a path dependency lands relative to the repo root.
#[derive(Debug, PartialEq, Eq)]
enum Escape {
    /// Inside the repo, or absolute - nothing for the slot to provide.
    Inside,
    /// One level above the root: this sibling directory.
    Sibling(String),
    /// Further out than the slot reaches.
    TooFar,
}

/// Lexically resolve `dep` against `manifest_dir` (relative to the repo root),
/// the way cargo joins a path dependency to its manifest's directory.
fn escape_of(manifest_dir: &Path, dep: &str) -> Escape {
    let dep = Path::new(dep);
    if dep.is_absolute() {
        return Escape::Inside;
    }
    // Normal components, with any unmatched `..` kept at the front.
    let mut parts: Vec<String> = Vec::new();
    for c in manifest_dir.components().chain(dep.components()) {
        match c {
            Component::Normal(n) => parts.push(n.to_string_lossy().into_owned()),
            Component::ParentDir => {
                if parts.last().is_some_and(|p| p != "..") {
                    parts.pop();
                } else {
                    parts.push("..".into());
                }
            }
            _ => {}
        }
    }
    match parts.iter().take_while(|p| *p == "..").count() {
        0 => Escape::Inside,
        1 => parts.get(1).map_or(Escape::TooFar, |name| Escape::Sibling(name.clone())),
        _ => Escape::TooFar,
    }
}

/// Every slot of `git_root`'s checkout, without touching them. `git_root` must
/// be the **build** root - the same root [`Worktree::create`] was given - since
/// the checkout key is derived from it.
pub fn list(git_root: &Path) -> Result<Vec<PathBuf>, DevError> {
    let dir = checkout_dir(git_root)?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_worktree_name("", name) {
            found.push(path);
        }
    }
    Ok(found)
}

/// Legacy sibling worktrees of `git_root`
/// (`<parent>/.brokkr-worktree-<name>-<short>`), from before the container.
pub fn list_legacy(git_root: &Path) -> Result<Vec<PathBuf>, DevError> {
    let Some(parent) = git_root.parent() else {
        return Ok(Vec::new());
    };
    let prefix = legacy_prefix(git_root);
    let entries = match std::fs::read_dir(parent) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_worktree_name(&prefix, name) {
            found.push(path);
        }
    }
    Ok(found)
}

/// The legacy sibling-name prefix for `root`: `.brokkr-worktree-<root name>-`.
fn legacy_prefix(root: &Path) -> String {
    format!("{LEGACY_PREFIX}{}-", checkout_name(root))
}

/// True when `name` is `prefix` followed by a bare short hash.
///
/// Matched by construction, not by prefix alone. A prefix test lets checkout
/// `foo` claim checkout `foo-bar`'s legacy worktrees
/// (`.brokkr-worktree-foo-bar-1a2b` starts with `.brokkr-worktree-foo-`), and
/// lets one checkout's records in the shared `worktrees.toml` be read as
/// another's. A short hash is hex with no hyphen, which the other checkout's
/// remainder (`bar-1a2b`) can never be.
pub fn is_worktree_name(prefix: &str, name: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Remove every worktree of `git_root`'s checkout - container slots and legacy
/// siblings alike. Returns the number removed.
///
/// Unlike retention eviction this does **not** skip dirty worktrees: it is the
/// explicit hammer (`brokkr clean --worktrees`), asked for by name, and a purge
/// that quietly kept some would leave the user believing the disk was clear.
pub fn purge_all(git_root: &Path) -> Result<usize, DevError> {
    let mut removed = 0usize;
    for slot in list(git_root)? {
        output::run_msg(&format!(
            "removing worktree {}",
            record_name(&slot).unwrap_or_else(|| slot.display().to_string())
        ));
        remove_one(git_root, &slot)?;
        removed += 1;
    }
    for path in list_legacy(git_root)? {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("(unnamed)");
        output::run_msg(&format!("removing legacy worktree {name}"));
        remove_checkout(git_root, &path)?;
        removed += 1;
    }
    Ok(removed)
}

/// Remove one slot: its isolated target dir, the git worktree, the slot.
///
/// The one removal sequence: shared by `purge_all`, by retention eviction
/// ([`crate::worktree_record::enforce`]) and by `create`'s stale-replacement
/// path. Callers decide *whether* to remove (eviction and `create` apply the
/// dirty rule first; the purge does not); this decides *how*.
///
/// The target dir is followed through the slot's link only when the link's
/// destination is the name brokkr constructs for this slot
/// (`brokkr-worktrees/<key>/<short>`). A link someone repointed is removed as a
/// link and its destination left alone.
pub fn remove_one(git_root: &Path, slot: &Path) -> Result<(), DevError> {
    if let Ok(dest) = std::fs::read_link(slot.join(TARGET_LINK))
        && let (Some(key), Some(short)) = (slot.parent().and_then(Path::file_name), slot.file_name())
        && dest.ends_with(Path::new(TARGET_SUBDIR).join(key).join(short))
        && dest.exists()
    {
        std::fs::remove_dir_all(&dest).map_err(|e| {
            DevError::Io(std::io::Error::new(
                e.kind(),
                format!("cannot remove worktree target dir {}: {e}", dest.display()),
            ))
        })?;
    }
    remove_checkout(git_root, &checkout_in(git_root, slot))?;
    // `remove_dir_all` does not follow symlinks: the dependency links go, the
    // live checkouts they point at stay.
    if slot.symlink_metadata().is_ok() {
        std::fs::remove_dir_all(slot).map_err(|e| {
            DevError::Io(std::io::Error::new(
                e.kind(),
                format!("cannot remove worktree slot {}: {e}", slot.display()),
            ))
        })?;
    }
    Ok(())
}

/// Remove a git worktree directory: ask git, then fall back to a directory
/// removal if git left it behind, then prune git's bookkeeping.
///
/// The git failure is not an error on its own - "not a working tree" is the
/// expected answer for a directory git has lost track of, which is exactly
/// what the filesystem fallback is for - but it is carried into the error when
/// the fallback fails too, since it is usually the more telling of the two.
fn remove_checkout(git_root: &Path, path: &Path) -> Result<(), DevError> {
    let git = run_git(
        git_root,
        &["worktree", "remove", "--force", &path.display().to_string()],
    );
    if path.exists()
        && let Err(e) = std::fs::remove_dir_all(path)
    {
        let git_note = match &git {
            Err(g) => format!(" (git worktree remove also failed: {g})"),
            Ok(_) => String::new(),
        };
        return Err(DevError::Io(std::io::Error::new(
            e.kind(),
            format!("cannot remove worktree at {}: {e}{git_note}", path.display()),
        )));
    }
    drop(run_git(git_root, &["worktree", "prune"]));
    Ok(())
}

/// True when the worktree has uncommitted or untracked content.
///
/// Deliberately does not exclude anything. `git::check_clean`'s exclusions
/// exist so brokkr's own outputs can't block a *measurement*; here the question
/// is whether deleting this directory would destroy work, and for that, an
/// untracked file counts.
pub fn is_dirty(path: &Path) -> bool {
    porcelain(path).is_none_or(|lines| !lines.is_empty())
}

/// True when the checkout holds exactly its commit, for reuse.
///
/// Stricter than [`git::check_clean`](crate::git)'s measurement rule - an
/// untracked source file counts, since cargo auto-discovers `src/bin/*.rs` and
/// examples - but blind to the two states brokkr itself leaves: the toolchain
/// pin moved aside by a run that was hard-killed (`toolchain::activate` adopts
/// that sidecar on the next lock), and a `Cargo.lock` cargo generated for a
/// commit that tracked none. A checkout git cannot read is not pristine.
pub fn is_pristine(path: &Path) -> bool {
    let Some(lines) = porcelain(path) else {
        return false;
    };
    lines.iter().all(|line| {
        let (status, file) = line.split_at(line.len().min(3));
        match status {
            "?? " => file == "Cargo.lock" || file.ends_with(crate::toolchain::SUFFIX),
            " D " => file == "rust-toolchain" || file == "rust-toolchain.toml",
            _ => false,
        }
    })
}

/// `git status --porcelain` lines, or `None` when git cannot answer.
fn porcelain(path: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}

/// Run a git command in the given directory and return trimmed stdout.
fn run_git(cwd: &Path, args: &[&str]) -> Result<String, DevError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| DevError::Spawn { program: "git".into(), error })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DevError::Subprocess {
            program: "git".into(),
            code: output.status.code(),
            stderr: stderr.trim().to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Escape, collect_dep_paths, escape_of, is_worktree_name, legacy_prefix};
    use std::path::Path;

    #[test]
    fn legacy_worktree_name_is_prefix_plus_short_hash() {
        let prefix = legacy_prefix(Path::new("/src/foo"));
        assert_eq!(prefix, ".brokkr-worktree-foo-");
        assert!(is_worktree_name(&prefix, ".brokkr-worktree-foo-1a2b3c4"));
        assert!(!is_worktree_name(&prefix, ".brokkr-worktree-foo-"));
        assert!(!is_worktree_name(&prefix, ".brokkr-worktree-bar-1a2b3c4"));
    }

    #[test]
    fn a_checkout_does_not_claim_a_hyphen_extended_siblings_worktrees() {
        // `foo` and `foo-bar` share a parent under the config-one-level-up
        // layout; a bare prefix test would let `foo` purge these.
        let prefix = legacy_prefix(Path::new("/src/foo"));
        assert!(!is_worktree_name(&prefix, ".brokkr-worktree-foo-bar-1a2b3c4"));
        assert!(!is_worktree_name(&prefix, ".brokkr-worktree-foo-beef-1a2b3c4"));
    }

    #[test]
    fn a_slot_name_is_a_bare_short_hash() {
        assert!(is_worktree_name("", "a5cc1f8"));
        assert!(!is_worktree_name("", ".brokkr-target"));
        assert!(!is_worktree_name("", ""));
    }

    #[test]
    fn path_deps_resolve_lexically_against_the_manifest_dir() {
        let root = Path::new("");
        assert_eq!(escape_of(root, "crates/x"), Escape::Inside);
        assert_eq!(escape_of(root, "../pbfhogg"), Escape::Sibling("pbfhogg".into()));
        assert_eq!(
            escape_of(root, "../pbfhogg/crates/core"),
            Escape::Sibling("pbfhogg".into())
        );
        // A member crate two deep climbing three levels lands one level out.
        let member = Path::new("crates/x");
        assert_eq!(escape_of(member, "../y"), Escape::Inside);
        assert_eq!(escape_of(member, "../../../pbfhogg"), Escape::Sibling("pbfhogg".into()));
        assert_eq!(escape_of(root, "../../far"), Escape::TooFar);
        assert_eq!(escape_of(root, ".."), Escape::TooFar);
        assert_eq!(escape_of(root, "/abs/dep"), Escape::Inside);
    }

    #[test]
    fn dep_paths_are_found_in_every_dependency_table() {
        let manifest: toml::Table = r#"
            [dependencies]
            a = { path = "../a" }
            reg = "1"
            [dev-dependencies]
            b = { path = "../b" }
            [target.'cfg(unix)'.build-dependencies]
            c = { path = "../c" }
            [workspace.dependencies]
            d = { path = "../d" }
            [patch.crates-io]
            e = { path = "../e" }
            [package]
            build = "../not-a-dep/build.rs"
        "#
        .parse()
        .expect("toml");
        let mut paths = Vec::new();
        collect_dep_paths(&manifest, &mut paths);
        paths.sort();
        assert_eq!(paths, vec!["../a", "../b", "../c", "../d", "../e"]);
    }
}
