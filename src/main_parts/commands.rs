
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

fn cmd_run(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    features: &[String],
    args: &[String],
    opts: &RunOptions,
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
        build::resolve_existing_binary(&build_config, project_root)?
    } else {
        build::cargo_build(&build_config, project_root)?
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
        let out = output::run_passthrough_timed(&binary_str, &arg_refs)?;
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

fn cmd_clean(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    worktrees: bool,
) -> Result<(), DevError> {
    let pi = bootstrap(None)?;
    let paths = bootstrap_config(dev_config, project_root, &pi.target_dir)?;

    // Clean verify output (pbfhogg only).
    let verify_dir = paths.target_dir.join("verify");
    if verify_dir.exists() {
        std::fs::remove_dir_all(&verify_dir)?;
        output::run_msg("removed verify output");
    }

    // Clean scratch temp files.
    if paths.scratch_dir.exists() {
        if project == Project::Elivagar {
            // Elivagar scratch is tilegen_tmp - remove all contents.
            std::fs::remove_dir_all(&paths.scratch_dir)?;
            std::fs::create_dir_all(&paths.scratch_dir)?;
            output::run_msg("cleaned tilegen_tmp");
        } else if project == Project::Nidhogg {
            // Clean nidhogg scratch temp files
            let ingest_tmp = project_root.join(".ingest_tmp");
            if ingest_tmp.exists() {
                std::fs::remove_dir_all(&ingest_tmp)?;
                output::run_msg("cleaned .ingest_tmp");
            }
            let tilegen_tmp = project_root.join(".tilegen_tmp");
            if tilegen_tmp.exists() {
                std::fs::remove_dir_all(&tilegen_tmp)?;
                output::run_msg("cleaned .tilegen_tmp");
            }
        } else {
            let mut removed = 0u32;
            if let Ok(entries) = std::fs::read_dir(&paths.scratch_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("pbf") {
                        std::fs::remove_file(&path).ok();
                        removed += 1;
                    }
                    // Clean geocode output directories (geocode-<dataset>/).
                    if path.is_dir()
                        && let Some(name) = path.file_name().and_then(|n| n.to_str())
                    {
                        if name.starts_with("geocode-") {
                            std::fs::remove_dir_all(&path).ok();
                            removed += 1;
                        }
                        // Clean orphaned external-join scratch dirs (.pbfhogg-external-join-{pid}).
                        // These survive OOM kills (SIGKILL prevents Drop cleanup).
                        if let Some(pid_str) = name.strip_prefix(".pbfhogg-external-join-")
                            && let Ok(pid) = pid_str.parse::<i32>()
                        {
                            let alive = unsafe { libc::kill(pid, 0) } == 0;
                            if !alive {
                                std::fs::remove_dir_all(&path).ok();
                                removed += 1;
                            }
                        }
                    }
                }
            }
            if removed > 0 {
                output::run_msg(&format!("removed {removed} scratch file(s)"));
            }
        }
    }

    // Clean ratatoskr artefact tree (`.brokkr/ratatoskr/<test>/run-N/`,
    // `.brokkr/ratatoskr/sync/<test>/run-N/[iter-K/]`, plus the
    // `mock/<fixture>/` dirs left by `mock-serve`). The whole tree is
    // debris by the time `clean` runs because we hold the project lock.
    if project == Project::Ratatoskr {
        let ratatoskr_root = project_root.join(".brokkr/ratatoskr");
        if ratatoskr_root.exists() {
            let runs = count_run_dirs(&ratatoskr_root);
            std::fs::remove_dir_all(&ratatoskr_root)?;
            output::run_msg(&format!("removed {runs} ratatoskr run dir(s)"));
        }
    }

    // Clean piners corpus artefact tree (`.brokkr/piners/corpus/run-N/`, each
    // holding a manifest + captured harness output). These are debris by the
    // time `clean` runs (we hold the project lock) - but the corpus run store
    // (`.brokkr/piners/corpus/runs.db`, + its `-wal`/`-shm`) is the source of
    // truth now and must survive, so only the `run-N/` dirs are removed.
    if project == Project::Piners {
        let corpus_root = project_root.join(".brokkr/piners/corpus");
        let mut removed = 0;
        if corpus_root.exists() {
            for entry in std::fs::read_dir(&corpus_root)?.flatten() {
                let path = entry.path();
                if path.is_dir() && entry.file_name().to_string_lossy().starts_with("run-") {
                    std::fs::remove_dir_all(&path).ok();
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            output::run_msg(&format!("removed {removed} piners run dir(s)"));
        }
    }

    if worktrees {
        let removed = worktree::purge_all(project_root)?;
        output::run_msg(&format!("removed {removed} worktree(s)"));
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

fn cmd_pmtiles_stats(files: &[String]) -> Result<(), DevError> {
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
) -> Result<(), DevError> {
    match verify {
        // ----- elivagar verify variants -----
        VerifyCommand::ElivVerify { dataset, tiles } => {
            project::require(project, Project::Elivagar, "verify")?;
            elivagar::cmd::verify(
                dev_config,
                project,
                project_root,
                build_root,
                &dataset,
                tiles.as_deref(),
                features,
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
            nidhogg::cmd::verify_readonly(dev_config, project, project_root, &dataset, features)
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
        req.force,
        req.wait,
        req.stop_marker.map(str::to_owned),
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

    ctx.harness.run_internal(&config, |_i| {
        let (result, _stderr, _sidecar) = harness::run_hotpath_capture(
            &binary_str,
            &[],
            &ctx.paths.scratch_dir,
            req.project_root,
            &[],
            &[],
            req.stop_marker,
            Some(ctx.harness.lock()),
        )?;
        Ok(result)
    })?;

    Ok(())
}
