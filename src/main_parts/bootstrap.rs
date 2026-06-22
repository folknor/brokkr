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
    context::with_worktree(project_root, mode.commit.as_deref(), mode.dry_run, |build_root| {
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
            wait: mode.wait,
            dry_run: mode.dry_run,
            stop_marker: mode.stop.as_deref(),
        };
        f(&req)
    })
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

fn main() {
    let raw_args: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let start = Instant::now();

    let cli = Cli::parse();

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
            if let Ok((project, dev_config, project_root)) = project::detect()
                && let Err(e) = cmd_clean(&dev_config, project, &project_root, false)
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
        return cmd_fmt(args);
    }
    if let Command::Run { args } = &cli.command {
        let (project, project_root) = match project::detect_optional()? {
            Some((p, _, root)) => (Some(p), root),
            None => (None, std::env::current_dir()?),
        };
        let _lock = acquire_cmd_lock_opt(project, &project_root, "run")?;
        return cmd_cargo_run(args);
    }
    if let Command::Wc { threshold } = &cli.command {
        let project_root = match project::detect_optional()? {
            Some((_, _, root)) => root,
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
        let project_root = match project::detect_optional()? {
            Some((_, _, root)) => root,
            None => std::env::current_dir()?,
        };
        return deps::run(
            &project_root,
            &deps::DepsArgs {
                json,
                limit,
                all,
                no_fail,
                focus,
            },
        );
    }
    if let Command::Check {
        features,
        no_default_features,
        package,
        profile,
        raw,
        json,
        limit,
        all,
        fix_gremlins,
        timings,
        args,
    } = cli.command
    {
        let (project, check_entries, dependency_rules, test_cfg, gremlins_cfg, project_root) =
            match project::detect_optional()? {
                Some((p, cfg, root)) => (
                    Some(p),
                    cfg.check,
                    cfg.dependency_rules,
                    cfg.test,
                    cfg.gremlins,
                    root,
                ),
                None => (None, Vec::new(), Vec::new(), None, None, std::env::current_dir()?),
            };
        let _lock = acquire_cmd_lock_opt_blocking(project, &project_root, "check")?;
        return check_cmd::cmd_check(
            project,
            &project_root,
            &check_entries,
            &dependency_rules,
            test_cfg.as_ref(),
            gremlins_cfg.as_ref(),
            &features,
            no_default_features,
            package.as_deref(),
            profile.as_deref(),
            raw,
            json,
            limit,
            all,
            fix_gremlins,
            timings,
            &args,
        );
    }

    let (project, dev_config, project_root) = project::detect()?;
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
        | Command::Fmt { .. }
        | Command::Run { .. }
        | Command::Wc { .. }
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
        Command::Read { mode, pbf, modes } => run_measured(
            &mode,
            &dev_config,
            project,
            &project_root,
            &pbf.dataset,
            &pbf.variant,
            &brokkr_args,
            |req| pbfhogg::cmd::bench_read(req, &modes),
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
            compression_level,
            no_ocean,
            force_sorted,
            allow_unsafe_flat_index,
            tile_format,
            tile_compression,
            compress_sort_chunks,
            in_memory,
            locations_on_ways,
            fanout_cap_default,
            fanout_cap,
            polygon_simplify_factor,
        } => {
            let opts = elivagar::PipelineOpts {
                no_ocean,
                force_sorted,
                allow_unsafe_flat_index,
                tile_format: tile_format.as_deref(),
                tile_compression: tile_compression.as_deref(),
                compress_sort_chunks: compress_sort_chunks.as_deref(),
                in_memory,
                locations_on_ways,
                fanout_cap_default,
                fanout_cap: fanout_cap.as_deref(),
                polygon_simplify_factor,
            };
            let cmd = elivagar::commands::ElivagarCommand::Tilegen {
                opts: &opts,
                skip_to: skip_to.as_deref(),
                compression_level,
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
            cmd_run(&dev_config, project, &project_root, &features, &args, &opts)
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
        Command::Clean { worktrees } => {
            let _lock = acquire_cmd_lock(project, &project_root, "clean")?;
            cmd_clean(&dev_config, project, &project_root, worktrees)
        }
        Command::Verify {
            verbose,
            commit,
            verify,
        } => {
            let features = resolve_features(&dev_config, &[]);
            output::set_quiet(!verbose);
            with_worktree(&project_root, commit.as_deref(), false, |build_root| {
                cmd_verify(
                    &dev_config,
                    project,
                    &project_root,
                    build_root,
                    verify,
                    &features,
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
        } => elivagar::cmd::compare_tiles(project, &project_root, &file_a, &file_b, sample),
        Command::DownloadOcean => {
            let _lock = acquire_cmd_lock(project, &project_root, "download-ocean")?;
            elivagar::cmd::download_ocean(&dev_config, project, &project_root)
        }
        Command::DownloadNaturalEarth => {
            let _lock = acquire_cmd_lock(project, &project_root, "download-natural-earth")?;
            elivagar::cmd::download_natural_earth(&dev_config, project, &project_root)
        }
        Command::PmtilesStats { files } => cmd_pmtiles_stats(&files),
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
                &variant,
                &dataset,
                &features,
            )
        }
        Command::Update { args } => {
            let _lock = acquire_cmd_lock(project, &project_root, "update")?;
            let features = resolve_features(&dev_config, &[]);
            nidhogg::cmd::update(project, &project_root, &args, &features)
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
        Command::Test { name, package, repeat, jobs, raw, debug, release, timeout } => {
            match project {
                Project::Litehtml | Project::Sluggrs => Err(DevError::Config(
                    "'test' runs a single cargo test; litehtml/sluggrs use `brokkr visual` for visual-fixture testing.".into(),
                )),
                _ => {
                    let _lock = acquire_cmd_lock_blocking(project, &project_root, "test")?;
                    test_cmd::run(&dev_config, project, &project_root, &name, package.as_deref(), repeat, jobs, raw, profile_override(debug, release), timeout)
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
        Command::Hotpath { alloc, runs, target, verbose, force, wait } => {
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
                wait,
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
