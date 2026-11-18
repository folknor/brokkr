use std::path::Path;

use crate::build;
use crate::config;
use crate::error::DevError;
use crate::harness;
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
        command: crate::cli::invocation(),
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

/// One lock, path resolution and measurement provenance for a command.
/// A build is optional; external tools and HTTP lifecycles use the same session.
pub(crate) struct MeasurementSession {
    pub(crate) harness: harness::BenchHarness,
    /// The artifact a build-shaped session produced; its identity is on the
    /// harness, which stamps every row with it.
    binary: Option<build::BuiltArtifact>,
}

/// Whether a measurement request may run against a dirty tree.
///
/// Run mode does not persist results, so it is safe regardless of the tree
/// state. Stored measurement modes still require the explicit `--force` flag.
#[derive(Clone, Copy)]
pub(crate) enum DirtyPolicy {
    Allow,
    Gate { force: bool },
}

impl DirtyPolicy {
    pub(crate) fn for_request(req: &crate::measure::MeasureRequest<'_>) -> Self {
        match req.mode {
            crate::measure::MeasureMode::Run => Self::Allow,
            _ => Self::Gate { force: req.force },
        }
    }

    fn allowed(self) -> bool {
        match self {
            Self::Allow => true,
            Self::Gate { force } => force,
        }
    }
}

/// Resolve the worktree retention value for this machine at the shared
/// context boundary.
pub(crate) fn worktree_keep(dev_config: &config::DevConfig) -> Result<usize, DevError> {
    let hostname = config::hostname()?;
    Ok(dev_config.worktree_keep(&hostname))
}

impl MeasurementSession {
    /// The dirty-tree policy must be applied before `BenchHarness` is
    /// constructed, since its constructor performs the dirty-tree check.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        dev_config: &config::DevConfig,
        project: Project,
        project_root: &ProjectRoot,
        build_root: &BuildRoot,
        force: bool,
        stop_marker: Option<String>,
    ) -> Result<Self, DevError> {
        // The lock first, which activates the configured tree policy before
        // anything the session does with the tree; the harness is handed the
        // held lock. Path resolution is config alone and asks cargo nothing.
        let lock = lockfile::acquire(&lockfile::LockContext {
            project: Some(project.name()),
            command: crate::cli::invocation(),
            project_root,
            build_root,
            tree: configured_tree(),
        })?;
        let paths = bootstrap_config(dev_config, project_root)?;
        let harness = harness::BenchHarness::new_with_lock(
            lock,
            &paths,
            &dev_config.included_config_files,
            build_root,
            project_root,
            project,
            force,
            stop_marker,
        )?;
        Ok(Self {
            harness,
            binary: None,
        })
    }

    /// Attach the brokkr invocation string and measurement mode from the
    /// given `MeasureRequest` to the harness. Every row built by this
    /// harness will carry them in the `brokkr_args` and `variant`
    /// columns, so individual bench writers don't have to plumb them.
    ///
    /// Every measured command's session passes here, under its hold, before
    /// its first sample: a recorded mode refuses while a `brokkr serve`
    /// generation is outstanding.
    pub(crate) fn with_request(
        mut self,
        req: &crate::measure::MeasureRequest,
    ) -> Result<Self, DevError> {
        if let Some(mode) = req.mode.recorded()
            && !req.dry_run
        {
            crate::serve::refuse_measurement(&format!("a --{} run", mode.label()))?;
        }
        if req.mode.recorded().is_some() {
            self.harness.note_unneeded_force(req.force);
        }
        self.harness = self
            .harness
            .with_invocation(req.invocation.clone())
            .with_measure_mode(req.mode.recorded())
            .with_env_kv(config::captured_env_pairs(req.dev_config));
        Ok(self)
    }

    /// Create a session with a built executable: lock, then `cargo_build` of
    /// the caller's `BuildConfig`, then `BenchHarness`. Builds use the
    /// selected checkout while the results DB uses `project_root` (main
    /// tree); git info comes from the build root so the commit hash matches
    /// the built code.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_build_config(
        dev_config: &config::DevConfig,
        project: Project,
        project_root: &ProjectRoot,
        build_root: &BuildRoot,
        build_config: &build::BuildConfig,
        force: bool,
        stop_marker: Option<String>,
    ) -> Result<Self, DevError> {
        let mut session = Self::new(
            dev_config,
            project,
            project_root,
            build_root,
            force,
            stop_marker,
        )?;
        let effective_build_root = build_root.as_path();
        // A session build always carries this host's configured defaults,
        // even when its caller built the BuildConfig through a different path.
        // Request callers may already have merged them into the request; the
        // shared resolver dedupes that overlap before cargo and DB labeling.
        // Direct cargo command paths that do not construct this context retain
        // their own command or sweep feature selection.
        let effective_config = build::BuildConfig {
            package: build_config.package.clone(),
            bin: build_config.bin.clone(),
            example: build_config.example.clone(),
            features: crate::command_args::resolve_features(dev_config, &build_config.features),
            default_features: build_config.default_features,
            all_features: build_config.all_features,
            profile: build_config.profile.clone(),
        };
        let artifact = build::cargo_build(&effective_config, effective_build_root)?;
        // The row's `cargo_profile` and canonical `cargo_features` describe
        // the artifact cargo actually produced.
        session.harness = session
            .harness
            .with_build_identity(harness::BuildIdentity::of(&artifact));
        session.binary = Some(artifact);
        Ok(session)
    }

    /// As [`with_build_config`](Self::with_build_config), with dirty-tree
    /// handling derived from the request's mode in one shared place.
    pub(crate) fn with_build_config_for_request(
        req: &crate::measure::MeasureRequest<'_>,
        build_config: &build::BuildConfig,
    ) -> Result<Self, DevError> {
        Self::with_build_config(
            req.dev_config,
            req.project.clone(),
            req.project_root,
            req.build_root,
            build_config,
            DirtyPolicy::for_request(req).allowed(),
            req.stop_marker.map(str::to_owned),
        )
        .and_then(|ctx| ctx.with_request(req))
    }

    /// The built executable's path.
    pub(crate) fn binary(&self) -> Result<&Path, DevError> {
        self.binary
            .as_ref()
            .map(|artifact| artifact.path.as_path())
            .ok_or_else(|| DevError::Internal("session has no built executable".into()))
    }
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
                command: crate::cli::invocation(),
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
