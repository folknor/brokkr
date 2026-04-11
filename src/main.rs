mod build;
mod cli;
mod config;
mod context;
mod db;
mod dispatch;
mod elivagar;
mod env;
mod error;
mod git;
mod harness;
mod history;
mod hotpath_fmt;
mod litehtml;
mod lockfile;
mod measure;
mod nidhogg;
mod oom;
mod output;
mod pbfhogg;
mod pmtiles;
mod preflight;
mod project;
mod request;
mod resolve;
mod sidecar;
mod sluggrs;
mod tools;
mod worktree;

use std::path::Path;
use std::process;
use std::time::{Duration, Instant};

use clap::Parser;

use cli::{Cli, Command, VerifyCommand};
use context::{acquire_cmd_lock, bootstrap, bootstrap_config, with_worktree};
use error::DevError;
use project::Project;
use request::ResultsQuery;
use resolve::results_db_path;

/// Shared setup for all measured commands: resolve mode/features, set quiet,
/// handle worktree, construct `MeasureRequest`, call the provided closure.
fn run_measured<F>(
    mode: &cli::ModeArgs,
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &std::path::PathBuf,
    dataset: &str,
    variant: &str,
    f: F,
) -> Result<(), DevError>
where
    F: FnOnce(&measure::MeasureRequest) -> Result<(), DevError>,
{
    let mm = resolve_mode(mode)?;
    let features = resolve_features(dev_config, &mode.features);
    output::set_quiet(!mode.verbose);
    context::with_worktree(project_root, mode.commit.as_deref(), |build_root| {
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
            no_mem_check: mode.no_mem_check,
            wait: mode.wait,
            dry_run: mode.dry_run,
        };
        f(&req)
    })
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
        Err(_) => 1,
    };

    if !is_history {
        record_history(&raw_args, elapsed_ms, exit_code);
    }

    match result {
        Ok(()) => {}
        Err(DevError::ExitCode(code)) => process::exit(code),
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
    if let Command::History {
        command,
        project,
        failed,
        since,
        slow,
        limit,
        all,
    } = cli.command
    {
        return cmd_history(command, project, failed, since, slow, limit, all);
    }

    let (project, dev_config, project_root) = project::detect()?;

    // Pbfhogg measured commands: 28 commands → single dispatch path.
    if let Some((mode, pbf, pbf_cmd, osc, mut params)) = cli.command.as_pbfhogg() {
        if pbf.direct_io {
            params.insert("direct_io".into(), "true".into());
        }
        if pbf.io_uring {
            params.insert("io_uring".into(), "true".into());
        }
        return run_measured(
            mode,
            &dev_config,
            project,
            &project_root,
            &pbf.dataset,
            &pbf.variant,
            |req| dispatch::run_pbfhogg_command_with_params(req, &pbf_cmd, osc, &params),
        );
    }

    match cli.command {
        // Already handled by as_pbfhogg() above the match.
        Command::Lock
        | Command::History { .. }
        | Command::Inspect { .. }
        | Command::InspectNodes { .. }
        | Command::InspectTags { .. }
        | Command::InspectTagsWay { .. }
        | Command::CheckRefs { .. }
        | Command::CheckIds { .. }
        | Command::Sort { .. }
        | Command::CatWay { .. }
        | Command::CatRelation { .. }
        | Command::CatDedupe { .. }
        | Command::TagsFilterWay { .. }
        | Command::TagsFilterAmenity { .. }
        | Command::TagsFilterTwopass { .. }
        | Command::TagsFilterOsc { .. }
        | Command::Getid { .. }
        | Command::GetidRefs { .. }
        | Command::Getparents { .. }
        | Command::GetidInvert { .. }
        | Command::Renumber { .. }
        | Command::MergeChanges { .. }
        | Command::ApplyChanges { .. }
        | Command::AddLocationsToWays { .. }
        | Command::ExtractSimple { .. }
        | Command::ExtractComplete { .. }
        | Command::ExtractSmart { .. }
        | Command::MultiExtract { .. }
        | Command::TimeFilter { .. }
        | Command::Diff { .. }
        | Command::DiffOsc { .. }
        | Command::BuildGeocodeIndex { .. } => unreachable!(),
        Command::Check {
            features,
            no_default_features,
            package,
            args,
        } => cmd_check(
            project,
            &project_root,
            &features,
            no_default_features,
            package.as_deref(),
            &args,
        ),
        Command::Env => cmd_env(&dev_config, project, &project_root),
        Command::Cat {
            mode,
            dataset,
            variant,
            direct_io,
        } => {
            let mut params = std::collections::HashMap::new();
            if direct_io {
                params.insert("direct_io".into(), "true".into());
            }
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                &variant,
                |req| {
                    dispatch::run_pbfhogg_command_with_params(
                        req,
                        &pbfhogg::commands::PbfhoggCommand::Cat,
                        None,
                        &params,
                    )
                },
            )
        }
        Command::DiffSnapshots {
            mode,
            dataset,
            from,
            to,
            variant,
            format,
        } => {
            let mut params = std::collections::HashMap::new();
            params.insert("from_snapshot".into(), from.clone());
            params.insert("to_snapshot".into(), to.clone());
            let format_enum = pbfhogg::commands::DiffFormat::parse(&format)?;
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &dataset,
                &variant,
                |req| {
                    let cmd = pbfhogg::commands::PbfhoggCommand::DiffSnapshots {
                        format: format_enum,
                    };
                    dispatch::run_pbfhogg_command_with_params(req, &cmd, None, &params)
                },
            )
        }
        Command::Extract {
            mode,
            pbf,
            strategy,
            bbox,
        } => {
            let mut params = std::collections::HashMap::new();
            if let Some(ref b) = bbox {
                params.insert("bbox".into(), b.clone());
            }
            run_measured(
                &mode,
                &dev_config,
                project,
                &project_root,
                &pbf.dataset,
                &pbf.variant,
                |req| {
                    if strategy == "all" {
                        for strat in pbfhogg::commands::ExtractStrategy::all() {
                            let cmd =
                                pbfhogg::commands::PbfhoggCommand::Extract { strategy: *strat };
                            dispatch::run_pbfhogg_command_with_params(req, &cmd, None, &params)?;
                        }
                        Ok(())
                    } else {
                        let strat = pbfhogg::commands::ExtractStrategy::parse(&strategy)?;
                        let cmd = pbfhogg::commands::PbfhoggCommand::Extract { strategy: strat };
                        dispatch::run_pbfhogg_command_with_params(req, &cmd, None, &params)
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
            |req| pbfhogg::cmd::bench_read(req, &modes),
        ),
        Command::Write {
            mode,
            pbf,
            compression,
        } => run_measured(
            &mode,
            &dev_config,
            project,
            &project_root,
            &pbf.dataset,
            &pbf.variant,
            |req| pbfhogg::cmd::bench_write(req, &compression),
        ),
        Command::MergeBench {
            mode,
            pbf,
            compression,
            uring,
            osc_seq,
        } => run_measured(
            &mode,
            &dev_config,
            project,
            &project_root,
            &pbf.dataset,
            &pbf.variant,
            |req| {
                pbfhogg::cmd::bench_merge(req, osc_seq.as_deref(), uring, &compression)
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
                |req| dispatch::run_elivagar_command(req, &cmd),
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
                |req| dispatch::run_elivagar_command(req, &cmd),
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
                |req| dispatch::run_elivagar_command(req, &cmd),
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
                |req| dispatch::run_elivagar_command(req, &cmd),
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
                |req| dispatch::run_elivagar_command(req, &cmd),
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
                |req| dispatch::run_nidhogg_command(req, &cmd),
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
                |req| dispatch::run_nidhogg_command(req, &cmd),
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
                |req| dispatch::run_nidhogg_command(req, &cmd),
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
                        dispatch::run_nidhogg_command(
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
            compare_last,
            command,
            variant,
            dataset,
            meta,
            limit,
            top,
            timeline,
            markers,
            summary,
            durations,
            fields,
            every,
            head,
            tail,
            r#where,
            run,
            stat,
            phase,
            range,
            compare_timeline,
            phases,
            counters,
        } => {
            let rq = ResultsQuery {
                query,
                commit,
                compare,
                compare_last,
                command,
                variant,
                dataset,
                meta,
                limit,
                top,
                timeline,
                markers,
                summary,
                durations,
                fields,
                every,
                head,
                tail,
                where_cond: r#where,
                stat,
                run,
                phase,
                range,
                compare_timeline,
                phases,
                counters,
            };
            cmd_results(&project_root, &rq)
        }
        Command::Clean => {
            let _lock = acquire_cmd_lock(project, &project_root, "clean")?;
            cmd_clean(&dev_config, project, &project_root)
        }
        Command::Verify {
            verbose,
            commit,
            verify,
        } => {
            let features = resolve_features(&dev_config, &[]);
            output::set_quiet(!verbose);
            with_worktree(&project_root, commit.as_deref(), |build_root| {
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
        Command::Test { fixture, suite, all, recapture } => {
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
                    "'test' runs visual tests and is only available for litehtml/sluggrs projects (current: {other})"
                )))
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
        Command::Hotpath { alloc, runs, target, verbose, force, no_mem_check, wait } => {
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
                no_mem_check,
                wait,
                // Sluggrs hotpath uses Command::Hotpath, not ModeArgs — no
                // dry-run surface to plumb from.
                dry_run: false,
            };
            sluggrs::hotpath::cmd(&req, &target)
        }
    }
}

// ---------------------------------------------------------------------------
// Host feature resolution
// ---------------------------------------------------------------------------

/// Merge CLI `--features` with host-configured default features from brokkr.toml.
///
/// Host features are appended first, then CLI features. Duplicates are removed
/// (CLI wins, but since features are additive this just means dedup).
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

fn cmd_check(
    project: Project,
    project_root: &Path,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    extra_args: &[String],
) -> Result<(), DevError> {
    run_clippy(project_root, features, no_default_features, package)?;
    run_tests(
        project,
        project_root,
        features,
        no_default_features,
        package,
        extra_args,
    )?;
    output::result_msg("check passed");
    Ok(())
}

fn run_clippy(
    project_root: &Path,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
) -> Result<(), DevError> {
    let mut args = vec!["clippy", "--all-targets"];
    if no_default_features {
        args.push("--no-default-features");
    }
    let joined = features.join(",");
    if !features.is_empty() {
        args.push("--features");
        args.push(&joined);
    } else if !no_default_features {
        // No explicit features and no --no-default-features: check everything.
        args.push("--all-features");
    }
    if let Some(pkg) = package {
        args.push("--package");
        args.push(pkg);
    }

    output::run_msg(&format!("cargo {}", args.join(" ")));

    let captured = output::run_captured("cargo", &args, project_root)?;

    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        output::error(&stderr);
        return Err(DevError::Build("clippy failed".into()));
    }

    Ok(())
}

fn run_tests(
    project: Project,
    project_root: &Path,
    features: &[String],
    no_default_features: bool,
    package: Option<&str>,
    extra_args: &[String],
) -> Result<(), DevError> {
    let mut args = vec!["test"];
    if no_default_features {
        args.push("--no-default-features");
    }
    let joined = features.join(",");
    if !features.is_empty() {
        args.push("--features");
        args.push(&joined);
    } else if !no_default_features {
        args.push("--all-features");
    }
    if let Some(pkg) = package {
        args.push("--package");
        args.push(pkg);
    }
    if !extra_args.is_empty() {
        let extra_refs: Vec<&str> = extra_args.iter().map(String::as_str).collect();
        args.extend_from_slice(&extra_refs);
    }

    output::run_msg(&format!("cargo {}", args.join(" ")));

    // Nidhogg tests need CARGO_TARGET_TMPDIR set.
    let env: Vec<(&str, &str)> = match project {
        Project::Nidhogg => vec![("CARGO_TARGET_TMPDIR", "target/tmp")],
        _ => vec![],
    };

    let captured = if env.is_empty() {
        output::run_captured("cargo", &args, project_root)?
    } else {
        output::run_captured_with_env("cargo", &args, project_root, &env)?
    };

    if !captured.status.success() {
        let stdout = String::from_utf8_lossy(&captured.stdout);
        let stderr = String::from_utf8_lossy(&captured.stderr);
        // Print stderr (compilation warnings) first, stdout (test results) last
        // so the actionable failure details appear at the bottom of output.
        if !stderr.is_empty() {
            output::error(&stderr);
        }
        if !stdout.is_empty() {
            output::error(&stdout);
        }
        return Err(DevError::Build("tests failed".into()));
    }

    Ok(())
}

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

    // Ensure scratch dir exists — binary commands often write output there.
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

fn open_sidecar_db(project_root: &Path) -> Option<db::sidecar::SidecarDb> {
    let path = resolve::sidecar_db_path(project_root);
    if path.exists() {
        db::sidecar::SidecarDb::open(&path).ok()
    } else {
        None
    }
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn cmd_results(project_root: &Path, q: &ResultsQuery) -> Result<(), DevError> {
    let db_path = results_db_path(project_root);

    if !db_path.exists() {
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }

    let results_db = db::ResultsDb::open(&db_path)?;
    let sidecar_db = open_sidecar_db(project_root);

    // --compare-timeline <uuid_a> <uuid_b>
    if let Some(ref uuids) = q.compare_timeline {
        let Some(ref sdb) = sidecar_db else {
            output::result_msg("no sidecar.db found");
            return Ok(());
        };
        let uuid_a = &uuids[0];
        let uuid_b = &uuids[1];
        let (best_a, _) = sdb.query_meta(uuid_a);
        let (best_b, _) = sdb.query_meta(uuid_b);
        let samples_a = sdb.query_samples(uuid_a, Some(best_a))?;
        let samples_b = sdb.query_samples(uuid_b, Some(best_b))?;
        if samples_a.is_empty() || samples_b.is_empty() {
            output::result_msg("one or both results have no sidecar data");
            return Ok(());
        }
        let markers_a = sdb.query_markers(uuid_a, Some(best_a))?;
        let markers_b = sdb.query_markers(uuid_b, Some(best_b))?;
        print_compare_timeline(
            uuid_a, &samples_a, &markers_a, uuid_b, &samples_b, &markers_b,
        );
        return Ok(());
    }

    if let Some(ref uuid_prefix) = q.query {
        // Sidecar output modes.
        if q.timeline {
            let Some(ref sdb) = sidecar_db else {
                output::result_msg("no sidecar.db found");
                return Ok(());
            };

            // Resolve --run: "all" → None (all runs), N → Some(N), absent → best run.
            let (best_idx, total) = sdb.query_meta(uuid_prefix);
            let run_filter = match q.run.as_deref() {
                Some("all") => None,
                Some(n) => Some(n.parse::<usize>().map_err(|_| {
                    DevError::Config(format!("--run: expected a number or 'all', got '{n}'"))
                })?),
                None => Some(best_idx),
            };
            if total > 1 {
                let showing = match run_filter {
                    Some(idx) => format!("run {idx}/{total}"),
                    None => format!("all {total} runs"),
                };
                output::sidecar_msg(&format!("showing {showing} (use --run to override)"));
            }

            let mut samples = sdb.query_samples(uuid_prefix, run_filter)?;
            if samples.is_empty() {
                output::result_msg("no sidecar data for this result");
            } else if q.summary {
                let markers = sdb.query_markers(uuid_prefix, run_filter)?;
                print_phase_summary(&samples, &markers);
            } else {
                if let Some(ref phase_name) = q.phase {
                    let markers = sdb.query_markers(uuid_prefix, run_filter)?;
                    let (start_us, end_us) = resolve_phase_range(phase_name, &markers, &samples)?;
                    samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
                }

                if let Some(ref range_str) = q.range {
                    let (start_us, end_us) = parse_time_range(range_str)?;
                    samples.retain(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us);
                }

                if let Some(ref field) = q.stat {
                    let filtered = apply_timeline_filter(&samples, q);
                    print_field_stat(&filtered, field)?;
                } else {
                    let filtered = apply_timeline_filter(&samples, q);
                    let fields = if q.fields.is_empty() {
                        None
                    } else {
                        Some(&q.fields)
                    };
                    for s in &filtered {
                        println!("{}", sidecar_sample_json_projected(s, fields));
                    }
                }
            }
            return Ok(());
        }
        if q.markers {
            let Some(ref sdb) = sidecar_db else {
                output::result_msg("no sidecar.db found");
                return Ok(());
            };
            let (best_idx, total) = sdb.query_meta(uuid_prefix);
            let run_filter = match q.run.as_deref() {
                Some("all") => None,
                Some(n) => Some(n.parse::<usize>().map_err(|_| {
                    DevError::Config(format!("--run: expected a number or 'all', got '{n}'"))
                })?),
                None => Some(best_idx),
            };
            if total > 1 {
                let showing = match run_filter {
                    Some(idx) => format!("run {idx}/{total}"),
                    None => format!("all {total} runs"),
                };
                output::sidecar_msg(&format!("showing {showing} (use --run to override)"));
            }
            let markers = sdb.query_markers(uuid_prefix, run_filter)?;
            if q.counters {
                let counters = sdb.query_counters(uuid_prefix, run_filter)?;
                if counters.is_empty() {
                    output::result_msg("no counters for this result");
                } else {
                    print_counters(&counters);
                }
                return Ok(());
            }
            if markers.is_empty() {
                output::result_msg("no sidecar markers for this result");
            } else if q.phases {
                let samples = sdb.query_samples(uuid_prefix, run_filter)?;
                let counters = sdb.query_counters(uuid_prefix, run_filter)?;
                print_marker_phases_with_counters(&markers, &samples, &counters);
            } else if q.durations {
                print_marker_durations(&markers);
            } else {
                for m in &markers {
                    println!("{}", sidecar_marker_json(m));
                }
            }
            return Ok(());
        }

        let rows = results_db.query_by_uuid(uuid_prefix)?;
        if rows.is_empty() {
            output::result_msg("no matching results");
        } else {
            let table = db::format_table(&rows);
            println!("{table}");
            // Show detail fields and hotpath report for UUID lookups.
            for row in &rows {
                let details = db::format_details(row);
                if !details.is_empty() {
                    println!("\n{details}");
                }
                if let Some(ref hotpath) = row.hotpath
                    && let Some(report) = hotpath_fmt::format_hotpath_report(hotpath, q.top)
                {
                    println!("\n{report}");
                }
                // Note if sidecar data exists.
                if sidecar_db
                    .as_ref()
                    .is_some_and(|sdb| sdb.has_data(&row.uuid))
                {
                    output::sidecar_msg("profile data available (use --timeline or --markers)");
                }
            }
        }
    } else if let Some(ref commits) = q.compare {
        let commit_a = commits.first().map_or("", String::as_str);
        let commit_b = commits.get(1).map_or("", String::as_str);
        let (rows_a, rows_b) = results_db.query_compare(
            commit_a,
            commit_b,
            q.command.as_deref(),
            q.variant.as_deref(),
            q.dataset.as_deref(),
        )?;
        let table = db::format_compare(commit_a, &rows_a, commit_b, &rows_b, q.top);
        println!("{table}");
    } else if q.compare_last {
        match results_db.query_compare_last(
            q.command.as_deref(),
            q.variant.as_deref(),
            q.dataset.as_deref(),
        )? {
            Some((commit_a, rows_a, commit_b, rows_b)) => {
                let table = db::format_compare(&commit_a, &rows_a, &commit_b, &rows_b, q.top);
                println!("{table}");
            }
            None => {
                output::result_msg("need at least two distinct commits to compare");
            }
        }
    } else {
        // Parse --meta KEY=VALUE strings into (key, value) pairs. The CLI
        // validator already guarantees each entry contains '=', so split_once
        // can't fail here, but we still defensively pattern-match.
        let meta_pairs: Vec<(String, String)> = q
            .meta
            .iter()
            .filter_map(|s| s.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
            .collect();
        let filter = db::QueryFilter {
            commit: q.commit.clone(),
            command: q.command.clone(),
            variant: q.variant.clone(),
            dataset: q.dataset.clone(),
            meta: meta_pairs,
            limit: q.limit,
        };
        let rows = results_db.query(&filter)?;
        if rows.is_empty() {
            output::result_msg("no matching results");
        } else {
            let table = db::format_table(&rows);
            println!("{table}");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Timeline query helpers
// ---------------------------------------------------------------------------

/// Resolve a phase name to a (start_us, end_us) range from markers.
///
/// Matches by:
/// 1. Exact marker name (e.g. "STAGE2_START" → that marker to the next)
/// 2. Base name (e.g. "STAGE2" → STAGE2_START to STAGE2_END)
/// 3. Substring match on marker name
fn resolve_phase_range(
    phase: &str,
    markers: &[sidecar::Marker],
    samples: &[sidecar::Sample],
) -> Result<(i64, i64), DevError> {
    let final_us = samples.last().map_or(0, |s| s.timestamp_us + 1);

    // Try exact match first.
    if let Some(idx) = markers.iter().position(|m| m.name == phase) {
        let start = markers[idx].timestamp_us;
        let end = markers.get(idx + 1).map_or(final_us, |m| m.timestamp_us);
        return Ok((start, end));
    }

    // Try base name: phase "STAGE2" matches "STAGE2_START".
    let start_name = format!("{phase}_START");
    let end_name = format!("{phase}_END");
    if let Some(start_idx) = markers.iter().position(|m| m.name == start_name) {
        let start = markers[start_idx].timestamp_us;
        let end = markers
            .iter()
            .find(|m| m.name == end_name)
            .map_or(final_us, |m| m.timestamp_us);
        return Ok((start, end));
    }

    // Try substring match.
    if let Some(idx) = markers.iter().position(|m| m.name.contains(phase)) {
        let start = markers[idx].timestamp_us;
        let end = markers.get(idx + 1).map_or(final_us, |m| m.timestamp_us);
        return Ok((start, end));
    }

    let available: Vec<&str> = markers.iter().map(|m| m.name.as_str()).collect();
    Err(DevError::Config(format!(
        "--phase: no marker matching '{phase}'. Available: {available:?}"
    )))
}

/// Parse a time range string like "10.0..82.0" (seconds) into (start_us, end_us).
fn parse_time_range(range: &str) -> Result<(i64, i64), DevError> {
    let (start_str, end_str) = range.split_once("..").ok_or_else(|| {
        DevError::Config(format!(
            "--range: expected 'start..end' in seconds, got '{range}'"
        ))
    })?;

    let start_sec: f64 = start_str.trim().parse().map_err(|_| {
        DevError::Config(format!(
            "--range: cannot parse start '{start_str}' as number"
        ))
    })?;
    let end_sec: f64 = end_str.trim().parse().map_err(|_| {
        DevError::Config(format!("--range: cannot parse end '{end_str}' as number"))
    })?;

    #[allow(clippy::cast_possible_truncation)]
    let start_us = (start_sec * 1_000_000.0) as i64;
    #[allow(clippy::cast_possible_truncation)]
    let end_us = (end_sec * 1_000_000.0) as i64;

    Ok((start_us, end_us))
}

/// All known sample field names and their accessor functions.
fn sample_field_value(s: &sidecar::Sample, field: &str) -> Option<i64> {
    match field {
        "i" => Some(i64::from(s.sample_idx)),
        "rss" => Some(s.rss_kb),
        "anon" => Some(s.anon_kb),
        "file" => Some(s.file_kb),
        "shmem" => Some(s.shmem_kb),
        "swap" => Some(s.swap_kb),
        "vsize" => Some(s.vsize_kb),
        "hwm" => Some(s.vm_hwm_kb),
        "utime" => Some(s.utime),
        "stime" => Some(s.stime),
        "threads" => Some(s.num_threads),
        "minflt" => Some(s.minflt),
        "majflt" => Some(s.majflt),
        "rchar" => Some(s.rchar),
        "wchar" => Some(s.wchar),
        "rd" => Some(s.read_bytes),
        "wr" => Some(s.write_bytes),
        "cwr" => Some(s.cancelled_write_bytes),
        "syscr" => Some(s.syscr),
        "syscw" => Some(s.syscw),
        "vcs" => Some(s.vol_cs),
        "nvcs" => Some(s.nonvol_cs),
        _ => None,
    }
}

/// Parse a --where condition like "majflt>0" or "anon>100000".
///
/// Returns (field, op, threshold). Supported ops: >, <, >=, <=, ==, !=.
fn parse_where_cond(cond: &str) -> Result<(&str, &str, i64), DevError> {
    // Try two-char operators first, then single-char.
    for op in &[">=", "<=", "==", "!=", ">", "<"] {
        if let Some(pos) = cond.find(op) {
            let field = cond[..pos].trim();
            let val_str = cond[pos + op.len()..].trim();
            let val: i64 = val_str.parse().map_err(|_| {
                DevError::Config(format!("--where: cannot parse '{val_str}' as integer"))
            })?;
            return Ok((field, op, val));
        }
    }
    Err(DevError::Config(format!(
        "--where: invalid condition '{cond}' (expected e.g. 'majflt>0')"
    )))
}

/// Apply --where, --every, --head, --tail filters to a sample list.
fn apply_timeline_filter<'a>(
    samples: &'a [sidecar::Sample],
    q: &ResultsQuery,
) -> Vec<&'a sidecar::Sample> {
    let mut result: Vec<&sidecar::Sample> = samples.iter().collect();

    // --where filter
    if let Some(ref cond) = q.where_cond
        && let Ok((field, op, threshold)) = parse_where_cond(cond)
    {
        result.retain(|s| {
            if let Some(val) = sample_field_value(s, field) {
                match op {
                    ">" => val > threshold,
                    "<" => val < threshold,
                    ">=" => val >= threshold,
                    "<=" => val <= threshold,
                    "==" => val == threshold,
                    "!=" => val != threshold,
                    _ => true,
                }
            } else {
                false
            }
        });
    }

    // --every N (downsample)
    if let Some(n) = q.every
        && n > 1
    {
        result = result.into_iter().step_by(n).collect();
    }

    // --tail N (take last N before head, so --tail 100 --head 10 = last 100 then first 10 of those)
    if let Some(n) = q.tail {
        let len = result.len();
        if n < len {
            result = result.split_off(len - n);
        }
    }

    // --head N
    if let Some(n) = q.head {
        result.truncate(n);
    }

    result
}

/// Print min/max/avg/p50/p95 for a field across the given samples.
fn print_field_stat(samples: &[&sidecar::Sample], field: &str) -> Result<(), DevError> {
    let mut values: Vec<i64> = samples
        .iter()
        .filter_map(|s| sample_field_value(s, field))
        .collect();

    if values.is_empty() {
        return Err(DevError::Config(format!(
            "unknown field '{field}' or no samples"
        )));
    }

    values.sort_unstable();
    let n = values.len();

    #[allow(clippy::cast_precision_loss)]
    let avg = values.iter().sum::<i64>() as f64 / n as f64;
    let min = values[0];
    let max = values[n - 1];

    // Linear interpolation percentiles (same as harness::percentile).
    let pct = |p: usize| -> i64 {
        if n == 1 {
            return values[0];
        }
        #[allow(clippy::cast_precision_loss)]
        let pos = (p as f64 / 100.0) * (n - 1) as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lo = pos as usize;
        let hi = (lo + 1).min(n - 1);
        #[allow(clippy::cast_precision_loss)]
        let frac = pos - lo as f64;
        #[allow(clippy::cast_precision_loss)]
        let result = values[lo] as f64 + frac * (values[hi] - values[lo]) as f64;
        #[allow(clippy::cast_possible_truncation)]
        {
            result.round() as i64
        }
    };

    println!("field    {field}");
    println!("samples  {n}");
    println!("min      {min}");
    println!("max      {max}");
    println!("avg      {avg:.1}");
    println!("p50      {}", pct(50));
    println!("p95      {}", pct(95));
    Ok(())
}

/// Format a sample as JSONL, optionally projecting only selected fields.
///
/// `t` is output as fractional seconds (e.g. `1.234`) not microseconds.
/// When `fields` is `None`, all fields are output. When `Some`, only the
/// listed fields are included (plus `t` is always included).
fn sidecar_sample_json_projected(s: &sidecar::Sample, fields: Option<&Vec<String>>) -> String {
    // t is always fractional seconds.
    #[allow(clippy::cast_precision_loss)]
    let t_sec = s.timestamp_us as f64 / 1_000_000.0;

    match fields {
        None => {
            // All fields.
            format!(
                concat!(
                    "{{",
                    "\"t\":{:.3},",
                    "\"rss\":{},",
                    "\"anon\":{},",
                    "\"file\":{},",
                    "\"shmem\":{},",
                    "\"swap\":{},",
                    "\"vsize\":{},",
                    "\"hwm\":{},",
                    "\"utime\":{},",
                    "\"stime\":{},",
                    "\"threads\":{},",
                    "\"minflt\":{},",
                    "\"majflt\":{},",
                    "\"rchar\":{},",
                    "\"wchar\":{},",
                    "\"rd\":{},",
                    "\"wr\":{},",
                    "\"cwr\":{},",
                    "\"syscr\":{},",
                    "\"syscw\":{},",
                    "\"vcs\":{},",
                    "\"nvcs\":{}",
                    "}}",
                ),
                t_sec,
                s.rss_kb,
                s.anon_kb,
                s.file_kb,
                s.shmem_kb,
                s.swap_kb,
                s.vsize_kb,
                s.vm_hwm_kb,
                s.utime,
                s.stime,
                s.num_threads,
                s.minflt,
                s.majflt,
                s.rchar,
                s.wchar,
                s.read_bytes,
                s.write_bytes,
                s.cancelled_write_bytes,
                s.syscr,
                s.syscw,
                s.vol_cs,
                s.nonvol_cs,
            )
        }
        Some(field_list) => {
            // Projected: only requested fields + always t.
            let mut parts: Vec<String> = Vec::with_capacity(field_list.len() + 1);
            parts.push(format!("\"t\":{t_sec:.3}"));
            for f in field_list {
                if f == "t" {
                    continue; // already included
                }
                if let Some(val) = sample_field_value(s, f) {
                    parts.push(format!("\"{f}\":{val}"));
                }
            }
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// Format a sidecar marker as a compact JSON object (single line).
/// `t` is fractional seconds.
fn sidecar_marker_json(m: &sidecar::Marker) -> String {
    #[allow(clippy::cast_precision_loss)]
    let t_sec = m.timestamp_us as f64 / 1_000_000.0;
    let name = m
        .name
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!(
        "{{\"i\":{},\"t\":{t_sec:.3},\"name\":\"{}\"}}",
        m.marker_idx, name,
    )
}

/// Print per-phase summary table from sidecar samples and markers.
///
/// Each marker defines a phase boundary. The phase runs from the marker's
/// timestamp up to (but not including) the next marker's timestamp. The
/// last phase runs to the final sample. Shows duration, peak RSS, peak
/// anon RSS, and disk I/O deltas per phase.
///
/// If there are no markers, treats the entire run as a single phase.
fn print_phase_summary(samples: &[sidecar::Sample], markers: &[sidecar::Marker]) {
    // Build phase boundaries: [start_us, end_us) — exclusive end to avoid
    // double-counting samples at boundaries.
    let mut phases: Vec<(&str, i64, i64)> = Vec::new(); // (name, start_us, end_us)

    if markers.is_empty() {
        if let (Some(first), Some(last)) = (samples.first(), samples.last()) {
            // Single phase — inclusive end since there's no next phase to overlap.
            phases.push(("(all)", first.timestamp_us, last.timestamp_us + 1));
        }
    } else {
        let final_us = samples.last().map_or(0, |s| s.timestamp_us + 1);
        for (i, m) in markers.iter().enumerate() {
            let phase_end = markers
                .get(i + 1)
                .map_or(final_us, |next| next.timestamp_us);
            phases.push((&m.name, m.timestamp_us, phase_end));
        }
    }

    println!(
        "{:<24} {:>8} {:>10} {:>10} {:>12} {:>12}",
        "Phase", "Duration", "Peak RSS", "Peak Anon", "Disk Read", "Disk Write",
    );
    println!("{}", "-".repeat(81));

    for (name, start_us, end_us) in &phases {
        // Samples in [start_us, end_us).
        let mut peak_rss: i64 = 0;
        let mut peak_anon: i64 = 0;
        let mut first_io: Option<(i64, i64)> = None;
        let mut last_io: (i64, i64) = (0, 0);
        let mut count = 0;

        for s in samples
            .iter()
            .filter(|s| s.timestamp_us >= *start_us && s.timestamp_us < *end_us)
        {
            if s.rss_kb > peak_rss {
                peak_rss = s.rss_kb;
            }
            if s.anon_kb > peak_anon {
                peak_anon = s.anon_kb;
            }
            if first_io.is_none() {
                first_io = Some((s.read_bytes, s.write_bytes));
            }
            last_io = (s.read_bytes, s.write_bytes);
            count += 1;
        }

        if count == 0 {
            println!("{name:<24} {:>8}", "(no samples)");
            continue;
        }

        let duration_ms = (end_us - start_us) / 1_000;
        let (first_rd, first_wr) = first_io.unwrap_or((0, 0));
        let disk_read = last_io.0 - first_rd;
        let disk_write = last_io.1 - first_wr;

        println!(
            "{name:<24} {:>6}ms {:>7} kB {:>7} kB {:>9} kB {:>9} kB",
            duration_ms,
            peak_rss,
            peak_anon,
            disk_read / 1024,
            disk_write / 1024,
        );
    }
}

/// Print duration between _START/_END marker pairs.
///
/// Matches markers by stripping the `_START`/`_END` suffix to find pairs.
/// For unpaired markers (standalone), prints the timestamp only.
fn print_marker_durations(markers: &[sidecar::Marker]) {
    // Build a map of base_name -> (start_us, end_us).
    let mut pairs: Vec<(String, i64, Option<i64>)> = Vec::new();

    // Index of consumed markers (to avoid double-counting).
    let mut consumed = vec![false; markers.len()];

    for (i, m) in markers.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        if let Some(base) = m.name.strip_suffix("_START") {
            consumed[i] = true;
            let end_name = format!("{base}_END");
            // Find the matching END.
            let end_us = markers[i + 1..]
                .iter()
                .enumerate()
                .find(|(_, m2)| m2.name == end_name)
                .map(|(j, m2)| {
                    consumed[i + 1 + j] = true;
                    m2.timestamp_us
                });
            pairs.push((base.to_owned(), m.timestamp_us, end_us));
        }
    }

    // Print standalone markers that weren't consumed.
    let mut standalone: Vec<&sidecar::Marker> = Vec::new();
    for (i, m) in markers.iter().enumerate() {
        if !consumed[i] {
            standalone.push(m);
        }
    }

    if !pairs.is_empty() {
        println!(
            "{:<32} {:>12} {:>12} {:>12}",
            "Phase", "Start", "End", "Duration"
        );
        println!("{}", "-".repeat(71));
        for (name, start_us, end_us) in &pairs {
            match end_us {
                Some(end) => {
                    let dur_ms = (end - start_us) / 1_000;
                    let start_ms = start_us / 1_000;
                    let end_ms = end / 1_000;
                    println!("{name:<32} {start_ms:>9}ms {end_ms:>9}ms {dur_ms:>9}ms");
                }
                None => {
                    let start_ms = start_us / 1_000;
                    println!(
                        "{name:<32} {:>9}ms {:>12} {:>12}",
                        start_ms, "(no end)", "—"
                    );
                }
            }
        }
    }

    if !standalone.is_empty() {
        if !pairs.is_empty() {
            println!();
        }
        println!("Standalone markers:");
        for m in &standalone {
            let ms = m.timestamp_us / 1_000;
            println!("  {:<32} {:>9}ms", m.name, ms);
        }
    }
}

/// Print phase-aligned comparison of two sidecar timelines.
///
/// For each phase (defined by markers in run A), shows duration, peak anon RSS,
/// total disk read, and the delta between the two runs.
fn print_compare_timeline(
    uuid_a: &str,
    samples_a: &[sidecar::Sample],
    markers_a: &[sidecar::Marker],
    uuid_b: &str,
    samples_b: &[sidecar::Sample],
    markers_b: &[sidecar::Marker],
) {
    // Build phases from run A's markers (or all markers if A has none).
    let phases_a = build_phases(markers_a, samples_a);
    let phases_b = build_phases(markers_b, samples_b);

    let short_a = &uuid_a[..8.min(uuid_a.len())];
    let short_b = &uuid_b[..8.min(uuid_b.len())];

    println!(
        "{:<20} {:>22} {:>22} {:>8}",
        "Phase",
        format!("Run A ({short_a})"),
        format!("Run B ({short_b})"),
        "Delta",
    );
    println!("{}", "-".repeat(76));

    for (name, start_a, end_a) in &phases_a {
        let stats_a = phase_stats(samples_a, *start_a, *end_a);

        // Find matching phase in B by name.
        let stats_b = phases_b
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, start, end)| phase_stats(samples_b, *start, *end));

        let dur_a = (end_a - start_a) / 1_000;

        match stats_b {
            Some(sb) => {
                let dur_b = phases_b
                    .iter()
                    .find(|(n, _, _)| n == name)
                    .map(|(_, s, e)| (e - s) / 1_000)
                    .unwrap_or(0);

                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let delta_pct = if dur_a > 0 {
                    ((dur_b - dur_a) as f64 / dur_a as f64 * 100.0) as i64
                } else {
                    0
                };

                println!(
                    "{:<20} {:>5}ms {:>6}kB {:>5}MB  {:>5}ms {:>6}kB {:>5}MB  {:>+5}%",
                    name,
                    dur_a,
                    stats_a.peak_anon,
                    stats_a.disk_read_kb / 1024,
                    dur_b,
                    sb.peak_anon,
                    sb.disk_read_kb / 1024,
                    delta_pct,
                );
            }
            None => {
                println!(
                    "{:<20} {:>5}ms {:>6}kB {:>5}MB  {:>22} {:>8}",
                    name,
                    dur_a,
                    stats_a.peak_anon,
                    stats_a.disk_read_kb / 1024,
                    "(no match)",
                    "—",
                );
            }
        }
    }
}

struct PhaseStats {
    peak_anon: i64,
    disk_read_kb: i64,
}

fn phase_stats(samples: &[sidecar::Sample], start_us: i64, end_us: i64) -> PhaseStats {
    let mut peak_anon: i64 = 0;
    let mut first_rd: Option<i64> = None;
    let mut last_rd: i64 = 0;

    for s in samples
        .iter()
        .filter(|s| s.timestamp_us >= start_us && s.timestamp_us < end_us)
    {
        if s.anon_kb > peak_anon {
            peak_anon = s.anon_kb;
        }
        if first_rd.is_none() {
            first_rd = Some(s.read_bytes);
        }
        last_rd = s.read_bytes;
    }

    PhaseStats {
        peak_anon,
        disk_read_kb: (last_rd - first_rd.unwrap_or(0)) / 1024,
    }
}

/// Build phase boundaries from markers (or single "(all)" phase if no markers).
fn build_phases<'a>(
    markers: &'a [sidecar::Marker],
    samples: &[sidecar::Sample],
) -> Vec<(&'a str, i64, i64)> {
    let mut phases = Vec::new();
    if markers.is_empty() {
        if let (Some(first), Some(last)) = (samples.first(), samples.last()) {
            // Use a static str for the lifetime.
            phases.push(("(all)" as &str, first.timestamp_us, last.timestamp_us + 1));
        }
    } else {
        let final_us = samples.last().map_or(0, |s| s.timestamp_us + 1);
        for (i, m) in markers.iter().enumerate() {
            let phase_end = markers
                .get(i + 1)
                .map_or(final_us, |next| next.timestamp_us);
            phases.push((m.name.as_str(), m.timestamp_us, phase_end));
        }
    }
    phases
}

/// Print START/END marker pairs with duration + peak RSS and majflt from samples.
/// Print counters as a simple list.
fn print_counters(counters: &[sidecar::Counter]) {
    for c in counters {
        #[allow(clippy::cast_precision_loss)]
        let t_sec = c.timestamp_us as f64 / 1_000_000.0;
        println!("t={t_sec:<10.3} {}={}", c.name, c.value);
    }
}

/// Print START/END marker pairs with duration, peak RSS/anon/majflt, and optional counters.
fn print_marker_phases_with_counters(
    markers: &[sidecar::Marker],
    samples: &[sidecar::Sample],
    counters: &[sidecar::Counter],
) {
    let has_counters = !counters.is_empty();

    // Pair START/END markers.
    let mut pairs: Vec<(String, i64, Option<i64>)> = Vec::new();
    let mut consumed = vec![false; markers.len()];

    for (i, m) in markers.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        if let Some(base) = m.name.strip_suffix("_START") {
            consumed[i] = true;
            let end_name = format!("{base}_END");
            let end_us = markers[i + 1..]
                .iter()
                .enumerate()
                .find(|(_, m2)| m2.name == end_name)
                .map(|(j, m2)| {
                    consumed[i + 1 + j] = true;
                    m2.timestamp_us
                });
            pairs.push((base.to_owned(), m.timestamp_us, end_us));
        }
    }

    if pairs.is_empty() {
        output::result_msg("no _START/_END marker pairs found");
        return;
    }

    if has_counters {
        println!(
            "{:<24} {:>10} {:>10} {:>10} {:>10}  Counters",
            "Phase", "Duration", "Peak RSS", "Peak Anon", "Peak Mflt",
        );
        println!("{}", "-".repeat(90));
    } else {
        println!(
            "{:<24} {:>10} {:>10} {:>10} {:>10}",
            "Phase", "Duration", "Peak RSS", "Peak Anon", "Peak Mflt",
        );
        println!("{}", "-".repeat(68));
    }

    for (name, start_us, end_us) in &pairs {
        let end = end_us.unwrap_or_else(|| {
            samples.last().map_or(*start_us, |s| s.timestamp_us + 1)
        });
        let dur_ms = (end - start_us) / 1_000;

        let mut peak_rss: i64 = 0;
        let mut peak_anon: i64 = 0;
        let mut peak_majflt: i64 = 0;
        let mut prev_majflt: Option<i64> = None;

        for s in samples
            .iter()
            .filter(|s| s.timestamp_us >= *start_us && s.timestamp_us < end)
        {
            if s.rss_kb > peak_rss {
                peak_rss = s.rss_kb;
            }
            if s.anon_kb > peak_anon {
                peak_anon = s.anon_kb;
            }
            if let Some(prev) = prev_majflt {
                let delta = s.majflt - prev;
                if delta > peak_majflt {
                    peak_majflt = delta;
                }
            }
            prev_majflt = Some(s.majflt);
        }

        let end_marker = if end_us.is_some() { "" } else { " (no end)" };

        if has_counters {
            let phase_counters: Vec<&sidecar::Counter> = counters
                .iter()
                .filter(|c| c.timestamp_us >= *start_us && c.timestamp_us <= end)
                .collect();

            let counter_str = phase_counters
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join(", ");

            println!(
                "{:<24} {:>7}ms {:>7}kB {:>7}kB {:>10}  {counter_str}",
                format!("{name}{end_marker}"),
                dur_ms,
                peak_rss,
                peak_anon,
                peak_majflt,
            );
        } else {
            println!(
                "{:<24} {:>7}ms {:>7}kB {:>7}kB {:>10}",
                format!("{name}{end_marker}"),
                dur_ms,
                peak_rss,
                peak_anon,
                peak_majflt,
            );
        }
    }
}

fn cmd_clean(
    dev_config: &config::DevConfig,
    project: Project,
    project_root: &Path,
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
            // Elivagar scratch is tilegen_tmp — remove all contents.
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

    output::result_msg("clean done");
    Ok(())
}

fn cmd_lock() -> Result<(), DevError> {
    match lockfile::status()? {
        Some(info) => {
            output::lock_msg(&format!(
                "held by PID {}: {} {} ({})",
                info.pid, info.project, info.command, info.project_root,
            ));
            if info.pid > 0 {
                if let Some(summary) = lockfile::process_summary(info.pid) {
                    output::lock_msg(&summary);
                }
                // Show the last sidecar marker if available.
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
        }
        None => {
            output::lock_msg("no active lock");
        }
    }
    Ok(())
}

#[allow(clippy::fn_params_excessive_bools)]
fn cmd_history(
    command: Option<String>,
    project: Option<String>,
    failed: bool,
    since: Option<String>,
    slow: Option<i64>,
    limit: usize,
    all: bool,
) -> Result<(), DevError> {
    let db = history::HistoryDb::open()?;
    let filter = history::HistoryFilter {
        command,
        project,
        failed,
        since,
        slow_ms: slow,
        limit,
        all,
    };
    let entries = db.query(&filter)?;
    let output = history::format_history(&entries);
    println!("{output}");
    Ok(())
}

/// Best-effort recording of command history. Warns once on failure.
fn record_history(raw_args: &str, elapsed_ms: u64, exit_code: i32) {
    let inner = || -> Result<(), error::DevError> {
        let db = history::HistoryDb::open()?;

        // Best-effort metadata collection. Each item can fail independently.
        let hostname = config::hostname().unwrap_or_else(|_| "unknown".into());
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into());

        // Try to detect project and git info — these are optional.
        let (project_name, commit_hash, dirty) = match project::detect() {
            Ok((project, _config, project_root)) => match git::collect(&project_root) {
                Ok(gi) => (
                    Some(project.name().to_owned()),
                    if gi.commit.is_empty() {
                        None
                    } else {
                        Some(gi.commit)
                    },
                    Some(!gi.is_clean),
                ),
                Err(_) => (Some(project.name().to_owned()), None, None),
            },
            Err(_) => (None, None, None),
        };

        let kernel = std::fs::read_to_string("/proc/version")
            .ok()
            .and_then(|s| s.split_whitespace().nth(2).map(String::from));
        let (_, avail) = env::read_memory();
        let avail_memory_mb = i64::try_from(avail).ok();

        #[allow(clippy::cast_possible_wrap)]
        let elapsed = elapsed_ms as i64;

        db.insert(&history::HistoryRow {
            project: project_name,
            cwd,
            command: raw_args.to_owned(),
            elapsed_ms: elapsed,
            exit_status: exit_code,
            hostname,
            commit_hash,
            dirty,
            kernel,
            avail_memory_mb,
        })?;
        Ok(())
    };

    if let Err(e) = inner() {
        eprintln!("[history] warning: failed to write history: {e}");
    }
}

fn cmd_pmtiles_stats(files: &[String]) -> Result<(), DevError> {
    for file in files {
        pmtiles::run(file)?;
    }
    Ok(())
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
    )?;

    let alloc = req.is_alloc();
    let label = harness::hotpath_feature(alloc);
    output::hotpath_msg(&format!("=== {} {label} ===", req.project));

    if alloc {
        output::hotpath_msg("NOTE: alloc profiling -- wall-clock times are not meaningful");
    }

    let variant_suffix = harness::hotpath_variant_suffix(alloc);
    let variant = format!("default{variant_suffix}");

    let binary_str = ctx.binary.display().to_string();

    let config = harness::BenchConfig {
        command: "hotpath".into(),
        variant: Some(variant),
        input_file: None,
        input_mb: None,
        cargo_features: None,
        cargo_profile: "release".into(),
        runs: req.runs(),
        cli_args: Some(harness::format_cli_args(&binary_str, &[])),
        metadata: vec![db::KvPair::text("meta.alloc", alloc.to_string())],
    };

    ctx.harness.run_internal(&config, |_i| {
        let (result, _stderr, _sidecar) = harness::run_hotpath_capture(
            &binary_str,
            &[],
            &ctx.paths.scratch_dir,
            req.project_root,
            &[],
            &[],
        )?;
        Ok(result)
    })?;

    Ok(())
}
