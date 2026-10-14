
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
    env::cmd_env(dev_config, project, project_root)
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

fn cmd_clean(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
    build_root: &Path,
    opts: CleanOpts,
) -> Result<(), DevError> {
    clean::cmd_clean(dev_config, project, project_root, build_root, opts)
}

fn cargo_clean_package(build_root: &Path, pkg: &str) -> Result<(), DevError> {
    clean::cargo_clean_package(build_root, pkg)
}

fn cmd_lock() -> Result<(), DevError> {
    lock_cmd::cmd_lock()
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

fn cmd_kill(hard: bool) -> Result<(), DevError> {
    lock_cmd::cmd_kill(hard)
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
    let ctx = context::BenchContext::new_for_request(
        req,
        req.project.cli_package(),
        &hotpath_features,
        true,
        "hotpath",
    )?;

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
