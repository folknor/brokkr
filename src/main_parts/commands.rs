
// ---------------------------------------------------------------------------
// Host feature resolution
// ---------------------------------------------------------------------------

/// Merge CLI `--features` with host-configured default features from brokkr.toml.
///
/// Host features are appended first, then CLI features. Duplicates are removed
/// (CLI wins, but since features are additive this just means dedup).
/// Resolve the ratatoskr harness profile override from the two CLI flags.
/// `Some(true)` = force dev, `Some(false)` = force release, `None` = defer
/// to `[ratatoskr.harness] debug` in `brokkr.toml`. The flags are
/// `conflicts_with` each other in clap, so they can't both be set.
fn profile_override(debug: bool, release: bool) -> Option<bool> {
    if debug {
        Some(true)
    } else if release {
        Some(false)
    } else {
        None
    }
}

/// Split each `brokkr clippy --env KEY=VALUE` pair (already validated to carry a
/// non-empty key and one `=` by `validate_env_kv`) into an owned `(key, value)`.
/// The `ok_or_else` is defence-in-depth against a caller that skipped the
/// validator.
fn parse_env_overrides(pairs: &[String]) -> Result<Vec<(String, String)>, DevError> {
    pairs
        .iter()
        .map(|p| {
            let (k, v) = p
                .split_once('=')
                .ok_or_else(|| DevError::Config(format!("--env expects KEY=VALUE, got '{p}'")))?;
            Ok((k.to_owned(), v.to_owned()))
        })
        .collect()
}

fn resolve_features(dev_config: &config::DevConfig, cli_features: &[String]) -> Vec<String> {
    let host_features = config::host_features(dev_config);
    if host_features.is_empty() {
        return cli_features.to_vec();
    }
    let mut merged = host_features;
    for f in cli_features {
        if !merged.iter().any(|existing| existing == f) {
            merged.push(f.clone());
        }
    }
    merged
}

fn resolve_mode(mode: &cli::ModeArgs) -> Result<measure::MeasureMode, DevError> {
    let set_count =
        mode.bench.is_some() as u8 + mode.hotpath.is_some() as u8 + mode.alloc.is_some() as u8;
    if set_count > 1 {
        return Err(DevError::Config(
            "--bench, --hotpath, and --alloc are mutually exclusive".into(),
        ));
    }
    let result = if let Some(runs) = mode.bench {
        measure::MeasureMode::Bench { runs }
    } else if let Some(runs) = mode.hotpath {
        measure::MeasureMode::Hotpath { runs }
    } else if let Some(runs) = mode.alloc {
        measure::MeasureMode::Alloc { runs }
    } else {
        measure::MeasureMode::Run
    };
    match &result {
        measure::MeasureMode::Bench { runs: 0 }
        | measure::MeasureMode::Hotpath { runs: 0 }
        | measure::MeasureMode::Alloc { runs: 0 } => {
            return Err(DevError::Config("run count must be >= 1".into()));
        }
        _ => {}
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Shared commands
// ---------------------------------------------------------------------------

/// `brokkr approve [ID...] [--all]` for both visual-testing projects.
///
/// Takes a list because approving one snapshot at a time was impractical:
/// the natural flow is run `visual`, eyeball the images, approve the ones
/// that look right, and commit the baselines once. Each ID is resolved and
/// approved in turn, and the first failure stops the batch - a bad ID should
/// not be silently skipped.
fn cmd_approve(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    fixture: Vec<String>,
    all: bool,
) -> Result<(), DevError> {
    if fixture.is_empty() && !all {
        return Err(DevError::Config(
            "specify one or more fixture/snapshot IDs, or --all".into(),
        ));
    }
    if all && !fixture.is_empty() {
        return Err(DevError::Config(
            "give fixture/snapshot IDs or --all, not both".into(),
        ));
    }

    match project {
        Project::Litehtml => {
            let cfg = dev_config
                .litehtml
                .as_ref()
                .ok_or_else(|| DevError::Config("no [litehtml] section in brokkr.toml".into()))?;
            let ids = if all {
                cfg.fixtures.iter().map(|f| f.id.clone()).collect()
            } else {
                fixture
            };
            for id in &ids {
                litehtml::cmd::approve(project, project_root, build_root, cfg, id)?;
            }
            Ok(())
        }
        Project::Sluggrs => {
            let cfg = dev_config
                .sluggrs
                .as_ref()
                .ok_or_else(|| DevError::Config("no [sluggrs] section in brokkr.toml".into()))?;
            let ids = if all {
                cfg.snapshots.iter().map(|s| s.id.clone()).collect()
            } else {
                fixture
            };
            for id in &ids {
                sluggrs::cmd::approve(project, project_root, build_root, cfg, id)?;
            }
            Ok(())
        }
        other => Err(DevError::Config(format!(
            "'approve' is only available for litehtml/sluggrs projects (current: {other})"
        ))),
    }
}

fn cmd_env(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
) -> Result<(), DevError> {
    // `bootstrap` shells out to `cargo metadata`, which rustup mediates - so in
    // a tree pinning a toolchain we don't have installed, `env` fails outright
    // unless the pin is moved aside. That window is the *locked* window (see
    // `crate::toolchain`), so reporting the environment has to take the lock
    // for the same reason `fmt` does, despite touching nothing. Only when
    // `disable_toolchain` is set: with nothing to move, `env` stays lock-free
    // and can still be run alongside a long build.
    let _lock = if dev_config.disable_toolchain {
        Some(acquire_cmd_lock(project, project_root, "env")?)
    } else {
        None
    };
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    let info = env::collect(&paths, project, project_root);
    env::print(&info);
    Ok(())
}

struct RunOptions {
    time: bool,
    json: bool,
    runs: usize,
    no_build: bool,
}

struct RunStats {
    min_ms: u64,
    median_ms: u64,
    p95_ms: u64,
}

fn duration_ms(duration: Duration) -> u64 {
    let ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    if ms == 0 && !duration.is_zero() {
        1
    } else {
        ms
    }
}

fn summarize_runs(samples_ms: &[u64]) -> Result<RunStats, DevError> {
    if samples_ms.is_empty() {
        return Err(DevError::Config("run requires at least one sample".into()));
    }

    let mut sorted = samples_ms.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let min_ms = sorted[0];
    let median_ms = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        let a = sorted[(n / 2) - 1];
        let b = sorted[n / 2];
        a.saturating_add(b) / 2
    };
    let p95_rank = (95 * n).div_ceil(100);
    let p95_index = p95_rank.saturating_sub(1);
    let p95_ms = sorted[p95_index];

    Ok(RunStats {
        min_ms,
        median_ms,
        p95_ms,
    })
}

fn print_run_timing(
    opts: &RunOptions,
    build_ms: u64,
    run_ms: u64,
    samples_ms: &[u64],
) -> Result<(), DevError> {
    let elapsed_ms = build_ms.saturating_add(run_ms);
    let stats = summarize_runs(samples_ms)?;

    if opts.json {
        if opts.runs == 1 {
            println!(
                "{}",
                serde_json::json!({
                    "build_ms": build_ms,
                    "run_ms": run_ms,
                    "elapsed_ms": elapsed_ms,
                })
            );
        } else {
            println!(
                "{}",
                serde_json::json!({
                    "build_ms": build_ms,
                    "run_ms": run_ms,
                    "elapsed_ms": elapsed_ms,
                    "runs": opts.runs,
                    "min_ms": stats.min_ms,
                    "median_ms": stats.median_ms,
                    "p95_ms": stats.p95_ms,
                    "run_samples_ms": samples_ms,
                })
            );
        }
        return Ok(());
    }

    if opts.time {
        if opts.runs == 1 {
            println!("elapsed_ms={elapsed_ms} build_ms={build_ms} run_ms={run_ms}");
        } else {
            println!(
                "elapsed_ms={elapsed_ms} build_ms={build_ms} run_ms={run_ms} runs={} min_ms={} median_ms={} p95_ms={}",
                opts.runs, stats.min_ms, stats.median_ms, stats.p95_ms,
            );
        }
        return Ok(());
    }

    if opts.runs > 1 {
        output::run_msg(&format!(
            "runs={} min={}ms median={}ms p95={}ms total={}ms",
            opts.runs, stats.min_ms, stats.median_ms, stats.p95_ms, run_ms,
        ));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    features: &[String],
    args: &[String],
    opts: &RunOptions,
    lock: Option<&lockfile::LockGuard>,
) -> Result<(), DevError> {
    if opts.runs == 0 {
        return Err(DevError::Config("--runs must be >= 1".into()));
    }

    let package = project.cli_package();
    let feature_refs: Vec<&str> = features.iter().map(String::as_str).collect();
    let build_config = if feature_refs.is_empty() {
        build::BuildConfig::release(package)
    } else {
        build::BuildConfig::release_with_features(package, &feature_refs)
    };
    let build_start = Instant::now();
    let binary = if opts.no_build {
        build::resolve_existing_binary(&build_config, build_root)?
    } else {
        build::cargo_build(&build_config, build_root)?
    };
    let build_ms = if opts.no_build {
        0
    } else {
        duration_ms(build_start.elapsed())
    };

    // Ensure scratch dir exists - binary commands often write output there.
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;
    std::fs::create_dir_all(&paths.scratch_dir)?;

    let mut run_total = Duration::ZERO;
    let mut samples_ms = Vec::with_capacity(opts.runs);

    for idx in 0..opts.runs {
        if opts.runs > 1 {
            output::run_msg(&format!("run {}/{}", idx + 1, opts.runs));
        }
        let binary_str = binary.display().to_string();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        output::run_msg(&format!("{binary_str} {}", args.join(" ")));
        let out = output::run_passthrough_timed(&binary_str, &arg_refs, lock)?;
        if out.code != 0 {
            return Err(DevError::ExitCode(out.code));
        }
        let run_elapsed = out.elapsed;
        run_total += run_elapsed;
        samples_ms.push(duration_ms(run_elapsed));
    }

    print_run_timing(opts, build_ms, duration_ms(run_total), &samples_ms)?;

    Ok(())
}

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
        if self.dry_run { "would remove" } else { "removed" }
    }

    /// Verb for "X" (named target) style messages.
    fn past(&self) -> &'static str {
        if self.dry_run { "would clean" } else { "cleaned" }
    }
}

fn cmd_clean(
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
        let found = worktree::list(worktree_root)?.len()
            + worktree::list_legacy(worktree_root)?.len();
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
                output::run_msg(&format!(
                    "removed {}",
                    output::count(removed, "worktree")
                ));
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
        let existing = worktree::list(worktree_root)?.len()
            + worktree::list_legacy(worktree_root)?.len();
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
fn cargo_clean_package(build_root: &Path, pkg: &str) -> Result<(), DevError> {
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
fn clean_scratch(project: Project, project_root: &Path, paths: &config::ResolvedPaths, c: &Cleaner) {
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
        output::run_msg(&format!("{} {}", c.verb(), output::count(removed, "archive")));
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

fn cmd_lock() -> Result<(), DevError> {
    let Some(info) = lockfile::status()? else {
        output::lock_msg("no active lock");
        return Ok(());
    };

    let invocation = if info.args.is_empty() {
        format!("{} {}", info.project, info.command)
    } else {
        format!("{} {}", info.project, info.args)
    };

    // First ask the holder itself, over its control socket. It answers about
    // its own process, so no PID crosses a namespace boundary - this works
    // from the host, from a sibling sandbox, from anywhere the socket is.
    if !info.acq_id.is_empty() {
        match crate::lock_service::request(&info.acq_id, "status") {
            crate::lock_service::Reply::Status(s) => {
                print_holder_status(&info, &invocation, &s);
                return Ok(());
            }
            crate::lock_service::Reply::Busy => {
                output::lock_msg(&invocation);
                output::lock_msg("the holder was mid-update; run `brokkr lock` again for details");
                output::lock_msg(&format!("root: {}", info.project_root));
                return Ok(());
            }
            // Released between the flock probe and the request, or stale
            // metadata: say what the file says, then try the local view.
            crate::lock_service::Reply::Gone
            | crate::lock_service::Reply::Accepted { .. }
            | crate::lock_service::Reply::NoAnswer(_) => {}
        }
    }
    if let Some(hint) = lockfile::same_agent_hint(&info) {
        output::lock_msg(&hint);
    }

    // The holder did not answer (an older brokkr, or one that is stopped).
    // Every PID in the lock file was written in the HOLDER's PID namespace
    // (a sandboxed holder writes pid=2, which is kthreadd here), so each
    // one is shown only when the namespaces match and its identity tokens
    // verify against this namespace's /proc - an unverified PID's stats
    // belong to some other process and are suppressed wholesale.
    if !lockfile::shares_namespaces(&info) {
        output::lock_msg(&invocation);
        output::lock_msg(&format!(
            "the holder did not answer its control socket, and it runs in {} - its process cannot \
             be inspected from here",
            if lockfile::foreign_holder_ns(&info).is_some() {
                "another PID namespace (a sandboxed command)"
            } else {
                "a PID or time namespace this process cannot verify"
            }
        ));
        output::lock_msg(&format!("root: {}", info.project_root));
        return Ok(());
    }
    let holder_verified = lockfile::verify_identity(info.pid, &info.starttime, &info.boot_id);
    let uptime_suffix = if holder_verified {
        lockfile::verified_uptime(info.pid, &info.starttime, &info.boot_id)
            .map(|u| format!(" running {u}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    if holder_verified {
        output::lock_msg(&format!(
            "brokkr PID {}{}: {}",
            info.pid, uptime_suffix, invocation,
        ));
    } else {
        output::lock_msg(&invocation);
        output::lock_msg(
            "process details unavailable - holder identity could not be verified from this namespace",
        );
    }
    output::lock_msg(&format!("root: {}", info.project_root));

    // Line 3: child process stats (if brokkr is currently running one).
    if let Some((child_pid, child_starttime)) = &info.child
        && let Some(summary) =
            lockfile::verified_summary(*child_pid, child_starttime, &info.boot_id)
    {
        let prefix = info
            .progress
            .map(|(r, t)| format!("run {r}/{t}, "))
            .unwrap_or_default();
        output::lock_msg(&format!("{prefix}child PID {child_pid} {summary}"));
    }

    // Lines 3b...: one per auxiliary mock-server (`service --all` keeps
    // one mock per distinct fixture alive for the whole cohort, so this
    // can be more than one line; sync / single-script service
    // emit at most one).
    for (mock_pid, mock_starttime) in &info.mocks {
        if let Some(summary) = lockfile::verified_summary(*mock_pid, mock_starttime, &info.boot_id)
        {
            output::lock_msg(&format!("mock PID {mock_pid} {summary}"));
        }
    }

    // Line 4: most recent sidecar marker, if any. The location and format
    // are owned by `sidecar::status_path` / `read_status`, which the writer
    // uses too - a reader-side path of its own drifted from the writer once.
    if info.pid > 0
        && let Some(marker) = crate::sidecar::read_status(info.pid)
    {
        output::lock_msg(&format!("last marker: {marker}"));
    }

    Ok(())
}

/// Render the holder's own answer. Everything here is the holder speaking
/// about itself; the one PID-keyed line (the sidecar marker) is printed only
/// when the PID means the same thing here as it did to the holder.
fn print_holder_status(
    info: &lockfile::LockInfo,
    invocation: &str,
    s: &crate::lock_service::HolderStatus,
) {
    let mut facts = Vec::new();
    if let Ok(secs) = s.get("held_secs").parse::<u64>() {
        facts.push(format!("holding {}", lockfile::format_duration(secs)));
    }
    if let Ok(secs) = s.get("cpu_secs").parse::<u64>() {
        facts.push(format!("{} CPU", lockfile::format_duration(secs)));
    }
    if let Ok(kb) = s.get("rss_kb").parse::<u64>() {
        facts.push(format!("RSS {} MB", kb / 1024));
    }
    if s.get("draining") == "1" {
        facts.push("draining earlier compilers".into());
    }
    let facts = if facts.is_empty() { String::new() } else { format!(" ({})", facts.join(", ")) };
    output::lock_msg(&format!("brokkr{facts}: {invocation}"));
    output::lock_msg(&format!("root: {}", info.project_root));

    let mut work = Vec::new();
    if !s.get("progress").is_empty() {
        work.push(format!("run {}", s.get("progress")));
    }
    if s.get("child") == "1" {
        work.push("a child process is running".into());
    }
    match s.get("mocks") {
        "" | "0" => {}
        n => work.push(match n.parse::<usize>() {
            Ok(k) => output::count(k, "mock server"),
            Err(_) => format!("{n} mock servers"),
        }),
    }
    if !work.is_empty() {
        output::lock_msg(&work.join(", "));
    }

    if lockfile::foreign_holder_ns(info).is_some() {
        output::lock_msg(
            "the holder runs in another PID namespace (a sandboxed command); `brokkr kill` asks it \
             to stop over its control socket, `brokkr kill --hard` cannot reach it from here",
        );
    }
    if let Some(hint) = lockfile::same_agent_hint(info) {
        output::lock_msg(&hint);
    }
    if lockfile::shares_namespaces(info)
        && info.pid > 0
        && let Some(marker) = crate::sidecar::read_status(info.pid)
    {
        output::lock_msg(&format!("last marker: {marker}"));
    }
}

/// `brokkr fmt`: forward `cargo fmt` with raw args, inheriting stdio.
///
/// Runs through the same passthrough runner as `brokkr run` (via
/// [`crate::runnables::run_cargo`]), not a bare `Command::status()`: the runner
/// installs the SIGTERM guard and publishes cargo's PID into the lock file, so
/// `brokkr kill` reaches cargo instead of orphaning it. `lock` is the hold
/// the `disable_toolchain` branch took, `None` when fmt runs lock-free.
///
/// A non-zero exit is cargo fmt's own verdict (`--check` reports a diff with
/// exit 1) and it has already printed it, so the code is propagated silently.
fn cmd_fmt(args: &[String], lock: Option<&lockfile::LockGuard>) -> Result<(), DevError> {
    let mut cargo_args: Vec<String> = vec!["fmt".into()];
    cargo_args.extend(args.iter().cloned());
    match crate::runnables::run_cargo(&cargo_args, lock)? {
        0 => Ok(()),
        code => Err(DevError::ExitCode(code)),
    }
}


/// `pmtiles-stats` is restricted to the two projects that produce or serve
/// PMTiles archives (elivagar, nidhogg). `project::require` gates a single
/// project; this command spans two, so we enforce the same restriction inline
/// with a message in `require`'s exact shape. Keeping the gate here - not only
/// in `visibility.rs`'s `TABLE` - is what stops the presentation layer from
/// becoming the de-facto gate: a hidden command still parses and reaches this
/// handler, which must produce the real error.
fn cmd_pmtiles_stats(project: Project, files: &[String]) -> Result<(), DevError> {
    if !matches!(project, Project::Elivagar | Project::Nidhogg) {
        return Err(DevError::Config(format!(
            "'brokkr pmtiles-stats' is only available in elivagar and nidhogg projects (current: {project})"
        )));
    }

    // Every file is attempted - one bad archive does not hide the stats of the
    // rest - but any failure fails the command, as `pmtiles::run` now reports
    // an unreadable file as an error rather than printing it as output.
    let mut failed = 0usize;
    for file in files {
        if let Err(e) = pmtiles::run(file) {
            output::error(&e.to_string());
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(DevError::Reported(format!(
            "{failed} of {} could not be read",
            output::count(files.len(), "PMTiles file")
        )));
    }
    Ok(())
}

/// Ask the brokkr process holding the lock to shut down. Default sends
/// SIGTERM (cooperative - brokkr handles cleanup itself). `--hard`
/// SIGSTOPs brokkr, then SIGKILLs the recorded child PID, every mock, every
/// other process beneath brokkr (each starttime-checked, see
/// `shutdown::kill_descendants`), and brokkr.
///
/// Every recorded PID was written in the HOLDER's PID namespace, which may
/// not be ours (a sandboxed holder writes pid=2 - kthreadd here), and any
/// PID can be recycled. So each signal target is authenticated first: its
/// recorded starttime token must match `/proc/{pid}/stat` in this namespace
/// under the same boot id, and a pidfd is opened and re-verified before
/// signalling (`pidfd_send_signal` then cannot be redirected by recycling).
/// Verification is all-or-nothing BEFORE the first signal: killing one
/// verified target and then discovering another is unverifiable would leave
/// a partially destroyed workload. Delivery itself is not transactional - a
/// target can exit naturally after preflight.
fn cmd_kill(hard: bool) -> Result<(), DevError> {
    let Some(info) = lockfile::status()? else {
        output::lock_msg("no active lock");
        return Ok(());
    };
    if info.pid == 0 {
        output::lock_msg("lock held by unknown process; nothing to kill");
        return Ok(());
    }

    if !hard {
        return kill_graceful(&info);
    }

    // Hard kill signals by number, so every recorded PID must mean the same
    // thing here as it did to the holder.
    if !lockfile::shares_namespaces(&info) {
        return Err(refuse_kill(
            "the holder runs in another PID or time namespace (a sandboxed command); `brokkr kill` \
             without --hard asks it to stop over its control socket, or stop the session that owns \
             it",
        ));
    }
    hard_kill(&info)
}

/// The cooperative stop. The control socket first: the holder validates the
/// request against its live acquisition and stops itself, so this works
/// across PID namespaces and cannot reach a later holder.
fn kill_graceful(info: &lockfile::LockInfo) -> Result<(), DevError> {
    use crate::lock_service::Reply;
    let unanswered = if info.acq_id.is_empty() {
        "the holder records no control socket (an older brokkr)".to_owned()
    } else {
        match crate::lock_service::request(&info.acq_id, "stop") {
            Reply::Accepted { signal_error: None } => {
                output::lock_msg("stop accepted by the holder - cleanup in progress");
                return Ok(());
            }
            Reply::Accepted { signal_error: Some(e) } => {
                output::lock_msg(&format!(
                    "stop committed by the holder, but its SIGTERM to itself failed ({e}); it \
                     stops at its next check"
                ));
                return Ok(());
            }
            Reply::Gone => {
                output::lock_msg("the holder had already released the lock; nothing to kill");
                return Ok(());
            }
            Reply::Busy | Reply::Status(_) => "the holder gave an unexpected answer".to_owned(),
            Reply::NoAnswer(why) => why,
        }
    };
    // No answer (an older brokkr, or a stopped holder): the numeric path,
    // only where the recorded PID means the same thing here.
    if !lockfile::shares_namespaces(info) {
        return Err(refuse_kill(&format!(
            "{unanswered}, and the holder runs in a PID or time namespace this process cannot \
             verify"
        )));
    }
    let holder = preflight_holder(info).map_err(|why| refuse_kill(&why))?;
    match signal_target(&holder, libc::SIGTERM) {
        Ok(true) => output::lock_msg(&format!(
            "SIGTERM sent to brokkr PID {} - cleanup in progress",
            info.pid,
        )),
        Ok(false) => output::lock_msg(&format!(
            "brokkr PID {} exited before the signal landed; nothing to kill",
            info.pid,
        )),
        Err(e) => {
            return Err(DevError::Lock(format!("SIGTERM to brokkr PID {} failed: {e}", info.pid)));
        }
    }
    Ok(())
}

/// SIGSTOP the holder, SIGKILL its recorded children, everything beneath it,
/// then the holder. Callers have checked `shares_namespaces`.
fn hard_kill(info: &lockfile::LockInfo) -> Result<(), DevError> {
    // Preflight every advertised target: verify identity, open and retain a
    // pidfd (pinning the process generation for the whole operation, PG
    // leaders included), re-verify. Any failure refuses the whole hard kill
    // with no signal sent.
    let mut targets = Vec::new();
    if let Some((child_pid, child_starttime)) = &info.child {
        targets.push(
            preflight_target("child", *child_pid, child_starttime, &info.boot_id)
                .map_err(|why| refuse_kill(&why))?,
        );
    }
    for (mock_pid, mock_starttime) in &info.mocks {
        targets.push(
            preflight_target("mock", *mock_pid, mock_starttime, &info.boot_id)
                .map_err(|why| refuse_kill(&why))?,
        );
    }
    let holder = preflight_holder(info).map_err(|why| refuse_kill(&why))?;

    // Freeze brokkr first, so it cannot start anything new (the next sweep,
    // a respawned mock) while its tree is being taken down.
    if let Err(e) = signal_target(&holder, libc::SIGSTOP) {
        output::lock_msg(&format!("SIGSTOP brokkr PID {}: failed ({e})", info.pid));
    }

    // Kill children first, then brokkr - otherwise there's a brief window
    // where brokkr is dead but the tool it was measuring is still alive
    // (and anyone peeking at `brokkr lock` sees stale state pointing at a
    // live child with no owner).
    for target in &targets {
        output::lock_msg(&format!(
            "SIGKILL {} {} {}: {}",
            target.role,
            if target.pg_leader { "PG" } else { "PID" },
            target.pid,
            describe_delivery(&signal_target(target, libc::SIGKILL)),
        ));
    }
    // Then everything else beneath brokkr. The recorded child is only the
    // process brokkr published: a test binary under cargo, a rustc under a
    // build, or a parallel lane's binaries are recorded nowhere, and a parent-
    // death signal reaches only a direct child - so without this walk a hard
    // kill orphaned them. Two sweeps for a child forked mid-walk; with brokkr
    // stopped, nothing restarts them. The holder's identity was verified from
    // this namespace above, so its pid is meaningful to our `/proc`.
    let swept = crate::shutdown::kill_descendants(info.pid)
        + crate::shutdown::kill_descendants(info.pid);
    if swept > 0 {
        output::lock_msg(&format!(
            "SIGKILL sent to {} beneath brokkr",
            output::count(swept, "process")
        ));
    }
    output::lock_msg(&format!(
        "SIGKILL brokkr PID {}: {}",
        info.pid,
        describe_delivery(&signal_target(&holder, libc::SIGKILL)),
    ));
    output::lock_msg("follow up with `brokkr clean` to wipe scratch");
    Ok(())
}

/// The all-or-nothing refusal: name the unverifiable target and state that
/// nothing was signalled.
fn refuse_kill(why: &str) -> DevError {
    DevError::Lock(format!(
        "{why} - lock holder and child process identities must all verify from this PID/time namespace before killing; no signals were sent"
    ))
}

/// A signal target that passed preflight: identity verified, pidfd held
/// (pinning the process generation until we signal), PG-leadership known.
struct KillTarget {
    role: &'static str,
    pid: u32,
    starttime: String,
    pidfd: std::os::unix::io::OwnedFd,
    /// `getpgid(pid) == pid` for a tracked child: spawns that called
    /// `process_group(0)` become their own PG leader, and `kill(-pid, ...)`
    /// sweeps the whole group (rustc, sæhrimnir's helpers, etc.).
    /// Non-leaders inherit brokkr's PG, where a group kill would target
    /// brokkr's own PG (catastrophic), so they get a single-process pidfd
    /// signal. Always false for the brokkr holder itself: brokkr *is*
    /// normally its own PG leader (`shutdown::isolate_process_group`), and a
    /// group signal at it would sweep its children too - defeating the
    /// cooperative-SIGTERM path, whose whole point is that brokkr's handler
    /// shuts the workload down itself.
    pg_leader: bool,
}

/// Preflight the brokkr holder. Same authentication as any target, but the
/// holder is always signalled through its pidfd alone, never as a process
/// group (see `pg_leader`).
fn preflight_holder(info: &lockfile::LockInfo) -> Result<KillTarget, String> {
    let mut holder = preflight_target("brokkr", info.pid, &info.starttime, &info.boot_id)?;
    holder.pg_leader = false;
    Ok(holder)
}

/// Authenticate one recorded PID and open a pidfd on it (see
/// [`lockfile::open_verified_pidfd`] for the verify -> open -> re-verify order).
fn preflight_target(
    role: &'static str,
    pid: u32,
    starttime: &str,
    boot_id: &str,
) -> Result<KillTarget, String> {
    let pidfd = lockfile::open_verified_pidfd(pid, starttime, boot_id).map_err(|r| match r {
        lockfile::PidfdRefusal::Unverified => {
            format!("{role} PID {pid}: identity could not be verified from this namespace")
        }
        lockfile::PidfdRefusal::OpenFailed => format!(
            "{role} PID {pid}: could not open a pidfd (process gone or namespace-isolated)"
        ),
        lockfile::PidfdRefusal::IdentityChanged => {
            format!("{role} PID {pid}: process changed identity during verification")
        }
    })?;
    let pgid = unsafe { libc::getpgid(pid.cast_signed()) };
    // Cast guarded by `pgid > 0`; `cast_unsigned` is the documented
    // i32->u32 conversion that clippy doesn't flag (mirrors the
    // pid.cast_signed() pattern we use for the inverse direction).
    let pg_leader = pgid > 0 && pgid.cast_unsigned() == pid;
    Ok(KillTarget {
        role,
        pid,
        starttime: starttime.to_owned(),
        pidfd,
        pg_leader,
    })
}

/// Signal a preflighted target. `Ok(true)` delivered, `Ok(false)` the target
/// was already gone, `Err` a real failure (EPERM, say) - reported, never
/// counted as delivered.
///
/// Non-leaders are signalled through the pidfd, which is race-free against
/// PID recycling. PG leaders need `kill(-pid, ...)` to sweep the group and
/// process-group IDs have no pidfd-equivalent handle, so the leader is
/// re-verified immediately before the group kill (starttime unchanged, i.e.
/// same generation); the sliver between that check and the kill is a
/// documented residual race we accept.
fn signal_target(target: &KillTarget, signal: libc::c_int) -> std::io::Result<bool> {
    if target.pg_leader {
        if lockfile::proc_starttime(target.pid).as_deref() != Some(target.starttime.as_str()) {
            return Ok(false);
        }
        let ret = unsafe { libc::kill(-target.pid.cast_signed(), signal) };
        if ret == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(err);
    }
    lockfile::pidfd_send_signal(&target.pidfd, signal)
}

fn describe_delivery(result: &std::io::Result<bool>) -> String {
    match result {
        Ok(true) => "sent".into(),
        Ok(false) => "not running".into(),
        Err(e) => format!("FAILED ({e})"),
    }
}

// ---------------------------------------------------------------------------
// Verify dispatch
// ---------------------------------------------------------------------------

fn cmd_verify(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: Option<&Path>,
    verify: VerifyCommand,
    features: &[String],
    verbose: bool,
) -> Result<(), DevError> {
    match verify {
        // ----- elivagar verify variants -----
        VerifyCommand::ElivVerify {
            dataset,
            variant,
            commit,
            file,
            geometry_stats,
            unique_payloads,
        } => {
            project::require(project, Project::Elivagar, "verify")?;
            // Never reached under a historical worktree: `run()` dispatches
            // this variant before `with_worktree`, so `build_root` is the
            // main tree and the archive resolver reads its HEAD.
            elivagar::cmd::verify(
                dev_config,
                project,
                project_root,
                build_root.unwrap_or(project_root),
                &dataset,
                &variant,
                commit.as_deref(),
                file.as_deref(),
                features,
                geometry_stats,
                unique_payloads,
            )
        }

        // ----- nidhogg verify variants -----
        // `batch` and `geocode` only query an already-running server - no
        // build, no files touched - so they take no lock. `readonly` builds
        // the server, stops and restarts it, and chmods the index, so it does
        // (and `cargo_build` refuses to run without it).
        VerifyCommand::Batch { dataset } => {
            project::require(project, Project::Nidhogg, "verify batch")?;
            nidhogg::cmd::verify_batch(dev_config, project, project_root, &dataset)
        }
        VerifyCommand::NidGeocode { queries } => {
            project::require(project, Project::Nidhogg, "verify geocode")?;
            nidhogg::cmd::verify_geocode(dev_config, project, project_root, &queries)
        }
        VerifyCommand::Readonly { dataset } => {
            project::require(project, Project::Nidhogg, "verify readonly")?;
            // Re-enters the hold when a `--commit` worktree already took it.
            let _lock = acquire_cmd_lock(project, project_root, "verify readonly")?;
            nidhogg::cmd::verify_readonly(dev_config, project, project_root, build_root, &dataset, features)
        }
        // ----- pbfhogg verify variants -----
        _ => {
            project::require(project, Project::Pbfhogg, "verify")?;
            pbfhogg::cmd::verify(
                dev_config,
                project,
                project_root,
                build_root,
                verify,
                features,
                verbose,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Generic hotpath
// ---------------------------------------------------------------------------

/// Generic hotpath for projects without dedicated modules.
///
/// Builds the binary with `--features hotpath` (or `hotpath-alloc`), runs it
/// with no extra arguments, and collects the JSON hotpath report via the
/// standard env-var mechanism.
fn cmd_hotpath_generic(req: &measure::MeasureRequest) -> Result<(), DevError> {
    let hotpath_features = req.hotpath_features();
    let ctx = context::BenchContext::new(
        req.dev_config,
        req.project,
        req.project_root,
        req.build_root,
        req.project.cli_package(),
        &hotpath_features,
        true,
        "hotpath",
        req.force,        req.stop_marker.map(str::to_owned),
    )?
    .with_request(req);

    let alloc = req.is_alloc();
    let label = harness::hotpath_feature(alloc);
    output::detail(&format!("{} {label}", req.project));
    harness::hotpath_alloc_note(alloc);

    let binary_str = ctx.binary.display().to_string();

    let config = harness::BenchConfig {
        command: "default".into(),
        mode: None,
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: crate::build::CargoProfile::Release,
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &[])),
        brokkr_args: None,
        metadata: vec![],
    };

    ctx.harness.run_hotpath(&config, &ctx.binary, |_i| {
        let (result, _stderr, sidecar) = harness::run_hotpath_capture(
            &binary_str,
            &[],
            &ctx.paths.scratch_dir,
            req.project_root,
            &[],
            &[],
            req.stop_marker,
            Some(ctx.harness.lock()),
        )?;
        Ok((result, sidecar))
    })?;

    Ok(())
}

#[cfg(test)]
mod pmtiles_stats_gate_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // The gate must agree with `visibility.rs`'s TABLE entry, which scopes
    // pmtiles-stats to elivagar + nidhogg. Wrong projects get the real
    // `require`-shaped error; allowed projects fall through (empty file list
    // makes the call a pure no-op, so no disk I/O).

    #[test]
    fn rejects_projects_without_pmtiles() {
        for project in [
            Project::Pbfhogg,
            Project::Litehtml,
            Project::Piners,
            Project::Other("mystery"),
        ] {
            let err = cmd_pmtiles_stats(project, &[]).unwrap_err();
            let DevError::Config(msg) = err else {
                panic!("expected DevError::Config, got {err:?}");
            };
            assert!(msg.contains("pmtiles-stats"), "message: {msg}");
            assert!(msg.contains("elivagar"), "message: {msg}");
            assert!(msg.contains("nidhogg"), "message: {msg}");
        }
    }

    #[test]
    fn allows_elivagar_and_nidhogg() {
        // Empty file list => the loop body never runs, so an allowed project
        // returns Ok without opening any archive.
        assert!(cmd_pmtiles_stats(Project::Elivagar, &[]).is_ok());
        assert!(cmd_pmtiles_stats(Project::Nidhogg, &[]).is_ok());
    }
}
