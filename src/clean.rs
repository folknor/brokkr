//! Presentation and cleanup policy for `brokkr clean`.

use std::path::Path;

pub(crate) mod layout;

pub(crate) const CORPUS_CALIBRAND_DIR: &str = "corpus-calibrands";

use crate::config;
use crate::context::{bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::litehtml;
use crate::output;
use crate::project::Project;
use crate::worktree;

/// What a `brokkr clean` run should reclaim beyond the routine scratch. `cargo`
/// is handled at the dispatch site (it shells out); everything else flows
/// through here.
#[derive(Clone, Copy)]
pub(crate) struct CleanOpts {
    /// The dispatch site already reported the cargo sweep.
    pub(crate) cargo_swept: bool,
    pub(crate) worktrees: bool,
    pub(crate) archives: bool,
    pub(crate) keep: usize,
    pub(crate) dry_run: bool,
    /// The sweep runs on the interrupt/kill path: it says nothing beyond what
    /// it removes (no hints, no "nothing to clean").
    pub(crate) interrupted: bool,
}

impl CleanOpts {
    /// Scratch/tmp cleanup after an interrupt or cooperative kill, without
    /// the hints and empty-sweep notice of an interactive clean.
    pub(crate) fn after_interrupt() -> Self {
        Self {
            cargo_swept: false,
            worktrees: false,
            archives: false,
            keep: 2,
            dry_run: false,
            interrupted: true,
        }
    }
}

/// Removal helper that honours `--dry-run`: in dry-run it reports the path and
/// removes nothing; otherwise it deletes and propagates failures. Counts only
/// paths that were removed or would be removed so the caller can tell an empty
/// sweep from a productive one.
struct Cleaner {
    dry_run: bool,
    hits: std::cell::Cell<usize>,
}

impl Cleaner {
    fn new(dry_run: bool) -> Self {
        Self {
            dry_run,
            hits: std::cell::Cell::new(0),
        }
    }

    fn dir(&self, path: &Path) -> Result<bool, DevError> {
        let removed = if self.dry_run {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata.file_type().is_dir() || metadata.file_type().is_symlink(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(clean_io_error("cannot inspect", path, &error)),
            }
        } else {
            match std::fs::remove_dir_all(path) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(clean_io_error("cannot remove directory", path, &error)),
            }
        };
        if removed {
            self.hits.set(self.hits.get() + 1);
        }
        Ok(removed)
    }

    fn file(&self, path: &Path) -> Result<bool, DevError> {
        let removed = if self.dry_run {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) => !metadata.file_type().is_dir() || metadata.file_type().is_symlink(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(clean_io_error("cannot inspect", path, &error)),
            }
        } else {
            match std::fs::remove_file(path) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(clean_io_error("cannot remove file", path, &error)),
            }
        };
        if removed {
            self.hits.set(self.hits.get() + 1);
        }
        Ok(removed)
    }

    /// Verb for "N files" style messages.
    fn verb(&self) -> &'static str {
        if self.dry_run {
            "would remove"
        } else {
            "removed"
        }
    }

    /// Verb for "X" (named target) style messages.
    fn past(&self) -> &'static str {
        if self.dry_run {
            "would clean"
        } else {
            "cleaned"
        }
    }
}

fn clean_io_error(action: &str, path: &Path, source: &std::io::Error) -> DevError {
    DevError::Io(std::io::Error::new(
        source.kind(),
        format!("{action} {}: {source}", path.display()),
    ))
}

pub(crate) fn cmd_clean(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &crate::project::ProjectRoot,
    build_root: &crate::project::BuildRoot,
    opts: CleanOpts,
) -> Result<(), DevError> {
    let CleanOpts {
        cargo_swept,
        worktrees,
        archives,
        keep,
        dry_run,
        interrupted,
    } = opts;
    let c = Cleaner::new(dry_run);
    // Worktrees are keyed by the *build* root, because that is the git repo
    // `Worktree::create` cuts them from and the checkout whose path names their
    // directory in the container (and, for legacy siblings, their parent and
    // prefix). `worktree::list` derives all of that from whatever root it is
    // handed, so handing it the project root under the config-one-level-up
    // layout looks under the wrong key and reports zero - a silent no-op for
    // the flag whose whole job is reclaiming gigabytes.
    //
    // Passed in rather than re-derived from cwd. It is the same value today -
    // `project::detect` sets the build root to cwd - but re-deriving it here
    // states an invariant this function cannot enforce, and the caller already
    // holds the real one. Two roots that must agree should be threaded, not
    // independently recomputed and hoped to match.
    let worktree_root = build_root.as_path();
    let pi = bootstrap(build_root)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    let project_root = project_root.as_path();
    let state = layout::ProjectState::new(project_root);

    // The verify harness writes only under target/verify.
    if project == Project::Pbfhogg {
        let verify_dir = layout::PbfhoggLayout::new(&paths).verify_output();
        if c.dir(&verify_dir)? {
            output::run_msg(&format!("{} verify output", c.verb()));
        }
    }

    // Reclaim the per-sweep isolated target dirs a `rustflags` `[[check]]`
    // entry mints (`<target>/rustflags-<hash>`). Under `pi.target_dir` -
    // cargo's real resolved target dir, where the check/test phase actually
    // built them - not `paths.target_dir` (which a host `target` override can
    // repoint). Routine scratch: reproducible, and nothing else reaches them.
    clean_rustflags_target_dirs(&pi.target_dir, &c)?;

    clean_scratch(project, &paths, &c)?;

    // Elivagar's default `-o` target for `pmtiles-corpus mutate` is a
    // brokkr-designated scratch location. `ocean-build_tmp` is not passed by
    // brokkr and belongs to the external tool, so clean leaves it alone.
    if project == Project::Elivagar {
        let elivagar = layout::ElivagarLayout::new(&paths);
        if c.dir(&elivagar.corpus_calibrands())? {
            output::run_msg(&format!("{} corpus-calibrands", c.past()));
        }
        if archives {
            clean_archives(&paths, keep, &c)?;
        }
    }

    clean_artefact_trees(project, &state, &c)?;

    if c.dir(&state.test_hung_dir())? {
        output::run_msg(&format!("{} test-hung snapshots", c.verb()));
    }

    if let Some(status) = crate::user_dirs::sidecar_status()
        && c.file(&status)?
    {
        output::run_msg(&format!("{} sidecar status", c.verb()));
    }

    // Litehtml: the per-user Node script cache keeps one directory per bundled
    // script version, including the hashed capture script, each with its own
    // node_modules. There is no project-local `.brokkr/capture.js`; every stale
    // version goes and the current one stays so the next run does not reinstall.
    if project == Project::Litehtml {
        if c.dir(state.litehtml().prepare_cache())? {
            output::run_msg(&format!("{} litehtml prepare cache", c.verb()));
        }
        let stale = litehtml::cmd::stale_script_cache_dirs();
        for dir in &stale {
            c.dir(dir)?;
        }
        if !stale.is_empty() {
            output::run_msg(&format!(
                "{} {}",
                c.verb(),
                output::count(stale.len(), "stale litehtml script cache")
            ));
        }
    }

    // An empty routine sweep says so once, instead of printing nothing (which
    // reads as "did it run?"). Cargo and the deep flags report their own work.
    if !cargo_swept && !worktrees && !archives && !interrupted && !dry_run && c.hits.get() == 0 {
        output::run_msg("nothing to clean");
    }

    if worktrees {
        // Deep clean: `--worktrees` reclaims the expensive *persistent* state.
        // The durable tilegen output store is elivagar's analog of piners'
        // `runs.db` (the archives `regress` diffs), so a routine `brokkr clean`
        // spares it - retention already bounds its growth. Only the explicit
        // deep clean wipes it wholesale, alongside the worktrees.
        if project == Project::Elivagar {
            clean_elivagar_outputs(&paths, &c)?;
        }
        // Legacy sibling worktrees (from before the `~/.brokkr/worktrees`
        // container) count and go too, or the move would orphan them.
        let found =
            worktree::list(worktree_root)?.len() + worktree::list_legacy(worktree_root)?.len();
        if dry_run {
            output::run_msg(&format!(
                "would remove {}",
                output::count(found, "worktree")
            ));
        } else {
            let existing = found;
            let removed = worktree::purge_all(worktree_root)?;
            // Report found as well as removed. A bare "removed 0" reads as
            // "nothing to do", which is indistinguishable from "looked in the
            // wrong place" - and these worktrees carry an isolated target dir
            // each, so a cleanup that silently reclaims nothing is how the
            // cargo volume fills up. A worktree's target dir goes with it.
            if removed == existing {
                output::run_msg(&format!("removed {}", output::count(removed, "worktree")));
            } else {
                output::run_msg(&format!(
                    "removed {removed} of {} found",
                    output::count(existing, "worktree")
                ));
            }
        }
    } else if !interrupted && !dry_run {
        // A hint for someone running `brokkr clean` by hand; the interrupt
        // path's cleanup is not a place to advertise a deep clean.
        let existing =
            worktree::list(worktree_root)?.len() + worktree::list_legacy(worktree_root)?.len();
        if existing > 0 {
            output::run_msg(&format!(
                "{} persistent; run `brokkr clean --worktrees` to remove",
                output::count(existing, "worktree"),
            ));
        }
    }

    Ok(())
}

/// `brokkr clean --cargo [PKG]`: wipe the project's own build artifacts
/// (all profiles) while keeping dependency artifacts cached - the fix for
/// stale incremental-build state (e.g. phantom undefined-symbol linker
/// errors).
pub(crate) fn cargo_clean_package(build_root: &Path, pkg: &str) -> Result<(), DevError> {
    output::run_msg(&format!("cargo clean -p {pkg}"));
    let captured = output::run_cargo(&["clean", "-p", pkg], build_root, &[], None)?;
    captured.check_success("cargo clean")?;
    // cargo clean reports "Removed N files, X total" on stderr.
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if let Some(summary) = stderr.trim().lines().last()
        && !summary.is_empty()
    {
        output::run_msg(summary.trim());
    }
    Ok(())
}

/// Clean the exact scratch paths named by each project's typed layout.
fn clean_scratch(
    project: Project,
    paths: &config::ResolvedPaths,
    c: &Cleaner,
) -> Result<(), DevError> {
    if project == Project::Elivagar {
        let elivagar = layout::ElivagarLayout::new(paths);
        if c.dir(elivagar.scratch())? {
            if !c.dry_run {
                std::fs::create_dir_all(elivagar.scratch())?;
            }
            output::run_msg(&format!("{} scratch", c.past()));
        }
        let tmp_dir = elivagar.tilegen_tmp();
        if tmp_dir.as_path() != elivagar.scratch() && c.dir(&tmp_dir)? {
            output::run_msg(&format!("{} tilegen_tmp", c.past()));
        }
        return Ok(());
    }

    if project == Project::Nidhogg {
        // This is the ingest output path declared by NidhoggLayout. The
        // similarly named project-root tmp directories are not created by brokkr.
        let ingest_output = layout::NidhoggLayout::new(paths).bench_ingest_output();
        if c.dir(&ingest_output)? {
            output::run_msg(&format!("{} nidhogg ingest output", c.past()));
        }
    }

    if project == Project::Pbfhogg {
        let pbfhogg = layout::PbfhoggLayout::new(paths);
        let mut removed = 0usize;
        for file in pbfhogg.scratch_files() {
            removed += usize::from(c.file(&file)?);
        }
        for dir in pbfhogg.scratch_dirs() {
            removed += usize::from(c.dir(&dir)?);
        }
        if removed > 0 {
            output::run_msg(&format!(
                "{} {}",
                c.verb(),
                output::count(removed, "pbfhogg scratch output")
            ));
        }
    }

    // The reserved basename envelope and FIFO type identify a sidecar pipe;
    // clean does not decode a PID from its basename.
    let mut fifo_count = 0usize;
    for fifo in layout::SidecarFifos::new(&paths.scratch_dir).existing() {
        if c.file(&fifo)? {
            fifo_count += 1;
        }
    }
    if fifo_count > 0 {
        output::run_msg(&format!(
            "{} {}",
            c.verb(),
            output::count(fifo_count, "sidecar FIFO")
        ));
    }
    Ok(())
}

/// Remove child directories from a brokkr-designated artifact root while
/// preserving each permanent store and any directory containing one.
fn clean_artifact_children(
    root: &Path,
    permanent_files: &[std::path::PathBuf],
    c: &Cleaner,
) -> Result<usize, DevError> {
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(clean_io_error("cannot inspect", root, &error)),
    };
    if !metadata.file_type().is_dir() {
        return Ok(0);
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(clean_io_error("cannot read directory", root, &error)),
    };
    let mut removed = 0usize;
    for entry in entries {
        let entry =
            entry.map_err(|error| clean_io_error("cannot read directory entry in", root, &error))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| clean_io_error("cannot inspect", &path, &error))?;
        if !kind.is_dir() {
            continue;
        }
        if permanent_files.iter().any(|file| file.starts_with(&path)) {
            removed += clean_artifact_children(&path, permanent_files, c)?;
        } else if c.dir(&path)? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Remove one explicitly owned artifact tree, descending only if it contains
/// a named permanent store.
fn clean_artifact_root(
    root: &Path,
    permanent_files: &[std::path::PathBuf],
    c: &Cleaner,
) -> Result<usize, DevError> {
    if permanent_files.iter().any(|file| file.starts_with(root)) {
        clean_artifact_children(root, permanent_files, c)
    } else {
        Ok(usize::from(c.dir(root)?))
    }
}

/// Clean each project's artifact roots from its typed layout. Durable stores
/// are named separately and never inferred from the contents of a directory.
fn clean_artefact_trees(
    project: Project,
    state: &layout::ProjectState,
    c: &Cleaner,
) -> Result<(), DevError> {
    if project == Project::Ratatoskr {
        let ratatoskr = state.ratatoskr();
        let permanent = [ratatoskr.gate_db()];
        let mut removed = 0usize;
        for root in ratatoskr.artifact_roots() {
            removed += clean_artifact_root(&root, &permanent, c)?;
        }
        if removed > 0 {
            output::run_msg(&format!(
                "{} {} in ratatoskr artefacts",
                c.verb(),
                output::count(removed, "directory")
            ));
        }
    }

    if project == Project::Dellingr && c.dir(state.dellingr().scratch())? {
        output::run_msg(&format!("{} dellingr scratch dir", c.verb()));
    }

    if project == Project::Mogwai && c.dir(state.mogwai().scratch())? {
        output::run_msg(&format!("{} mogwai scratch dir", c.verb()));
    }

    if project == Project::Piners {
        let piners = state.piners();
        let corpus = piners.corpus_root();
        let removed = clean_artifact_children(&corpus, &[piners.corpus_runs_db()], c)?;
        if removed > 0 {
            output::run_msg(&format!(
                "{} {} in piners corpus",
                c.verb(),
                output::count(removed, "artefact directory")
            ));
        }
        // The lint root currently holds only its permanent runs.db (and
        // SQLite sidecars); lint runs create no per-run artifact directories.
        // Keeping that path outside the artifact sweep is deliberate.
    }
    Ok(())
}

/// Deep-clean (`--worktrees`) the durable tilegen output store: removes every
/// canonical `<dataset>-<variant>-<commit>.pmtiles` archive, i.e. `--archives`
/// with a keep window of zero. It used to remove every `*.pmtiles` in the
/// output dir, which broke the constructed-name rule `clean_archives` states:
/// with `output = "data"` it would have taken the hand-built ocean artifact
/// `ocean-tiles.pmtiles` too. Skipped when the output dir coincides with
/// scratch (already wiped by the caller).
fn clean_elivagar_outputs(paths: &config::ResolvedPaths, c: &Cleaner) -> Result<(), DevError> {
    let elivagar = layout::ElivagarLayout::new(paths);
    if elivagar.output() == elivagar.scratch() {
        return Ok(());
    }
    clean_archives(paths, 0, c)
}

/// `--archives`: prune canonical `<dataset>-<variant>-<commit>.pmtiles` archives
/// in the durable output store, keeping the newest `keep` per (dataset,
/// variant). Group membership and retention order come from the same
/// `ArchiveStore::prune()` used after tilegen admission. It constructs known
/// `(dataset, variant)` name shapes - filenames are never parsed back, since
/// dataset names carry hyphens. Anything outside the configured archive
/// namespace (hand-named files, the ocean artifact, pre-rename archives) stays.
fn clean_archives(paths: &config::ResolvedPaths, keep: usize, c: &Cleaner) -> Result<(), DevError> {
    let elivagar = layout::ElivagarLayout::new(paths);
    let output_dir = elivagar.output();
    let groups = elivagar.datasets().iter().flat_map(|(dataset, config)| {
        config.pbf.keys().map(|variant| {
            crate::elivagar::archive_store::ArchiveGroup::new(dataset.clone(), variant.clone())
        })
    });
    let store = crate::elivagar::archive_store::ArchiveStore::new(output_dir.to_path_buf())
        .with_groups(groups);
    let mut removal_error = None;
    let removed = store.prune(keep, |path| {
        if removal_error.is_some() {
            return false;
        }
        match c.file(path) {
            Ok(removed) => removed,
            Err(error) => {
                removal_error = Some(error);
                false
            }
        }
    })?;
    if let Some(error) = removal_error {
        return Err(DevError::Config(format!(
            "cannot prune archives in {}: {error}",
            output_dir.display()
        )));
    }
    if removed > 0 {
        output::run_msg(&format!(
            "{} {}",
            c.verb(),
            output::count(removed, "archive")
        ));
    }
    Ok(())
}
/// Remove the per-sweep isolated target dirs (`<target>/rustflags-<hash>`) that
/// a `rustflags` `[[check]]` entry mints (see `check_cmd::sweep_env`'s
/// `isolated_target_dir`). They are reproducible build scratch - a full rebuild
/// recreates them - and nothing else reclaims them: `cargo clean -p` only
/// touches the default target dir, and editing a sweep's `rustflags` orphans the
/// old hash's tree permanently. A routine `brokkr clean` sweeps them.
fn clean_rustflags_target_dirs(target_dir: &Path, c: &Cleaner) -> Result<(), DevError> {
    let mut removed = 0usize;
    for path in layout::RustflagsTargets::new(target_dir).existing_dirs() {
        if c.dir(&path)? {
            removed += 1;
        }
    }
    if removed > 0 {
        output::run_msg(&format!(
            "{} {}",
            c.verb(),
            output::count(removed, "isolated rustflags target dir")
        ));
    }
    Ok(())
}
