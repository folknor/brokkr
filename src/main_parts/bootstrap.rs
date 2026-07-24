// Measurement entry point: threads the resolved config/project/dataset/variant
// context through to the closure. The args are the context, not a smell.
#[allow(clippy::too_many_arguments)]
fn run_measured<F>(
    mode: &cli::ModeArgs,
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &std::path::PathBuf,
    dataset: &str,
    variant: &str,
    brokkr_args: &str,
    f: F,
) -> Result<(), DevError>
where
    F: FnOnce(&measure::MeasureRequest) -> Result<(), DevError>,
{
    let mm = resolve_mode(mode)?;
    let features = resolve_features(dev_config, &mode.features);
    output::set_quiet(!mode.verbose);
    // When brokkr.toml lives one level up, `project_root` is the config dir
    // and the code tree is cwd; build/git must run against cwd. `None` in the
    // common case (config in cwd), where behaviour is unchanged.
    let cwd = std::env::current_dir()
        .map_err(|e| DevError::Config(format!("cannot determine current directory: {e}")))?;
    let parent_build_root = (cwd != *project_root).then_some(cwd.as_path());
    context::with_worktree(
        project_root,
        parent_build_root,
        mode.commit.as_deref(),
        mode.dry_run,
        dev_config.disable_toolchain,
        |build_root| {
            let req = measure::MeasureRequest {
                dev_config,
                project,
                project_root,
                build_root,
                dataset,
                variant,
                features: &features,
                force: mode.force,
                mode: mm,
                brokkr_args,
                dry_run: mode.dry_run,
                stop_marker: mode.stop.as_deref(),
            };
            f(&req)
        },
    )
}

/// Build the canonical brokkr invocation string from `std::env::args()`.
///
/// Shell-quotes any argv element containing whitespace or special characters
/// so the joined string is unambiguous. Stored on each result row in the
/// `brokkr_args` column, parallel to `cli_args` (the subprocess invocation).
fn capture_brokkr_args() -> String {
    let argv: Vec<String> = std::env::args().collect();
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    if let Some((program, args)) = refs.split_first() {
        harness::format_cli_args(program, args)
    } else {
        String::new()
    }
}

/// Parse argv, with subcommands that don't apply to the detected project
/// hidden from `--help`.
///
/// Hidden is not disabled: a hidden subcommand still parses and still reaches
/// its handler, where `project::require()` produces the "only available in X
/// projects" error it always did. The filtering is purely about what a help
/// listing shows, so `brokkr --help` in a piners tree isn't a menu of tilegen
/// and nidhogg commands.
///
/// Fails open in every direction. Detection runs before parsing, so it cannot
/// use the CLI, and any failure to detect - no `brokkr.toml`, a malformed one,
/// an unreadable cwd - leaves the full command list visible rather than
/// hiding commands on the strength of a guess. A `brokkr.toml` so broken that
/// the project is unknown must not also cost the user their `--help`.
fn parse_cli() -> Cli {
    let Some(project) = project::detect_optional().ok().flatten().map(|d| d.project) else {
        return Cli::parse();
    };

    let mut cmd = <Cli as clap::CommandFactory>::command();
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();

    for name in names {
        if !cli::visible_in(&name, project) {
            cmd = cmd.mut_subcommand(&name, |s| s.hide(true));
        }
    }

    let matches = cmd.get_matches();
    match <Cli as clap::FromArgMatches>::from_arg_matches(&matches) {
        Ok(cli) => cli,
        // The matches came from the same derived command, so a conversion
        // failure is a clap-internal contract break, not user error: let clap
        // render and exit exactly as `parse()` would.
        Err(e) => e.exit(),
    }
}

fn main() {
    let raw_args: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let start = Instant::now();

    let cli = parse_cli();

    // Lead our own process group before spawning anything or acquiring the
    // lock, so brokkr's internal `kill(-pgid, …)` sweeps can't escape upward
    // into a launcher that spawned us without its own session. No-op for the
    // common interactive-foreground case; see `shutdown::isolate_process_group`.
    shutdown::isolate_process_group();

    // Don't record `history` itself (avoids recursive noise).
    let is_history = matches!(cli.command, Command::History { .. });

    let result = run(cli);
    let elapsed_ms = duration_ms(start.elapsed());

    let exit_code = match &result {
        Ok(()) => 0,
        Err(DevError::ExitCode(code)) => *code,
        Err(DevError::Interrupted) => 130,
        Err(_) => 1,
    };

    if !is_history {
        history_cmd::record_history(&raw_args, elapsed_ms, exit_code);
    }

    match result {
        Ok(()) => {}
        Err(DevError::ExitCode(code)) => process::exit(code),
        Err(DevError::Interrupted) => {
            output::lock_msg("interrupted - running scratch cleanup");
            // Best-effort cleanup; if project detection fails here, the
            // user already has `brokkr clean` as a follow-up.
            if let Ok(d) = project::detect()
                && let Err(e) = cmd_clean(&d.config, d.project, &d.project_root, false)
            {
                output::error(&format!("cleanup failed: {e}"));
            }
            process::exit(130);
        }
        Err(e) => {
            output::error(&e.to_string());
            process::exit(1);
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(cli: Cli) -> Result<(), DevError> {
    // These commands work without a project root.
    if matches!(cli.command, Command::Lock) {
        return cmd_lock();
    }
    if let Command::Kill { hard } = cli.command {
        return cmd_kill(hard);
    }
    if let Command::History {
        id,
        command,
        project,
        project_dir,
        failed,
        status,
        since,
        until,
        slow,
        limit,
        all,
    } = cli.command
    {
        return history_cmd::cmd_history(history_cmd::HistoryQuery {
            id,
            command,
            project,
            project_dir,
            failed,
            status,
            since,
            until,
            slow,
            limit,
            all,
        });
    }
    if let Command::Fmt { args } = &cli.command {
        // `cargo fmt` runs rustfmt from the pinned toolchain, so honour
        // disable_toolchain. When it's set we must move the pin aside; do it
        // by arming the build root and riding the global lock's activation -
        // the same mechanism every build path uses - rather than a bare
        // guard. A bare guard would race a concurrent locked build that had
        // already moved the same file aside: fmt would adopt that build's
        // sidecar and restore it mid-command. Under the lock, fmt and the
        // build can't overlap. When disable_toolchain is off there is nothing
        // to move, so fmt stays lock-free as before.
        match project::detect_optional()? {
            Some(d) if d.config.disable_toolchain => {
                toolchain::arm(Some(d.build_root.clone()));
                let _lock = acquire_cmd_lock(d.project, &d.build_root, "fmt")?;
                return cmd_fmt(args);
            }
            _ => return cmd_fmt(args),
        }
    }
    if let Command::Man { topic } = &cli.command {
        // Reading the docs must work in a tree brokkr knows nothing about, so
        // an undetectable project falls back to `Other("")` and gets the
        // project-agnostic topics rather than an error. This sits ABOVE the
        // `detect_optional()?` below on purpose: that `?` propagates a parse
        // error from a malformed brokkr.toml, which would abort `man` before
        // its own `.ok()` fallback could run - the exact case the fallback
        // exists to cover. No lock: this reads compiled-in strings and touches
        // nothing on disk, and needs no toolchain arming (it never builds).
        let project = project::detect_optional()
            .ok()
            .flatten()
            .map_or(Project::Other(""), |d| d.project);
        return man::run(topic.as_deref(), project);
    }

    // When `disable_toolchain` is set, arm the build root whose pinned
    // rust-toolchain should be moved aside. Nothing is moved here: the *global
    // lock* activates it (and restores it on release), so the moved-aside window
    // is exactly the locked window and concurrent brokkr runs can't race it. The
    // file lives in the code tree (build root / cwd), not the config dir; a
    // `--commit` run re-arms this at the worktree via `with_worktree`. This peeks
    // the config once; per-command detection still happens below.
    let disable_dir = match project::detect_optional()? {
        Some(d) if d.config.disable_toolchain => Some(d.build_root),
        _ => None,
    };
    toolchain::arm(disable_dir);

    if let Command::Run { args } = &cli.command {
        // `run` builds and runs the code, so it anchors on the build root
        // (cwd), not the config dir. Detection is only consulted for the
        // project label on the lock.
        let (project, project_root) = match project::detect_optional()? {
            Some(d) => (Some(d.project), d.build_root),
            None => (None, std::env::current_dir()?),
        };
        let _lock = acquire_cmd_lock_opt(project, &project_root, "run")?;
        return cmd_cargo_run(args);
    }
    if let Command::Wc { threshold } = &cli.command {
        // `wc` lists source files in the code tree (cwd).
        let project_root = match project::detect_optional()? {
            Some(d) => d.build_root,
            None => std::env::current_dir()?,
        };
        return wc::run(&project_root, *threshold);
    }
    if let Command::Deps {
        json,
        limit,
        all,
        no_fail,
        focus,
    } = cli.command
    {
        // `deps` audits the code tree's Cargo.lock / metadata (cwd). It shells
        // out to `cargo metadata` and `ccu`/`rustc`, all rustup-mediated, so it
        // must take the global lock like the other build paths: the lock is what
        // activates the armed toolchain-disable, moving a foreign checkout's
        // uninstalled pin aside for the window these tools run in.
        let (project, project_root, workspace_dep_ignore) = match project::detect_optional()? {
            Some(d) => (
                Some(d.project),
                d.build_root,
                d.config
                    .deps
                    .map(|c| c.workspace_dep_ignore)
                    .unwrap_or_default(),
            ),
            None => (None, std::env::current_dir()?, Vec::new()),
        };
        let _lock = acquire_cmd_lock_opt(project, &project_root, "deps")?;
        return deps::run(
            &project_root,
            &deps::DepsArgs {
                json,
                limit,
                all,
                no_fail,
                focus,
                workspace_dep_ignore,
            },
        );
    }
    if let Command::Check {
        features,
        no_default_features,
        package,
        profile,
        gate,
        raw,
        json,
        limit,
        all,
        fix_gremlins,
        timings,
        commands,
        args,
    } = cli.command
    {
        // `check` builds, scans, and diffs the code tree, so it anchors on
        // the build root (cwd). The config values it uses ([[check]] sweeps,
        // dependency rules, gremlin excludes) come from wherever brokkr.toml
        // was found, in cwd or one level up.
        let (
            project,
            check_entries,
            dependency_rules,
            quarantine,
            test_cfg,
            gremlins_cfg,
            style_cfg,
            header_cfg,
            textlint_rules,
            script_checks,
            manifest_cfg,
            project_root,
            state_root,
        ) = match project::detect_optional()? {
            Some(d) => (
                Some(d.project),
                d.config.check,
                d.config.dependency_rules,
                d.config.quarantine,
                d.config.test,
                d.config.gremlins,
                d.config.style,
                d.config.header,
                d.config.textlint,
                d.config.script_checks,
                d.config.manifest,
                // cargo/git run in the code tree (build_root); brokkr's own
                // `.brokkr` state anchors to the config dir (project_root).
                // The two differ only under the one-level-up layout.
                d.build_root,
                d.project_root,
            ),
            None => {
                let cwd = std::env::current_dir()?;
                (
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    None,
                    None,
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    None,
                    cwd.clone(),
                    cwd,
                )
            }
        };
        let _lock = acquire_cmd_lock_opt(project, &state_root, "check")?;
        return check_cmd::cmd_check(
            project,
            &project_root,
            &state_root,
            &check_entries,
            &dependency_rules,
            &quarantine,
            test_cfg.as_ref(),
            gremlins_cfg.as_ref(),
            style_cfg.as_ref(),
            header_cfg.as_ref(),
            &textlint_rules,
            &script_checks,
            manifest_cfg.as_ref(),
            &features,
            no_default_features,
            package.as_deref(),
            profile.as_deref(),
            gate,
            raw,
            json,
            limit,
            all,
            fix_gremlins,
            timings,
            commands,
            &args,
        );
    }
    if let Command::Clippy {
        package,
        all_features,
        features,
        no_default_features,
        sweep,
        env,
        raw,
        limit,
        all,
    } = cli.command
    {
        // Like `check`, `clippy` builds the code tree (cwd), reading `[[check]]`
        // env/features from wherever brokkr.toml was found. The disable dir armed
        // above activates when `acquire_cmd_lock_opt` takes the global lock below,
        // so the clippy build honours pin-suppression. Detection is optional -
        // `clippy` runs in any Rust+git repo with no config.
        let (project, check_entries, project_root) = match project::detect_optional()? {
            Some(d) => (Some(d.project), d.config.check, d.build_root),
            None => (None, Vec::new(), std::env::current_dir()?),
        };
        let env_overrides = parse_env_overrides(&env)?;
        let _lock = acquire_cmd_lock_opt(project, &project_root, "clippy")?;
        return check_cmd::cmd_clippy(
            &project_root,
            &check_entries,
            &package,
            all_features,
            &features,
            no_default_features,
            sweep.as_deref(),
            &env_overrides,
            raw,
            limit,
            all,
        );
    }

    let detection = project::detect()?;
    let project = detection.project;
    let dev_config = detection.config;
    // The config dir (brokkr.toml's directory). Anchors data/ and .brokkr/.
    // Commands that build/git against the code tree derive the build root
    // (cwd) themselves - see `run_measured`.
    let project_root = detection.project_root;
    // The code tree (cwd), where cargo and git run. Coincides with
    // `project_root` in the common case (config in cwd) and differs only under
    // the one-level-up layout used to drive a foreign checkout.
    let build_root = detection.build_root;
    let brokkr_args = capture_brokkr_args();

    // Pbfhogg measured commands: 28 commands → single dispatch path.
    if let Some((mode, pbf, pbf_cmd, osc, mut params)) = cli.command.as_pbfhogg() {
        params.direct_io = pbf.direct_io;
        params.io_uring = pbf.io_uring;
        params.compression = pbf.compression.clone();
        return run_measured(
            mode,
            &dev_config,
            project,
            &project_root,
            &pbf.dataset,
            &pbf.variant,
            &brokkr_args,
            |req| pbfhogg::dispatch::run_command_with_params(req, &pbf_cmd, osc, &params),
        );
    }

    match cli.command {
        // Already handled by as_pbfhogg() above the match.
        Command::Lock
        | Command::Kill { .. }
        | Command::History { .. }
        | Command::Inspect { .. }
        | Command::CheckRefs { .. }
        | Command::CheckIds { .. }
        | Command::Sort { .. }
        | Command::Cat { .. }
        | Command::TagsFilter { .. }
        | Command::Getid { .. }
        | Command::Getparents { .. }
        | Command::Renumber { .. }
        | Command::MergeChanges { .. }
        | Command::ApplyChanges { .. }
        | Command::AddLocationsToWays { .. }
        | Command::Repack { .. }
        | Command::Degrade { .. }
        | Command::TimeFilter { .. }
        | Command::Diff { .. }
        | Command::BuildGeocodeIndex { .. }
        | Command::Check { .. }
        | Command::Clippy { .. }
        | Command::Fmt { .. }
        | Command::Run { .. }
        | Command::Wc { .. }
        | Command::Man { .. }
        | Command::Deps { .. } => unreachable!(),
        Command::Env => cmd_env(&dev_config, project, &project_root),
        Command::DiffSnapshots {
            mode,
            dataset,
            from,
            to,
            variant,
            format,
            jobs,
        } => {
            let params = crate::measure::CommandParams {
                from_snapshot: Some(from.clone()),
                to_snapshot: Some(to.clone()),
                jobs,
                ..Default::default()
            };
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                &variant,
                &brokkr_args,
                |req| {
                    let cmd = pbfhogg::commands::PbfhoggCommand::DiffSnapshots { format };
                    pbfhogg::dispatch::run_command_with_params(req, &cmd, None, &params)
                },
            )
        }
        Command::Extract {
            mode,
            pbf,
            strategy,
            bbox,
            snapshot,
        } => {
            let params = crate::measure::CommandParams {
                bbox: bbox.clone(),
                snapshot: snapshot.clone(),
                ..Default::default()
            };
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &pbf.dataset,
                &pbf.variant,
                &brokkr_args,
                |req| {
                    if strategy == "all" {
                        for strat in pbfhogg::commands::ExtractStrategy::all() {
                            let cmd =
                                pbfhogg::commands::PbfhoggCommand::Extract { strategy: *strat };
                            pbfhogg::dispatch::run_command_with_params(req, &cmd, None, &params)?;
                        }
                        Ok(())
                    } else {
                        let strat = pbfhogg::commands::ExtractStrategy::parse(&strategy)?;
                        let cmd = pbfhogg::commands::PbfhoggCommand::Extract { strategy: strat };
                        pbfhogg::dispatch::run_command_with_params(req, &cmd, None, &params)
                    }
                },
            )
        }
        Command::MultiExtract {
            mode,
            pbf,
            regions,
            bbox,
            strategy,
            snapshot,
        } => {
            let params = crate::measure::CommandParams {
                bbox: bbox.clone(),
                snapshot: snapshot.clone(),
                ..Default::default()
            };
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &pbf.dataset,
                &pbf.variant,
                &brokkr_args,
                |req| {
                    if strategy == "all" {
                        for strat in pbfhogg::commands::ExtractStrategy::all() {
                            let cmd = pbfhogg::commands::PbfhoggCommand::MultiExtract {
                                regions,
                                strategy: *strat,
                            };
                            pbfhogg::dispatch::run_command_with_params(req, &cmd, None, &params)?;
                        }
                        Ok(())
                    } else {
                        let strat = pbfhogg::commands::ExtractStrategy::parse(&strategy)?;
                        let cmd = pbfhogg::commands::PbfhoggCommand::MultiExtract {
                            regions,
                            strategy: strat,
                        };
                        pbfhogg::dispatch::run_command_with_params(req, &cmd, None, &params)
                    }
                },
            )
        }
        Command::Read {
            mode,
            pbf,
            modes,
            snapshot,
        } => run_measured(
            &mode,
            &dev_config,
            project,
            &project_root,
            &pbf.dataset,
            &pbf.variant,
            &brokkr_args,
            |req| pbfhogg::cmd::bench_read(req, &modes, snapshot.as_deref()),
        ),
        Command::Write {
            mode,
            pbf,
            compressions,
        } => run_measured(
            &mode,
            &dev_config,
            project,
            &project_root,
            &pbf.dataset,
            &pbf.variant,
            &brokkr_args,
            |req| pbfhogg::cmd::bench_write(req, &compressions),
        ),
        Command::MergeBench {
            mode,
            pbf,
            compressions,
            uring,
            osc_seq,
        } => run_measured(
            &mode,
            &dev_config,
            project,
            &project_root,
            &pbf.dataset,
            &pbf.variant,
            &brokkr_args,
            |req| {
                pbfhogg::cmd::bench_merge(req, osc_seq.as_deref(), uring, &compressions)
            },
        ),

        // ----- elivagar commands -----
        Command::Tilegen {
            mode,
            dataset,
            variant,
            skip_to,
        } => {
            // The whole pipeline contract comes from the named block; the two
            // input assertions come from the variant that has the property.
            let tilegen = elivagar::resolve_tilegen(&dev_config, elivagar::DEFAULT_TILEGEN)?;
            let (locations_on_ways, force_sorted) =
                elivagar::input_assertions(&dev_config, &dataset, &variant);
            let opts = elivagar::PipelineOpts {
                tilegen,
                locations_on_ways,
                force_sorted,
            };
            let cmd = elivagar::commands::ElivagarCommand::Tilegen {
                opts: &opts,
                skip_to: skip_to.as_deref(),
            };
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                &variant,
                &brokkr_args,
                |req| elivagar::dispatch::run_command(req, &cmd),
            )
        }
        Command::PmtilesWriter { mode, tiles } => {
            let cmd = elivagar::commands::ElivagarCommand::PmtilesWriter { tiles };
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                "denmark",
                "raw",
                &brokkr_args,
                |req| elivagar::dispatch::run_command(req, &cmd),
            )
        }
        Command::NodeStore { mode, nodes } => {
            let cmd = elivagar::commands::ElivagarCommand::NodeStore { nodes };
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                "denmark",
                "raw",
                &brokkr_args,
                |req| elivagar::dispatch::run_command(req, &cmd),
            )
        }
        Command::ElivPlanetiler {
            mode,
            dataset,
            variant,
        } => {
            let cmd = elivagar::commands::ElivagarCommand::Planetiler;
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                &variant,
                &brokkr_args,
                |req| elivagar::dispatch::run_command(req, &cmd),
            )
        }
        Command::ElivTilemaker {
            mode,
            dataset,
            variant,
        } => {
            let cmd = elivagar::commands::ElivagarCommand::Tilemaker;
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                &variant,
                &brokkr_args,
                |req| elivagar::dispatch::run_command(req, &cmd),
            )
        }

        // ----- nidhogg commands -----
        Command::RunApi {
            mode,
            dataset,
            query,
        } => {
            let cmd = nidhogg::commands::NidhoggCommand::Api { query };
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                "raw",
                &brokkr_args,
                |req| nidhogg::dispatch::run_command(req, &cmd),
            )
        }
        Command::RunNidIngest {
            mode,
            dataset,
            variant,
        } => {
            let cmd = nidhogg::commands::NidhoggCommand::Ingest;
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                &variant,
                &brokkr_args,
                |req| nidhogg::dispatch::run_command(req, &cmd),
            )
        }
        Command::RunTiles {
            mode,
            dataset,
            tiles,
            uring,
        } => {
            let cmd = nidhogg::commands::NidhoggCommand::Tiles {
                tiles_variant: tiles,
                uring,
            };
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                "raw",
                &brokkr_args,
                |req| nidhogg::dispatch::run_command(req, &cmd),
            )
        }

        // ----- sluggrs commands -----
        // ----- generic commands -----
        Command::GenericHotpath {
            mode,
            dataset,
            variant,
        } => run_measured(
            &mode,
            &dev_config,
            project,
            &project_root,
            &dataset,
            &variant,
            &brokkr_args,
            |req| {
                if req.dry_run {
                    return Err(DevError::Config(
                        "--dry-run is not yet supported for generic-hotpath".into(),
                    ));
                }
                if !req.is_alloc() && !matches!(req.mode, measure::MeasureMode::Hotpath { .. }) {
                    return Err(DevError::Config(
                        "generic-hotpath only supports --hotpath or --alloc modes".into(),
                    ));
                }
                cmd_hotpath_generic(req)
            },
        ),

        // ----- suites -----
        Command::Suite {
            mode,
            name,
            dataset,
            variant,
        } => run_measured(
            &mode,
            &dev_config,
            project,
            &project_root,
            &dataset,
            &variant,
            &brokkr_args,
            |req| {
                if req.dry_run {
                    return Err(DevError::Config(
                        "--dry-run is not yet supported for suites".into(),
                    ));
                }
                if !matches!(
                    req.mode,
                    measure::MeasureMode::Run | measure::MeasureMode::Bench { .. }
                ) {
                    return Err(DevError::Config(
                        "suite mode only supports wall-clock timing".into(),
                    ));
                }
                match name.as_str() {
                    "pbfhogg" => {
                        project::require(project, Project::Pbfhogg, "suite pbfhogg")?;
                        pbfhogg::cmd::bench_all(req)
                    }
                    "elivagar" => {
                        project::require(project, Project::Elivagar, "suite elivagar")?;
                        elivagar::cmd::bench_all(req)
                    }
                    "nidhogg" => {
                        project::require(project, Project::Nidhogg, "suite nidhogg")?;
                        nidhogg::cmd::bench_api(req, None)?;
                        nidhogg::dispatch::run_command(
                            req,
                            &nidhogg::commands::NidhoggCommand::Ingest,
                        )
                    }
                    other => Err(DevError::Config(format!(
                        "unknown suite: {other} (expected: pbfhogg, elivagar, nidhogg)"
                    ))),
                }
            },
        ),
        Command::Passthrough {
            features,
            time,
            json,
            runs,
            no_build,
            args,
        } => {
            let _lock = acquire_cmd_lock(project, &project_root, "run")?;
            let features = resolve_features(&dev_config, &features);
            let opts = RunOptions {
                time,
                json,
                runs,
                no_build,
            };
            cmd_run(
                &dev_config,
                project,
                &project_root,
                &build_root,
                &features,
                &args,
                &opts,
                Some(&_lock),
            )
        }
        Command::Results {
            query,
            commit,
            compare,
            command,
            mode,
            dataset,
            meta,
            env,
            grep,
            grep_v,
            limit,
            top,
        } => {
            let rq = ResultsQuery {
                query,
                commit,
                compare,
                command,
                mode,
                dataset,
                meta,
                env,
                grep,
                grep_v,
                limit,
                top,
            };
            results_cmd::cmd_results(&dev_config, &project_root, &rq)
        }
        Command::CorpusResults {
            run_id,
            run,
            limit,
            probe,
            diffs,
            columns,
            runtimes,
            over,
            trend,
            where_expr,
            sql,
            full,
        } => {
            project::require(project, Project::Piners, "corpus-results")?;
            let cq = CorpusQuery {
                run_id,
                run,
                limit,
                probe,
                diffs,
                columns,
                runtimes,
                over,
                trend,
                where_expr,
                sql,
                full,
            };
            piners::corpus_query::cmd(&project_root, &cq)
        }
        Command::Sidecar {
            query,
            samples,
            markers,
            durations,
            counters,
            stalls,
            stat,
            compare,
            human,
            run,
            phase,
            grep,
            range,
            r#where,
            fields,
            every,
            head,
            tail,
        } => {
            let sq = SidecarQuery {
                query,
                samples,
                markers,
                durations,
                counters,
                stalls,
                stat,
                compare,
                human,
                run,
                phase,
                grep,
                range,
                where_cond: r#where,
                fields,
                every,
                head,
                tail,
            };
            sidecar_cmd::cmd_sidecar(&project_root, &sq)
        }
        Command::Invalidate { uuid, commit, force } => {
            invalidate_cmd::cmd_invalidate(&project_root, uuid.as_deref(), commit.as_deref(), force)
        }
        Command::Clean { worktrees, cargo } => {
            let _lock = acquire_cmd_lock(project, &project_root, "clean")?;
            if let Some(pkg) = cargo {
                let pkg = pkg.unwrap_or_else(|| project.name().to_owned());
                cargo_clean_package(&build_root, &pkg)?;
            }
            cmd_clean(&dev_config, project, &project_root, worktrees)
        }
        Command::Verify {
            verbose,
            commit,
            verify,
        } => {
            let features = resolve_features(&dev_config, &[]);
            output::set_quiet(!verbose);
            let cwd = std::env::current_dir()
                .map_err(|e| DevError::Config(format!("cannot determine current directory: {e}")))?;
            let parent_build_root = (cwd != project_root).then_some(cwd.as_path());
            with_worktree(&project_root, parent_build_root, commit.as_deref(), false, dev_config.disable_toolchain, |build_root| {
                cmd_verify(
                    &dev_config,
                    project,
                    &project_root,
                    build_root,
                    verify,
                    &features,
                    verbose,
                )
            })
        }
        Command::Download {
            region,
            osc_seq,
            as_snapshot,
            refresh,
            force,
        } => {
            let _lock = acquire_cmd_lock(project, &project_root, "download")?;
            pbfhogg::cmd::download(
                &dev_config,
                project,
                &project_root,
                &build_root,
                &region,
                osc_seq,
                as_snapshot.as_deref(),
                refresh,
                force,
            )
        }
        Command::CompareTiles {
            file_a,
            file_b,
            sample,
        } => {
            // compare_tiles resolves target_dir via `cargo metadata` (bootstrap),
            // so like its elivagar siblings it must hold the lock - that is what
            // activates the armed toolchain-disable before the rustup-mediated
            // metadata call runs in a foreign checkout.
            let _lock = acquire_cmd_lock(project, &project_root, "compare-tiles")?;
            elivagar::cmd::compare_tiles(project, &build_root, &file_a, &file_b, sample)
        }
        Command::PmtilesInspect {
            dataset,
            commit,
            file,
        } => {
            let _lock = acquire_cmd_lock(project, &project_root, "pmtiles-inspect")?;
            elivagar::cmd::inspect(
                &dev_config,
                project,
                &project_root,
                &build_root,
                &dataset,
                commit.as_deref(),
                file.as_deref(),
            )
        }
        Command::Diag {
            dataset,
            commit,
            file,
            z,
            x,
            y,
        } => {
            let _lock = acquire_cmd_lock(project, &project_root, "diag")?;
            elivagar::cmd::diag(
                &dev_config,
                project,
                &project_root,
                &build_root,
                &dataset,
                commit.as_deref(),
                file.as_deref(),
                z,
                x,
                y,
            )
        }
        Command::Svg {
            dataset,
            commit,
            file,
            z,
            x,
            y,
            width,
            height,
            layers,
            output,
        } => {
            let _lock = acquire_cmd_lock(project, &project_root, "svg")?;
            elivagar::cmd::svg(
                &dev_config,
                project,
                &project_root,
                &build_root,
                &dataset,
                commit.as_deref(),
                file.as_deref(),
                z,
                x,
                y,
                width,
                height,
                layers.as_deref(),
                output.as_deref(),
            )
        }
        Command::Regress {
            dataset,
            commit,
            file,
            against_commit,
            against,
            tol,
            max_moved,
            max_examples,
            overlay,
            overlay_max,
            json,
        } => {
            let _lock = acquire_cmd_lock(project, &project_root, "regress")?;
            elivagar::cmd::regress(
                &dev_config,
                project,
                &project_root,
                &build_root,
                &dataset,
                commit.as_deref(),
                file.as_deref(),
                against_commit.as_deref(),
                against.as_deref(),
                tol,
                max_moved,
                max_examples,
                overlay.as_deref(),
                overlay_max,
                json,
                Some(&_lock),
            )
        }
        Command::DownloadOcean => {
            let _lock = acquire_cmd_lock(project, &project_root, "download-ocean")?;
            elivagar::cmd::download_ocean(&dev_config, project, &project_root)
        }
        Command::DownloadNaturalEarth => {
            let _lock = acquire_cmd_lock(project, &project_root, "download-natural-earth")?;
            elivagar::cmd::download_natural_earth(&dev_config, project, &project_root)
        }
        Command::PmtilesStats { files } => cmd_pmtiles_stats(project, &files),
        Command::Serve {
            data_dir,
            dataset,
            tiles,
        } => {
            let _lock = acquire_cmd_lock(project, &project_root, "serve")?;
            let features = resolve_features(&dev_config, &[]);
            nidhogg::cmd::serve(
                &dev_config,
                project,
                &project_root,
                &build_root,
                data_dir.as_deref(),
                &dataset,
                tiles.as_deref(),
                &features,
            )
        }
        Command::Stop => nidhogg::cmd::stop(project, &project_root),
        Command::Status => nidhogg::cmd::status(&dev_config, project, &project_root),
        Command::Ingest { variant, dataset } => {
            let _lock = acquire_cmd_lock(project, &project_root, "ingest")?;
            let features = resolve_features(&dev_config, &[]);
            nidhogg::cmd::ingest(
                &dev_config,
                project,
                &project_root,
                &build_root,
                &variant,
                &dataset,
                &features,
            )
        }
        Command::Update { args } => {
            let _lock = acquire_cmd_lock(project, &project_root, "update")?;
            let features = resolve_features(&dev_config, &[]);
            nidhogg::cmd::update(project, &project_root, &build_root, &args, &features)
        }
        Command::Query { json } => {
            nidhogg::cmd::query(&dev_config, project, &project_root, json.as_deref())
        }
        Command::Geocode { term } => {
            nidhogg::cmd::geocode(&dev_config, project, &project_root, &term)
        }
        // ----- visual testing commands (litehtml + sluggrs) -----
        Command::Visual { fixture, suite, all, recapture } => {
            match project {
                Project::Litehtml => {
                    let litehtml_config = dev_config.litehtml.as_ref().ok_or_else(|| {
                        DevError::Config("no [litehtml] section in brokkr.toml".into())
                    })?;
                    litehtml::cmd::test(project, &project_root, litehtml_config, fixture.as_deref(), suite.as_deref(), all, recapture)
                }
                Project::Sluggrs => {
                    let sluggrs_config = dev_config.sluggrs.as_ref().ok_or_else(|| {
                        DevError::Config("no [sluggrs] section in brokkr.toml".into())
                    })?;
                    sluggrs::cmd::test(project, &project_root, sluggrs_config, fixture.as_deref(), all)
                }
                other => Err(DevError::Config(format!(
                    "'visual' runs visual tests and is only available for litehtml/sluggrs projects (current: {other})"
                )))
            }
        }
        // ----- cargo single-test runner -----
        Command::Test { name, package, repeat, jobs, raw, debug, release, timeout, sweep } => {
            match project {
                Project::Litehtml | Project::Sluggrs => Err(DevError::Config(
                    "'test' runs a single cargo test; litehtml/sluggrs use `brokkr visual` for visual-fixture testing.".into(),
                )),
                _ => {
                    // The lock lives in the config dir (.brokkr); cargo runs
                    // against the code tree (build_root), which differs under
                    // the one-level-up layout.
                    let _lock = acquire_cmd_lock(project, &project_root, "test")?;
                    // cargo runs in the code tree (build_root); brokkr's own
                    // `.brokkr` state (hung-test snapshots) belongs under the
                    // config dir (project_root), which differs under the
                    // one-level-up foreign-checkout layout.
                    test_cmd::run(&dev_config, project, &build_root, &project_root, &name, package.as_deref(), repeat, jobs, raw, profile_override(debug, release), timeout, sweep.as_deref())
                }
            }
        }
        Command::List => {
            match project {
                Project::Litehtml => {
                    let cfg = dev_config.litehtml.as_ref().ok_or_else(|| DevError::Config("no [litehtml] section in brokkr.toml".into()))?;
                    litehtml::cmd::list(project, &project_root, cfg)
                }
                Project::Sluggrs => {
                    let cfg = dev_config.sluggrs.as_ref().ok_or_else(|| DevError::Config("no [sluggrs] section in brokkr.toml".into()))?;
                    sluggrs::cmd::list(project, &project_root, cfg)
                }
                other => Err(DevError::Config(format!(
                    "'list' is only available for litehtml/sluggrs projects (current: {other})"
                )))
            }
        }
        Command::Approve { fixture } => {
            match project {
                Project::Litehtml => {
                    let cfg = dev_config.litehtml.as_ref().ok_or_else(|| DevError::Config("no [litehtml] section in brokkr.toml".into()))?;
                    litehtml::cmd::approve(project, &project_root, cfg, &fixture)
                }
                Project::Sluggrs => {
                    let cfg = dev_config.sluggrs.as_ref().ok_or_else(|| DevError::Config("no [sluggrs] section in brokkr.toml".into()))?;
                    sluggrs::cmd::approve(project, &project_root, cfg, &fixture)
                }
                other => Err(DevError::Config(format!(
                    "'approve' is only available for litehtml/sluggrs projects (current: {other})"
                )))
            }
        }
        Command::Report { run_id } => {
            match project {
                Project::Litehtml => litehtml::cmd::report(project, &project_root, &run_id),
                Project::Sluggrs => sluggrs::cmd::report(project, &project_root, &run_id),
                other => Err(DevError::Config(format!(
                    "'report' is only available for litehtml/sluggrs projects (current: {other})"
                )))
            }
        }
        Command::VisualStatus => {
            match project {
                Project::Litehtml => {
                    let cfg = dev_config.litehtml.as_ref().ok_or_else(|| DevError::Config("no [litehtml] section in brokkr.toml".into()))?;
                    litehtml::cmd::status(project, &project_root, cfg)
                }
                Project::Sluggrs => {
                    let cfg = dev_config.sluggrs.as_ref().ok_or_else(|| DevError::Config("no [sluggrs] section in brokkr.toml".into()))?;
                    sluggrs::cmd::status(project, &project_root, cfg)
                }
                other => Err(DevError::Config(format!(
                    "'visual-status' is only available for litehtml/sluggrs projects (current: {other})"
                )))
            }
        }
        // ----- litehtml-only commands -----
        Command::Prepare { input, output } => {
            let cfg = dev_config.litehtml.as_ref().ok_or_else(|| DevError::Config("no [litehtml] section in brokkr.toml".into()))?;
            litehtml::cmd::prepare(project, &project_root, cfg, &input, &output)
        }
        Command::HtmlExtract { input, selector, from, to, output } => {
            litehtml::cmd::extract(project, &project_root, &input, selector.as_deref(), from.as_deref(), to.as_deref(), &output)
        }
        Command::Outline { input, depth, full, selectors } => {
            litehtml::cmd::outline(project, &project_root, &input, depth, full, selectors)
        }
        // ----- sluggrs-only commands -----
        Command::Hotpath { alloc, runs, target, verbose, force } => {
            project::require(project, Project::Sluggrs, "hotpath")?;
            let mm = if alloc {
                measure::MeasureMode::Alloc { runs }
            } else {
                measure::MeasureMode::Hotpath { runs }
            };
            let features = resolve_features(&dev_config, &[]);
            output::set_quiet(!verbose);
            let req = measure::MeasureRequest {
                dev_config: &dev_config,
                project,
                project_root: &project_root,
                build_root: None,
                dataset: "n/a",
                variant: "n/a",
                features: &features,
                force,
                mode: mm,
                brokkr_args: &brokkr_args,
                // Sluggrs hotpath uses Command::Hotpath, not ModeArgs - no
                // dry-run or stop-marker surface to plumb from.
                dry_run: false,
                stop_marker: None,
            };
            sluggrs::hotpath::cmd(&req, &target)
        }
        // ----- ratatoskr-only commands -----
        Command::ServiceTest {
            script,
            keep_artefacts,
            debug,
            release,
            repeat,
            keep_going,
        } => {
            project::require(project, Project::Ratatoskr, "service-test")?;
            ratatoskr::cmd::service_test(
                &project_root,
                &dev_config,
                &script,
                keep_artefacts,
                profile_override(debug, release),
                repeat,
                keep_going,
            )
        }
        Command::ServiceList => {
            project::require(project, Project::Ratatoskr, "service-list")?;
            ratatoskr::cmd::service_list(&project_root)
        }
        Command::SyncBench {
            script,
            bench,
            force,
            keep_artefacts,
            debug,
            release,
            gate,
            as_baseline,
        } => {
            project::require(project, Project::Ratatoskr, "sync-bench")?;
            ratatoskr::sync::run_sync_bench(&ratatoskr::sync::SyncBenchRequest {
                project_root: &project_root,
                dev_config: &dev_config,
                script: &script,
                bench,
                force,
                keep_artefacts,
                profile_override: profile_override(debug, release),
                brokkr_args: brokkr_args.clone(),
                gate: gate.as_deref(),
                as_baseline,
            })
        }
        Command::SyncList => {
            project::require(project, Project::Ratatoskr, "sync-list")?;
            ratatoskr::sync::run_sync_list(&project_root, &dev_config)
        }
        Command::SyncSmoke {
            script,
            keep_artefacts,
            debug,
            release,
        } => {
            project::require(project, Project::Ratatoskr, "sync-smoke")?;
            ratatoskr::sync::run_sync_smoke(&ratatoskr::sync::SyncSmokeRequest {
                project_root: &project_root,
                dev_config: &dev_config,
                script: &script,
                keep_artefacts,
                profile_override: profile_override(debug, release),
            })
        }
        Command::MockServe { fixture } => {
            project::require(project, Project::Ratatoskr, "mock-serve")?;
            let cfg = dev_config.ratatoskr.as_ref().ok_or_else(|| {
                DevError::Config(
                    "mock-serve: no [ratatoskr] section in brokkr.toml. \
                     Set mock_server_binary and fixtures_dir to point at \
                     sæhrimnir's checkout."
                        .into(),
                )
            })?;
            ratatoskr::saehrimnir::run_mock_serve(&ratatoskr::saehrimnir::MockServeRequest {
                project_root: &project_root,
                config: cfg,
                fixture: &fixture,
            })
        }
        Command::ServiceSuite {
            filter,
            keep_artefacts,
            debug,
            release,
            keep_going,
            include_ignored,
            repeat,
        } => {
            project::require(project, Project::Ratatoskr, "service-suite")?;
            ratatoskr::cmd::service_suite(
                &project_root,
                &dev_config,
                filter.as_deref(),
                keep_artefacts,
                profile_override(debug, release),
                keep_going,
                include_ignored,
                repeat,
            )
        }
        // ----- piners-only commands -----
        Command::Corpus {
            keyword,
            probe,
            all,
            verify_only,
            reseed,
            bless,
            no_gate,
            debug,
            release,
            keep_artefacts,
            harness_args,
            mode,
        } => {
            project::require(project, Project::Piners, "corpus")?;
            // `--force` is dual-purpose: in a parity run it bypasses the
            // runtime ceiling; in a measured run it carries the dirty-tree
            // meaning (handled by run_measured/BenchContext). One field, mode
            // picks the meaning.
            let args = piners::cmd::CorpusArgs {
                keywords: keyword,
                probe,
                all,
                verify_only,
                reseed,
                bless,
                no_gate,
                profile_override: profile_override(debug, release),
                keep_artefacts,
                force: mode.force,
                harness_args,
            };
            if mode.is_measured() {
                // --hotpath/--alloc: build the harness with the hotpath feature
                // and record to results.db, skipping the gate and runs.db.
                run_measured(
                    &mode,
                    &dev_config,
                    project,
                    &project_root,
                    "",
                    "",
                    &brokkr_args,
                    |req| piners::measured::run(req, &args),
                )
            } else {
                // Bare corpus: the parity run (gate + runs.db), unchanged.
                piners::cmd::corpus(&project_root, &dev_config, &args)
            }
        }
        Command::LintCorpus {
            keyword,
            probe,
            all,
            verify_only,
            reseed,
            reanchor,
            bless,
            no_gate,
            all_stages,
            warnings,
            debug,
            release,
        } => {
            project::require(project, Project::Piners, "lint-corpus")?;
            let args = piners::lint::cmd::LintArgs {
                keywords: keyword,
                probe,
                all,
                verify_only,
                reseed,
                reanchor,
                bless,
                no_gate,
                all_stages,
                warnings,
                profile_override: profile_override(debug, release),
            };
            piners::lint::cmd::lint_corpus(&project_root, &dev_config, &args)
        }
        Command::LintResults {
            run_id,
            run,
            limit,
            full,
        } => {
            project::require(project, Project::Piners, "lint-results")?;
            let lq = piners::lint::query::LintQuery {
                run_id,
                run,
                limit,
                full,
            };
            piners::lint::query::cmd(&project_root, &lq)
        }
    }
}
