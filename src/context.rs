//! Command context: the command lock, path bootstrap, and the `--commit`
//! worktree lifecycle
//! ([`with_worktree`]). [`bootstrap_config`] resolves host paths from config
//! alone; [`bootstrap`] is the cargo query a command that needs its target
//! dir makes of the tree it acts on.


use crate::build;
use crate::config;
use crate::error::DevError;
use crate::lockfile;
use crate::output;
use crate::project::{BuildRoot, Project, ProjectRoot};

/// The build root a fresh hold edits and the policy it applies, recorded once
/// at startup from the detected config. A brokkr process handles one CLI
/// invocation, and every nested lock acquisition in that invocation inherits
/// this detected policy. This is invocation context, used to fill
/// [`lockfile::LockContext::tree`]. The lock layer stores active paths on
/// each hold, including worktree paths.
static CONFIGURED_TREE: std::sync::OnceLock<
    Option<(std::path::PathBuf, crate::tree_policy::TreePolicy)>,
> = std::sync::OnceLock::new();

/// Record the invocation's tree target. The first call wins.
pub(crate) fn set_configured_tree(
    tree: Option<(std::path::PathBuf, crate::tree_policy::TreePolicy)>,
) {
    CONFIGURED_TREE.set(tree).ok();
}

fn configured_tree() -> Option<crate::tree_policy::TreeTarget<'static>> {
    CONFIGURED_TREE
        .get()
        .and_then(Option::as_ref)
        .map(|(dir, policy)| crate::tree_policy::TreeTarget { dir, policy })
}

/// Re-enter the invocation's hold without inventing a second lock label.
pub(crate) fn reenter_command_lock() -> Result<lockfile::LockGuard, DevError> {
    let (project_root, build_root) = crate::project::detect_roots()?;
    acquire_cmd_lock_opt(None, &project_root, &build_root)
}

/// Acquire the global per-user lock for a non-bench command.
///
/// Commands that build, write to data/scratch dirs, or run long-lived
/// processes should call this to prevent concurrent brokkr invocations
/// from corrupting shared state. The returned guard is held (and the
/// lock released) until the caller drops it.
///
/// Blocks until the lock is free. If another brokkr holds it, this prints
/// `[lock] waiting for …` then proceeds once released - so no command ever
/// exits with a confusing `lock: already locked` error that an agent might
/// "handle" by bypassing the tool or inventing a polling loop.
pub(crate) fn acquire_cmd_lock(
    project: &Project,
    project_root: &ProjectRoot,
    build_root: &BuildRoot,
) -> Result<lockfile::LockGuard, DevError> {
    acquire_cmd_lock_opt(Some(project), project_root, build_root)
}

/// As [`acquire_cmd_lock`], but for commands that run outside a detected
/// brokkr project (`check` / `test` / `run` in a bare Rust repo). Falls
/// back to the project label `"brokkr"` when no project is detected.
pub(crate) fn acquire_cmd_lock_opt(
    project: Option<&Project>,
    project_root: &ProjectRoot,
    build_root: &BuildRoot,
) -> Result<lockfile::LockGuard, DevError> {
    // The stray reap and the stale-guard warning run inside
    // `lockfile::acquire`'s fresh-hold path, so every acquirer gets them -
    // not only the commands that come through here.
    lockfile::acquire(&lockfile::LockContext {
        project: project.map(Project::name),
        command: crate::cli::invocation()?,
        project_root,
        build_root,
        tree: configured_tree(),
    })
}

/// Resolve project info (target_dir) using cargo metadata.
pub(crate) fn bootstrap(build_root: &BuildRoot) -> Result<build::ProjectInfo, DevError> {
    build::project_info(Some(build_root.as_path()))
}

/// Resolve paths for the current host from an already-loaded config: config
/// alone, no cargo query, so no code tree's state can fail it. A command
/// that needs the cargo target dir asks [`bootstrap`] of the tree it acts on.
pub(crate) fn bootstrap_config(
    dev_config: &config::DevConfig,
    project_root: &ProjectRoot,
) -> Result<config::ResolvedPaths, DevError> {
    let hostname = config::hostname()?;
    Ok(config::resolve_paths(
        dev_config,
        &hostname,
        project_root.as_path(),
    ))
}

/// Resolve the worktree retention value for this machine at the shared
/// context boundary.
pub(crate) fn worktree_keep(dev_config: &config::DevConfig) -> Result<usize, DevError> {
    let hostname = config::hostname()?;
    Ok(dev_config.worktree_keep(&hostname))
}

// ---------------------------------------------------------------------------
// Worktree lifecycle helper
// ---------------------------------------------------------------------------

/// Run a closure with an optional git worktree for retroactive benchmarking.
///
/// The code checkout supplies git identity; the separate project root owns
/// bookkeeping. A commit selects a derived checkout and `f` always receives
/// a concrete build root, including when no commit was requested.
///
/// With a commit (and not `dry_run`), the global lock is taken before any
/// worktree is evicted, replaced or created, and held until `f` returns; the
/// locks `f` takes re-enter that hold. A closure must therefore not wait on
/// another *process* that needs the brokkr lock.
///
/// Worktrees persist across runs so their isolated target dir survives.
/// Reuse is automatic when the same commit is requested again. Run
/// `brokkr clean --worktrees` to garbage collect.
#[allow(clippy::too_many_arguments)]
pub(crate) fn with_worktree<F, T>(
    project: Option<&Project>,
    project_root: &ProjectRoot,
    build_root: &BuildRoot,
    commit: Option<&str>,
    dry_run: bool,
    tree: &crate::tree_policy::TreePolicy,
    keep: usize,
    f: F,
) -> Result<T, DevError>
where
    F: FnOnce(&BuildRoot) -> Result<T, DevError>,
{
    // The git repo a worktree is cut from: the code tree (cwd) when the
    // config lives one level up, otherwise the project root itself.
    let git_root = build_root.as_path();
    match commit {
        Some(hash) if dry_run => {
            output::dry_run_msg(&format!("skipping worktree creation for {hash}"));
            f(build_root)
        }
        Some(hash) => {
            // Take the global lock BEFORE touching any worktree, and hold it
            // across the closure. Eviction and stale-replacement delete
            // directories another brokkr may be building in (`is_dirty` cannot
            // see that use - `target/` is gitignored), `worktrees.toml` is a
            // read-modify-write, and `clean --worktrees` removes the same
            // directories under the lock. The closure's own acquires re-enter
            // this hold (`lockfile::acquire` is re-entrant within the process),
            // so the build and measurement run under the same hold rather than
            // leaving a window between housekeeping and build.
            //
            // The record this acquisition writes is the one `brokkr lock` and a
            // waiter's message show for the whole run (the closure's re-entrant
            // acquires never rewrite it), so it carries the caller's project
            // and the invoked command, not a bookkeeping name.
            // The live build root's tree is unrelated to this run. Leave it
            // alone and explicitly activate the worktree's tree below, once
            // its checkout exists.
            let lock = lockfile::acquire_without_tree(&lockfile::LockContext {
                project: project.map(Project::name),
                command: crate::cli::invocation()?,
                project_root,
                build_root,
                tree: None,
            })?;

            // Retention runs from `create`'s cut hook: only when a worktree is
            // actually cut, never on reuse, so the cost lands beside a build
            // you are already paying for and no run is turned into a
            // destructive operation merely by happening. A project that stops
            // growing therefore never shrinks on its own - `clean --worktrees`
            // is the explicit hammer for that. Never fatal: housekeeping must
            // not fail the measurement that was actually asked for.
            let wt = build_root.at_commit(hash, |cutting| {
                if let Err(e) =
                    crate::worktree_record::enforce(project_root.as_path(), git_root, keep, cutting)
                {
                    output::warn(&format!("worktree retention: {e}"));
                }
            })?;
            if let Err(e) =
                crate::worktree_record::Store::touch(project_root.as_path(), wt.record_name())
            {
                output::warn(&format!("worktree bookkeeping: {e}"));
            }
            // The one line naming the commit. Neutral wording: this serves
            // `run`, `crit` and the measured commands alike. Quiet-gated like
            // the bench narration it replaces.
            if !output::is_quiet() {
                output::run_msg(&format!("commit {} ({})", wt.commit(), wt.subject()));
            }
            // The worktree is a checkout of `hash`, which carries its own
            // committed toolchain pin and cargo config. Apply the tree policy
            // to it for the closure. Safe as a bare guard here (unlike the
            // race `toolchain`'s module doc describes for `fmt`) because it
            // sits entirely inside the lock held above; declared after `lock`,
            // it drops - undoing the edits - before the flock is released. The
            // directory is registered first, so the watchdog can see every
            // edit that is ever applied.
            let registration = lock.track_tree_dir(wt.as_path());
            let tree_guard = crate::tree_policy::HeldTree::activate(wt.as_path(), tree)?;
            let result = f(&wt);
            drop(tree_guard);
            drop(registration);
            drop(lock);
            result
        }
        None => f(build_root),
    }
}

#[cfg(test)]
mod command_label_tests {
    #[test]
    fn lock_labels_are_command_names() {
        for id in crate::cli::COMMANDS {
            assert_eq!(crate::cli::CommandId::from_name(id.spec().name), Some(*id));
        }
    }
}
