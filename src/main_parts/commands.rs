
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

fn cmd_env(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
) -> Result<(), DevError> {
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
    pub(crate) worktrees: bool,
    pub(crate) archives: bool,
    pub(crate) keep: usize,
    pub(crate) dry_run: bool,
}

impl CleanOpts {
    /// The routine sweep: scratch/tmp only, nothing durable. Used by the
    /// interrupt/kill cleanup path.
    pub(crate) fn routine() -> Self {
        Self {
            worktrees: false,
            archives: false,
            keep: 2,
            dry_run: false,
        }
    }
}

/// Removal helper that honours `--dry-run`: in dry-run it reports the path and
/// removes nothing; otherwise it deletes best-effort. Returns whether the path
/// existed (i.e. something was, or would be, removed).
struct Cleaner {
    dry_run: bool,
}

impl Cleaner {
    fn dir(&self, path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        if !self.dry_run {
            std::fs::remove_dir_all(path).ok();
        }
        true
    }

    fn file(&self, path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        if !self.dry_run {
            std::fs::remove_file(path).ok();
        }
        true
    }

    /// Verb for "N file(s)" style messages.
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
    opts: CleanOpts,
) -> Result<(), DevError> {
    let CleanOpts {
        worktrees,
        archives,
        keep,
        dry_run,
    } = opts;
    let c = Cleaner { dry_run };
    let pi = bootstrap(None)?;
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

    if worktrees {
        // Deep clean: `--worktrees` reclaims the expensive *persistent* state.
        // The durable tilegen output store is elivagar's analog of piners'
        // `runs.db` (the archives `regress` diffs), so a routine `brokkr clean`
        // spares it - retention already bounds its growth. Only the explicit
        // deep clean wipes it wholesale, alongside the worktrees.
        if project == Project::Elivagar {
            clean_elivagar_outputs(&paths, &c);
        }
        if dry_run {
            let existing = worktree::list(project_root)?;
            output::run_msg(&format!("would remove {} worktree(s)", existing.len()));
        } else {
            let removed = worktree::purge_all(project_root)?;
            output::run_msg(&format!("removed {removed} worktree(s)"));
        }
    } else {
        let existing = worktree::list(project_root)?;
        if !existing.is_empty() {
            output::run_msg(&format!(
                "{} persistent worktree(s); run `brokkr clean --worktrees` to remove",
                existing.len(),
            ));
        }
    }

    output::result_msg("clean done");
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
/// different shape: elivagar wipes `tilegen_tmp` wholesale (and recreates it),
/// nidhogg has two named tmp dirs, and pbfhogg/others sweep loose `.pbf`
/// scratch, geocode output dirs, and dead external-join dirs.
fn clean_scratch(project: Project, project_root: &Path, paths: &config::ResolvedPaths, c: &Cleaner) {
    if !paths.scratch_dir.exists() {
        return;
    }
    if project == Project::Elivagar {
        // Elivagar scratch is tilegen_tmp - remove all contents and recreate.
        if c.dir(&paths.scratch_dir) {
            if !c.dry_run {
                std::fs::create_dir_all(&paths.scratch_dir).ok();
            }
            output::run_msg(&format!("{} tilegen_tmp", c.past()));
        }
        return;
    }
    if project == Project::Nidhogg {
        if c.dir(&project_root.join(".ingest_tmp")) {
            output::run_msg(&format!("{} .ingest_tmp", c.past()));
        }
        if c.dir(&project_root.join(".tilegen_tmp")) {
            output::run_msg(&format!("{} .tilegen_tmp", c.past()));
        }
        return;
    }

    let mut removed = 0u32;
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
                    && let Ok(pid) = pid_str.parse::<i32>()
                {
                    let alive = unsafe { libc::kill(pid, 0) } == 0;
                    if !alive {
                        c.dir(&path);
                        removed += 1;
                    }
                }
            }
        }
    }
    if removed > 0 {
        output::run_msg(&format!("{} {removed} scratch file(s)", c.verb()));
    }
}

/// Clean the ratatoskr and piners run-artefact trees. Both are debris by the
/// time `clean` runs (we hold the project lock). For piners only the `run-N/`
/// dirs go - the corpus run store (`runs.db` + wal/shm) is the source of truth
/// and must survive.
fn clean_artefact_trees(project: Project, project_root: &Path, c: &Cleaner) {
    if project == Project::Ratatoskr {
        let ratatoskr_root = project_root.join(".brokkr/ratatoskr");
        if ratatoskr_root.exists() {
            let runs = count_run_dirs(&ratatoskr_root);
            c.dir(&ratatoskr_root);
            output::run_msg(&format!("{} {runs} ratatoskr run dir(s)", c.verb()));
        }
    }

    if project == Project::Piners {
        let corpus_root = project_root.join(".brokkr/piners/corpus");
        let mut removed = 0;
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
            output::run_msg(&format!("{} {removed} piners run dir(s)", c.verb()));
        }
    }
}

/// The default `-o` directory for `pmtiles-corpus mutate` calibrands, under the
/// data dir. A routine `brokkr clean` clears it wholesale; an explicit `-o`
/// elsewhere is the user's file and is never touched.
pub(crate) const CORPUS_CALIBRAND_DIR: &str = "corpus-calibrands";

/// Deep-clean (`--worktrees`) the durable tilegen output store: removes ALL
/// `*.pmtiles` in the output dir. Unlike `--archives` (canonical-name,
/// per-(dataset,variant), keep-N), the deep clean wipes the store wholesale
/// because it is reproducible (rerun tilegen). Skipped when the output dir
/// coincides with scratch (already wiped by the caller).
fn clean_elivagar_outputs(paths: &config::ResolvedPaths, c: &Cleaner) {
    if paths.output_dir == paths.scratch_dir || !paths.output_dir.exists() {
        return;
    }
    let mut removed = 0u32;
    if let Ok(entries) = std::fs::read_dir(&paths.output_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("pmtiles") {
                c.file(&path);
                removed += 1;
            }
        }
    }
    if removed > 0 {
        output::run_msg(&format!("{} {removed} tilegen output archive(s)", c.verb()));
    }
}

/// `--archives`: prune canonical `<dataset>-<variant>-<commit>.pmtiles` archives
/// in the durable output store, keeping the newest `keep` per (dataset,
/// variant). Groups are built by CONSTRUCTING each known (dataset, variant)
/// prefix from config (`resolve::pmtiles_archive_prefix`) - filenames are never
/// parsed back, since dataset names carry hyphens. The safety property follows:
/// anything not matching a constructed prefix (hand-named files, the
/// toml-contract ocean artifact, pre-rename `<dataset>-<commit>` archives) is
/// preserved unconditionally, because a file brokkr can't name by construction
/// is self-evidently not brokkr's to delete.
fn clean_archives(paths: &config::ResolvedPaths, keep: usize, c: &Cleaner) {
    let output_dir = &paths.output_dir;
    if !output_dir.exists() {
        return;
    }
    let mut removed = 0u32;
    for (dataset, ds) in &paths.datasets {
        for variant in ds.pbf.keys() {
            let prefix = crate::resolve::pmtiles_archive_prefix(dataset, variant);
            let mut archives: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
            let Ok(entries) = std::fs::read_dir(output_dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_match = path.extension().and_then(|e| e.to_str()) == Some("pmtiles")
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&prefix));
                if is_match {
                    let mtime = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    archives.push((mtime, path));
                }
            }
            if archives.len() <= keep {
                continue;
            }
            // Newest first; prune everything past the keep window.
            archives.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
            for (_, path) in archives.into_iter().skip(keep) {
                c.file(&path);
                removed += 1;
            }
        }
    }
    if removed > 0 {
        output::run_msg(&format!("{} {removed} archive(s)", c.verb()));
    }
}

/// Remove the per-sweep isolated target dirs (`<target>/rustflags-<hash>`) that
/// a `rustflags` `[[check]]` entry mints (see `check_cmd::output`'s
/// `isolated_target_dir`). They are reproducible build scratch - a full rebuild
/// recreates them - and nothing else reclaims them: `cargo clean -p` only
/// touches the default target dir, and editing a sweep's `rustflags` orphans the
/// old hash's tree permanently. A routine `brokkr clean` sweeps them (S3-06).
fn clean_rustflags_target_dirs(target_dir: &Path, c: &Cleaner) {
    let mut removed = 0u32;
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
            "{} {removed} isolated rustflags target dir(s)",
            c.verb()
        ));
    }
}

fn count_run_dirs(root: &Path) -> u32 {
    let mut count = 0u32;
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

    // Line 1: brokkr orchestrator + the invocation it was asked to run.
    let brokkr_uptime = if info.pid > 0 {
        lockfile::process_uptime(info.pid)
    } else {
        None
    };
    let uptime_suffix = brokkr_uptime
        .as_deref()
        .map(|u| format!(" running {u}"))
        .unwrap_or_default();
    let invocation = if info.args.is_empty() {
        format!("{} {}", info.project, info.command)
    } else {
        format!("{} {}", info.project, info.args)
    };
    output::lock_msg(&format!(
        "brokkr PID {}{}: {}",
        info.pid, uptime_suffix, invocation,
    ));
    output::lock_msg(&format!("root: {}", info.project_root));

    // Line 3: child process stats (if brokkr is currently running one).
    if let Some(child_pid) = info.child_pid
        && let Some(summary) = lockfile::process_summary(child_pid)
    {
        let prefix = info
            .progress
            .map(|(r, t)| format!("run {r}/{t}, "))
            .unwrap_or_default();
        output::lock_msg(&format!("{prefix}child PID {child_pid} {summary}"));
    }

    // Lines 3b...: one per auxiliary mock-server (service-suite keeps
    // one mock per distinct fixture alive for the whole suite, so this
    // can be more than one line; sync-smoke / sync-bench / service-test
    // emit at most one).
    for mock_pid in &info.mock_pids {
        if let Some(summary) = lockfile::process_summary(*mock_pid) {
            output::lock_msg(&format!("mock PID {mock_pid} {summary}"));
        }
    }

    // Line 4: most recent sidecar marker, if any.
    if info.pid > 0 {
        let status_path = std::path::Path::new(&info.project_root)
            .join(".brokkr")
            .join(".sidecar-status");
        if let Ok(marker) = std::fs::read_to_string(&status_path) {
            let marker = marker.trim();
            if !marker.is_empty() {
                output::lock_msg(&format!("last marker: {marker}"));
            }
        }
    }

    Ok(())
}

fn cmd_fmt(args: &[String]) -> Result<(), DevError> {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command as ProcCommand;

    let mut cmd = ProcCommand::new("cargo");
    cmd.arg("fmt");
    cmd.args(args);
    let status = cmd.status().map_err(|e| DevError::Subprocess {
        program: "cargo".into(),
        code: None,
        stderr: e.to_string(),
    })?;
    if status.success() {
        return Ok(());
    }
    match status.code() {
        Some(code) => Err(DevError::Subprocess {
            program: "cargo fmt".into(),
            code: Some(code),
            stderr: String::new(),
        }),
        None => Err(DevError::Subprocess {
            program: "cargo fmt".into(),
            code: None,
            stderr: format!("killed by signal {}", status.signal().unwrap_or(0)),
        }),
    }
}

fn cmd_cargo_run(args: &[String]) -> Result<(), DevError> {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command as ProcCommand;

    let mut cmd = ProcCommand::new("cargo");
    cmd.arg("run");
    cmd.args(args);
    let status = cmd.status().map_err(|e| DevError::Subprocess {
        program: "cargo".into(),
        code: None,
        stderr: e.to_string(),
    })?;
    if status.success() {
        return Ok(());
    }
    match status.code() {
        Some(code) => Err(DevError::Subprocess {
            program: "cargo run".into(),
            code: Some(code),
            stderr: String::new(),
        }),
        None => Err(DevError::Subprocess {
            program: "cargo run".into(),
            code: None,
            stderr: format!("killed by signal {}", status.signal().unwrap_or(0)),
        }),
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

    for file in files {
        pmtiles::run(file)?;
    }
    Ok(())
}

/// Ask the brokkr process holding the lock to shut down. Default sends
/// SIGTERM (cooperative - brokkr handles cleanup itself). `--hard`
/// sends SIGKILL to both brokkr and the recorded child PID.
fn cmd_kill(hard: bool) -> Result<(), DevError> {
    let Some(info) = lockfile::status()? else {
        output::lock_msg("no active lock");
        return Ok(());
    };
    if info.pid == 0 {
        output::lock_msg("lock held by unknown process; nothing to kill");
        return Ok(());
    }

    if hard {
        // Kill children first, then brokkr - otherwise there's a brief
        // window where brokkr is dead but the tool it was measuring is
        // still alive (and anyone peeking at `brokkr lock` sees stale
        // state pointing at a live child with no owner).
        //
        // Tracked children may have been spawned with `process_group(0)`
        // (sync-smoke / service-test / service-suite / mock-serve /
        // BenchHarness sidecar) or not (sync-bench pre-loop spawns,
        // nidhogg tile server). The lockfile doesn't carry the policy,
        // so detect at kill time via `getpgid(pid)` and fan out
        // accordingly: PG leader → group kill (sweeps descendants);
        // non-leader → single-PID kill. Brokkr itself shares the user's
        // terminal session and is never a PG leader of its own children,
        // so we keep its kill as a plain per-PID kill.
        if let Some(child_pid) = info.child_pid {
            let (kind, sent) = kill_tracked_pid(child_pid, libc::SIGKILL);
            output::lock_msg(&format!(
                "SIGKILL child {kind} {child_pid}: {}",
                if sent { "sent" } else { "not running" },
            ));
        }
        for mock_pid in &info.mock_pids {
            let (kind, sent) = kill_tracked_pid(*mock_pid, libc::SIGKILL);
            output::lock_msg(&format!(
                "SIGKILL mock {kind} {mock_pid}: {}",
                if sent { "sent" } else { "not running" },
            ));
        }
        let brokkr_sent = send_signal(info.pid, libc::SIGKILL);
        output::lock_msg(&format!(
            "SIGKILL brokkr PID {}: {}",
            info.pid,
            if brokkr_sent { "sent" } else { "not running" },
        ));
        output::lock_msg("follow up with `brokkr clean` to wipe scratch");
        return Ok(());
    }

    if !send_signal(info.pid, libc::SIGTERM) {
        output::lock_msg(&format!(
            "brokkr PID {} is not running; nothing to kill",
            info.pid,
        ));
        return Ok(());
    }
    output::lock_msg(&format!(
        "SIGTERM sent to brokkr PID {} - cleanup in progress",
        info.pid,
    ));
    Ok(())
}

/// Send a signal to a PID. Returns `true` if the process existed.
fn send_signal(pid: u32, signal: libc::c_int) -> bool {
    let ret = unsafe { libc::kill(pid.cast_signed(), signal) };
    if ret == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    err.raw_os_error() != Some(libc::ESRCH)
}

/// Send a signal to the process group whose PGID equals `pid` (i.e.
/// `kill(-pid, signal)`). Returns `true` if the group existed. Used
/// for tracked children spawned with `process_group(0)` so descendants
/// (rustc, sæhrimnir's helpers, etc.) go down with the leader.
fn send_pgrp_signal(pid: u32, signal: libc::c_int) -> bool {
    let ret = unsafe { libc::kill(-pid.cast_signed(), signal) };
    if ret == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    err.raw_os_error() != Some(libc::ESRCH)
}

/// Signal a tracked PID, choosing PG-kill vs single-PID kill at runtime
/// based on whether the PID is a process-group leader. Returns the
/// label used in user-facing output (`"PG"` / `"PID"`) plus whether
/// the target existed at signal time.
///
/// The lockfile doesn't carry an isolation policy alongside each PID,
/// so we use `getpgid(pid) == pid` to detect: spawns that called
/// `process_group(0)` become their own PG leader (PGID == PID), while
/// non-isolated spawns inherit brokkr's PG (PGID != PID). For the
/// former, `kill(-pid, ...)` sweeps the whole group; for the latter
/// it would target brokkr's own PG (catastrophic) or return ESRCH if
/// no group with that PGID exists, missing the actual child.
fn kill_tracked_pid(pid: u32, signal: libc::c_int) -> (&'static str, bool) {
    let pgid = unsafe { libc::getpgid(pid.cast_signed()) };
    // Cast guarded by `pgid > 0`; `cast_unsigned` is the documented
    // i32->u32 conversion that clippy doesn't flag (mirrors the
    // pid.cast_signed() pattern we use for the inverse direction).
    if pgid > 0 && pgid.cast_unsigned() == pid {
        ("PG", send_pgrp_signal(pid, signal))
    } else {
        ("PID", send_signal(pid, signal))
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
            tiles,
            geometry_stats,
        } => {
            project::require(project, Project::Elivagar, "verify")?;
            elivagar::cmd::verify(
                dev_config,
                project,
                project_root,
                build_root,
                &dataset,
                tiles.as_deref(),
                features,
                geometry_stats,
            )
        }

        // ----- nidhogg verify variants -----
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
    output::hotpath_msg(&format!("=== {} {label} ===", req.project));

    if alloc {
        output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
    }

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
