use std::path::{Path, PathBuf};

use crate::build;
use crate::config;
use crate::error::DevError;
use crate::harness;
use crate::lockfile;
use crate::output;
use crate::project::Project;
use crate::worktree;

/// Acquire the global per-user lock for a non-bench command.
///
/// Commands that build, write to data/scratch dirs, or run long-lived
/// processes should call this to prevent concurrent brokkr invocations
/// from corrupting shared state. The returned guard is held (and the
/// lock released) until the caller drops it.
pub(crate) fn acquire_cmd_lock(
    project: Project,
    project_root: &Path,
    command: &str,
) -> Result<lockfile::LockGuard, DevError> {
    lockfile::acquire(&lockfile::LockContext {
        project: project.name(),
        command,
        project_root: &project_root.display().to_string(),
    })
}

/// Resolve project info (target_dir) using cargo metadata.
pub(crate) fn bootstrap(build_root: Option<&Path>) -> Result<build::ProjectInfo, DevError> {
    build::project_info(build_root)
}

/// Resolve paths for the current host from an already-loaded config.
pub(crate) fn bootstrap_config(
    dev_config: &config::DevConfig,
    project_root: &Path,
    target_dir: &Path,
) -> Result<config::ResolvedPaths, DevError> {
    let hostname = config::hostname()?;
    let paths = config::resolve_paths(dev_config, &hostname, project_root, target_dir);
    Ok(paths)
}

// ---------------------------------------------------------------------------
// HarnessContext — shared bootstrap for no-build command handlers
// ---------------------------------------------------------------------------

/// Lighter context for commands that need a harness but don't build a binary
/// (allocator, planetiler, bench-all, profile, hotpath variants, etc.).
pub(crate) struct HarnessContext {
    pub(crate) paths: config::ResolvedPaths,
    pub(crate) harness: harness::BenchHarness,
}

impl HarnessContext {
    pub(crate) fn new(
        dev_config: &config::DevConfig,
        project: Project,
        project_root: &Path,
        build_root: Option<&Path>,
        lock_command: &str,
        force: bool,
    ) -> Result<Self, DevError> {
        let pi = bootstrap(build_root)?;
        let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
        let effective = build_root.unwrap_or(project_root);
        let db_root = build_root.map(|_| project_root);
        let harness = harness::BenchHarness::new(&paths, effective, db_root, project, lock_command, force)?;
        Ok(Self { paths, harness })
    }
}

// ---------------------------------------------------------------------------
// BenchContext — shared bootstrap for benchmark command handlers
// ---------------------------------------------------------------------------

pub(crate) struct BenchContext {
    pub(crate) paths: config::ResolvedPaths,
    pub(crate) harness: harness::BenchHarness,
    pub(crate) binary: PathBuf,
}

impl BenchContext {
    /// Create a new bench context.
    ///
    /// When `build_root` is `Some`, builds happen there (worktree) while data
    /// paths and the results DB use `project_root` (main tree). Git info is
    /// collected from the build root so the commit hash matches the built code.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        dev_config: &config::DevConfig,
        project: Project,
        project_root: &Path,
        build_root: Option<&Path>,
        package: Option<&str>,
        features: &[&str],
        default_features: bool,
        lock_command: &str,
        force: bool,
    ) -> Result<Self, DevError> {
        let effective_build_root = build_root.unwrap_or(project_root);
        let pi = bootstrap(build_root)?;
        let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
        // Acquire the lock BEFORE building so concurrent brokkr invocations
        // block here instead of competing for CPU during cargo build.
        let lock = lockfile::acquire(&lockfile::LockContext {
            project: project.name(),
            command: lock_command,
            project_root: &project_root.display().to_string(),
        })?;
        let build_config = if features.is_empty() && default_features {
            build::BuildConfig::release(package)
        } else if default_features {
            build::BuildConfig::release_with_features(package, features)
        } else {
            build::BuildConfig::release_no_defaults(package, features)
        };
        let cargo_features = if build_config.features.is_empty() {
            None
        } else {
            Some(build_config.features.join(","))
        };
        let binary = build::cargo_build(&build_config, effective_build_root)?;
        let db_root = build_root.map(|_| project_root);
        let harness = harness::BenchHarness::new_with_lock(lock, &paths, effective_build_root, db_root, project, force)?
            .with_cargo_features(cargo_features);
        Ok(Self { paths, harness, binary })
    }
}

// ---------------------------------------------------------------------------
// Worktree lifecycle helper
// ---------------------------------------------------------------------------

/// Run a closure with an optional git worktree for retroactive benchmarking.
///
/// When `commit` is `Some`, creates a worktree at that commit, passes
/// `Some(&worktree_path)` as `build_root` to the closure, and cleans up
/// afterwards (even on error).  When `None`, just calls `f(None)`.
pub(crate) fn with_worktree<F, T>(
    project_root: &Path,
    commit: Option<&str>,
    f: F,
) -> Result<T, DevError>
where
    F: FnOnce(Option<&Path>) -> Result<T, DevError>,
{
    match commit {
        Some(hash) => {
            let wt = worktree::Worktree::create(project_root, hash)?;
            output::bench_msg(&format!(
                "benchmarking commit {} ({})",
                wt.commit, wt.subject,
            ));
            let result = f(Some(&wt.path));
            if let Err(e) = wt.remove() {
                output::error(&format!("worktree cleanup failed: {e}"));
            }
            result
        }
        None => f(None),
    }
}
