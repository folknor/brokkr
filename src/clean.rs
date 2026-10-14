//! Presentation and cleanup policy for `brokkr clean`.

use std::path::Path;

use crate::config;
use crate::context::{bootstrap, bootstrap_config};
use crate::error::DevError;
use crate::litehtml;
use crate::lockfile;
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
/// removes nothing; otherwise it deletes best-effort. Returns whether the path
/// existed (i.e. something was, or would be, removed). Counts those hits so
/// the caller can tell an empty sweep from a productive one.
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

    fn dir(&self, path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        if !self.dry_run {
            std::fs::remove_dir_all(path).ok();
        }
        self.hits.set(self.hits.get() + 1);
        true
    }

    fn file(&self, path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        if !self.dry_run {
            std::fs::remove_file(path).ok();
        }
        self.hits.set(self.hits.get() + 1);
        true
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

pub(crate) fn cmd_clean(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
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
    let worktree_root = build_root;
    let pi = bootstrap(Some(build_root))?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    // Clean verify output (pbfhogg only).
    let verify_dir = paths.target_dir.join("verify");
    if c.dir(&verify_dir) {
        output::run_msg(&format!("{} verify output", c.verb()));
    }

    // Reclaim the per-sweep isolated target dirs a `rustflags` `[[check]]`
    // entry mints (`<target>/rustflags-<hash>`). Under `pi.target_dir` -
    // cargo's real resolved target dir, where the check/test phase actually
    // built them - not `paths.target_dir` (which a host `target` override can
    // repoint). Routine scratch: reproducible, and nothing else reaches them.
    clean_rustflags_target_dirs(&pi.target_dir, &c);

    clean_scratch(project, project_root, &paths, &c);

    // Elivagar: routine clean also clears the corpus-calibrand scratch dir (the
    // default `-o` target for `pmtiles-corpus mutate`) and ocean-build's tmp
    // dir. Both are brokkr-designated locations holding reproducible scratch;
    // an explicit `-o` elsewhere is the user's file and is never touched.
    if project == Project::Elivagar {
        if c.dir(&paths.data_dir.join(CORPUS_CALIBRAND_DIR)) {
            output::run_msg(&format!("{} corpus-calibrands", c.past()));
        }
        if c.dir(&paths.data_dir.join("ocean-build_tmp")) {
            output::run_msg(&format!("{} ocean-build_tmp", c.past()));
        }
        if archives {
            clean_archives(&paths, keep, &c);
        }
    }

    clean_artefact_trees(project, project_root, &c);

    // Litehtml: the per-user Node script cache keeps one directory per bundled
    // script version, each with its own node_modules. Every version other than
    // the one this binary carries is unreachable and goes; the current one
    // stays so the next run does not reinstall.
    if project == Project::Litehtml {
        let stale = litehtml::cmd::stale_script_cache_dirs();
        for dir in &stale {
            c.dir(dir);
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
            clean_elivagar_outputs(&paths, &c);
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
    let captured = output::run_captured("cargo", &["clean", "-p", pkg], build_root)?;
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

/// Clean the project's scratch/tmp directory. Each project's scratch is a
/// different shape: elivagar wipes its scratch dir (recreated) and
/// `<data>/tilegen_tmp`, nidhogg has two named tmp dirs, and pbfhogg/others
/// sweep loose `.pbf` scratch, geocode output dirs, and dead external-join dirs.
fn clean_scratch(
    project: Project,
    project_root: &Path,
    paths: &config::ResolvedPaths,
    c: &Cleaner,
) {
    if project == Project::Elivagar {
        // Two distinct brokkr-designated dirs, not one. The scratch dir (default
        // `data/scratch`) holds tilegen's `-o` target before it is renamed into
        // the durable store; `<data>/tilegen_tmp` is the `--tmp-dir` brokkr
        // passes (`ElivagarCommand::build_args`), where elivagar spills its
        // intermediates. Cleaning only the scratch dir under the tilegen_tmp
        // label left the real tmp dir - the large one - untouched on every
        // host that does not happen to point `scratch` at it.
        if c.dir(&paths.scratch_dir) {
            if !c.dry_run {
                std::fs::create_dir_all(&paths.scratch_dir).ok();
            }
            output::run_msg(&format!("{} scratch", c.past()));
        }
        let tmp_dir = paths.data_dir.join("tilegen_tmp");
        if tmp_dir != paths.scratch_dir && c.dir(&tmp_dir) {
            output::run_msg(&format!("{} tilegen_tmp", c.past()));
        }
        return;
    }
    if project == Project::Nidhogg {
        // Nidhogg's tmp dirs live under the project root, not the scratch dir,
        // so their cleanup must not be gated on the scratch dir existing.
        if c.dir(&project_root.join(".ingest_tmp")) {
            output::run_msg(&format!("{} .ingest_tmp", c.past()));
        }
        if c.dir(&project_root.join(".tilegen_tmp")) {
            output::run_msg(&format!("{} .tilegen_tmp", c.past()));
        }
        return;
    }
    if !paths.scratch_dir.exists() {
        return;
    }

    let mut removed = 0usize;
    if let Ok(entries) = std::fs::read_dir(&paths.scratch_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("pbf") {
                c.file(&path);
                removed += 1;
            }
            // Clean geocode output directories (geocode-<dataset>/).
            if path.is_dir()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                if name.starts_with("geocode-") {
                    c.dir(&path);
                    removed += 1;
                }
                // Clean orphaned external-join scratch dirs
                // (.pbfhogg-external-join-{pid}); these survive OOM kills
                // (SIGKILL prevents Drop cleanup).
                if let Some(pid_str) = name.strip_prefix(".pbfhogg-external-join-")
                    && let Ok(pid) = pid_str.parse::<u32>()
                    && !join_dir_owner_alive(pid, &path)
                {
                    c.dir(&path);
                    removed += 1;
                }
            }
        }
    }
    if removed > 0 {
        output::run_msg(&format!(
            "{} {}",
            c.verb(),
            output::count(removed, "scratch file")
        ));
    }
}

/// Whether the process that created `.pbfhogg-external-join-{pid}` at `dir` may
/// still be running. The dir name carries only a PID - no starttime token - so
/// the owner's identity is reconstructed by ordering: a process that started
/// *after* the directory was created cannot have created it, so a live PID with
/// a later start is a recycled PID and the dir is orphaned.
///
/// Errs toward "alive" whenever it cannot tell: a kept dir leaks disk until the
/// next clean, a deleted live dir breaks a running join. `EPERM` from
/// `kill(pid, 0)` means the process exists under another uid - alive, not dead.
fn join_dir_owner_alive(pid: u32, dir: &Path) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return true;
    };
    if raw <= 0 {
        return true;
    }
    let exists = if unsafe { libc::kill(raw, 0) } == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    };
    // The dir's birth time where the filesystem records one, else its mtime.
    // mtime is never earlier than birth, so "started after mtime" still
    // implies "started after the dir existed" - the fallback stays sound.
    let dir_epoch = std::fs::metadata(dir)
        .ok()
        .and_then(|m| m.created().or_else(|_| m.modified()).ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64());
    let proc_start = if exists { proc_start_epoch(pid) } else { None };
    join_owner_alive_decision(exists, proc_start, dir_epoch)
}

/// Slack for [`join_owner_alive_decision`]'s ordering test: `btime` is whole
/// seconds and starttime is in clock ticks, so a process start is known to
/// within about a second. Generous on purpose - the slack only ever widens the
/// "alive" side.
const JOIN_OWNER_START_SLACK_SECS: f64 = 5.0;

/// Pure decision behind [`join_dir_owner_alive`]. `exists` is the `kill(pid, 0)`
/// verdict (EPERM counted as existing); the two epochs are the live PID's
/// start and the dir's creation, each `None` when unreadable.
fn join_owner_alive_decision(
    exists: bool,
    proc_start: Option<f64>,
    dir_created: Option<f64>,
) -> bool {
    if !exists {
        return false;
    }
    match (proc_start, dir_created) {
        (Some(start), Some(created)) => start <= created + JOIN_OWNER_START_SLACK_SECS,
        _ => true,
    }
}

/// A PID's start as seconds since the Unix epoch: `/proc/stat`'s `btime` plus
/// the PID's starttime ticks. `None` on any read or parse failure.
fn proc_start_epoch(pid: u32) -> Option<f64> {
    let ticks: f64 = lockfile::proc_starttime(pid)?.parse().ok()?;
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    if clk_tck <= 0.0 {
        return None;
    }
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let btime: f64 = stat
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(btime + ticks / clk_tck)
}

/// Clean the ratatoskr and piners run-artefact trees. Both are debris by the
/// time `clean` runs (we hold the project lock). For piners only the `run-N/`
/// dirs go - the corpus run store (`runs.db` + wal/shm) is the source of truth
/// and must survive. Ratatoskr is the same shape: the artefact and `mock/`
/// *directories* go, but every file directly under `.brokkr/ratatoskr` stays,
/// because that is where `gate.db` lives. A gate baseline is pinned by UUID in
/// `brokkr.toml`; deleting the row it points at breaks the gate with no way
/// back short of re-recording, which silently rebases the reference onto
/// current numbers. Measurement stores are permanently out of clean's scope.
fn clean_artefact_trees(project: Project, project_root: &Path, c: &Cleaner) {
    if project == Project::Ratatoskr {
        let ratatoskr_root = project_root.join(".brokkr/ratatoskr");
        let runs = count_run_dirs(&ratatoskr_root);
        let mut removed = 0;
        if let Ok(entries) = std::fs::read_dir(&ratatoskr_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    c.dir(&path);
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            output::run_msg(&format!(
                "{} {} in ratatoskr artefacts",
                c.verb(),
                output::count(runs, "run dir")
            ));
        }
    }

    if project == Project::Dellingr {
        // Only brokkr's own leavings live here (the harness hotpath report and
        // marker FIFO); results.db and sidecar.db are one level up in
        // `.brokkr/` and stay out of scope as everywhere else.
        let dellingr_root = project_root.join(".brokkr/dellingr");
        if dellingr_root.exists() {
            c.dir(&dellingr_root);
            output::run_msg(&format!("{} dellingr scratch dir", c.verb()));
        }
    }

    if project == Project::Piners {
        let corpus_root = project_root.join(".brokkr/piners/corpus");
        let mut removed = 0usize;
        if let Ok(entries) = std::fs::read_dir(&corpus_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && entry.file_name().to_string_lossy().starts_with("run-") {
                    c.dir(&path);
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            output::run_msg(&format!(
                "{} {} in piners corpus",
                c.verb(),
                output::count(removed, "run dir")
            ));
        }
    }
}

/// The default `-o` directory for `pmtiles-corpus mutate` calibrands, under the
/// data dir. A routine `brokkr clean` clears it wholesale; an explicit `-o`
/// elsewhere is the user's file and is never touched.
pub(crate) const CORPUS_CALIBRAND_DIR: &str = "corpus-calibrands";

/// Deep-clean (`--worktrees`) the durable tilegen output store: removes every
/// canonical `<dataset>-<variant>-<commit>.pmtiles` archive, i.e. `--archives`
/// with a keep window of zero. It used to remove every `*.pmtiles` in the
/// output dir, which broke the constructed-name rule `clean_archives` states:
/// with `output = "data"` it would have taken the hand-built ocean artifact
/// `ocean-tiles.pmtiles` too. Skipped when the output dir coincides with
/// scratch (already wiped by the caller).
fn clean_elivagar_outputs(paths: &config::ResolvedPaths, c: &Cleaner) {
    if paths.output_dir == paths.scratch_dir {
        return;
    }
    clean_archives(paths, 0, c);
}

/// `--archives`: prune canonical `<dataset>-<variant>-<commit>.pmtiles` archives
/// in the durable output store, keeping the newest `keep` per (dataset,
/// variant). Group membership is decided by CONSTRUCTING each known (dataset,
/// variant) name shape (`resolve::pmtiles_archive_matches`) - filenames are
/// never parsed back, since dataset names carry hyphens. The safety property
/// follows: anything the constructed matcher rejects (hand-named files, the
/// toml-contract ocean artifact, pre-rename `<dataset>-<commit>` archives) is
/// preserved unconditionally, because a file brokkr can't name by construction
/// is self-evidently not brokkr's to delete.
fn clean_archives(paths: &config::ResolvedPaths, keep: usize, c: &Cleaner) {
    let output_dir = &paths.output_dir;
    if !output_dir.exists() {
        return;
    }
    // Read the directory once, for every group. Reading it per group made a
    // transient failure abort the whole pass silently, halfway through.
    let entries = match std::fs::read_dir(output_dir) {
        Ok(entries) => entries,
        Err(e) => {
            output::error(&format!(
                "cannot read output dir {}: {e} - archives not pruned",
                output_dir.display()
            ));
            return;
        }
    };
    let listing: Vec<(String, std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_owned();
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((name, mtime, path))
        })
        .collect();

    let mut removed = 0usize;
    for (dataset, ds) in &paths.datasets {
        for variant in ds.pbf.keys() {
            let mut archives: Vec<(std::time::SystemTime, &std::path::Path)> = listing
                .iter()
                .filter(|(name, _, _)| {
                    crate::resolve::pmtiles_archive_matches(name, dataset, variant)
                })
                .map(|(_, mtime, path)| (*mtime, path.as_path()))
                .collect();
            if archives.len() <= keep {
                continue;
            }
            // Newest first; prune everything past the keep window.
            archives.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
            for (_, path) in archives.into_iter().skip(keep) {
                c.file(path);
                removed += 1;
            }
        }
    }
    if removed > 0 {
        output::run_msg(&format!(
            "{} {}",
            c.verb(),
            output::count(removed, "archive")
        ));
    }
}

/// Remove the per-sweep isolated target dirs (`<target>/rustflags-<hash>`) that
/// a `rustflags` `[[check]]` entry mints (see `check_cmd::output`'s
/// `isolated_target_dir`). They are reproducible build scratch - a full rebuild
/// recreates them - and nothing else reclaims them: `cargo clean -p` only
/// touches the default target dir, and editing a sweep's `rustflags` orphans the
/// old hash's tree permanently. A routine `brokkr clean` sweeps them (S3-06).
fn clean_rustflags_target_dirs(target_dir: &Path, c: &Cleaner) {
    let mut removed = 0usize;
    if let Ok(entries) = std::fs::read_dir(target_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("rustflags-")
            {
                c.dir(&path);
                removed += 1;
            }
        }
    }
    if removed > 0 {
        output::run_msg(&format!(
            "{} {}",
            c.verb(),
            output::count(removed, "isolated rustflags target dir")
        ));
    }
}

fn count_run_dirs(root: &Path) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let is_run = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("run-"))
                .is_some_and(|rest| rest.parse::<u32>().is_ok());
            if is_run {
                count += 1;
            } else {
                stack.push(path);
            }
        }
    }
    count
}

#[cfg(test)]
mod join_owner_tests {
    use super::join_owner_alive_decision;

    #[test]
    fn a_missing_pid_is_dead() {
        assert!(!join_owner_alive_decision(false, None, None));
    }

    #[test]
    fn an_existing_pid_with_unknown_times_is_alive() {
        assert!(join_owner_alive_decision(true, None, Some(100.0)));
        assert!(join_owner_alive_decision(true, Some(100.0), None));
    }

    #[test]
    fn a_pid_started_before_the_dir_is_alive() {
        assert!(join_owner_alive_decision(true, Some(100.0), Some(200.0)));
        // Within the tick/btime slack.
        assert!(join_owner_alive_decision(true, Some(203.0), Some(200.0)));
    }

    #[test]
    fn a_pid_started_after_the_dir_is_recycled() {
        assert!(!join_owner_alive_decision(true, Some(1000.0), Some(200.0)));
    }
}
