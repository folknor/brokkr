// Clippy's `restriction` lints (unwrap_used, too_many_lines, etc.) are useful
// for library code but noisy in tests where `.unwrap()` on a fixture and
// long table-driven test functions are idiomatic. Allow them in the test
// compilation only; the main binary still gets the full strict lint set.
#![cfg_attr(test, allow(
    clippy::unwrap_used,
    clippy::unwrap_in_result,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::approx_constant,
    clippy::needless_pass_by_value,
    clippy::let_underscore_must_use,
    clippy::useless_vec,
))]

mod build;
mod cargo_filter;
mod cargo_json;
mod check_cmd;
mod cli;
mod config;
mod context;
mod db;
mod elivagar;
mod env;
mod error;
mod git;
mod harness;
mod history;
mod history_cmd;
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
mod results_cmd;
mod sidecar;
mod sidecar_cmd;
mod sidecar_fmt;
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
use request::{ResultsQuery, SidecarQuery};

/// Shared setup for all measured commands: resolve mode/features, set quiet,
/// handle worktree, construct `MeasureRequest`, call the provided closure.
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
            brokkr_args,
            no_mem_check: mode.no_mem_check,
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
        Err(_) => 1,
    };

    if !is_history {
        history_cmd::record_history(&raw_args, elapsed_ms, exit_code);
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
        return history_cmd::cmd_history(command, project, failed, since, slow, limit, all);
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
        | Command::MultiExtract { .. }
        | Command::TimeFilter { .. }
        | Command::Diff { .. }
        | Command::BuildGeocodeIndex { .. } => unreachable!(),
        Command::Check {
            features,
            no_default_features,
            package,
            raw,
            json,
            args,
        } => check_cmd::cmd_check(
            project,
            &project_root,
            &features,
            no_default_features,
            package.as_deref(),
            raw,
            json,
            &args,
        ),
        Command::Env => cmd_env(&dev_config, project, &project_root),
        Command::DiffSnapshots {
            mode,
            dataset,
            from,
            to,
            variant,
            format,
        } => {
            let params = crate::measure::CommandParams {
                from_snapshot: Some(from.clone()),
                to_snapshot: Some(to.clone()),
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
        } => {
            let params = crate::measure::CommandParams {
                bbox: bbox.clone(),
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
                grep,
                limit,
                top,
            };
            results_cmd::cmd_results(&project_root, &rq)
        }
        Command::Sidecar {
            query,
            samples,
            markers,
            durations,
            counters,
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
                brokkr_args: &brokkr_args,
                no_mem_check,
                wait,
                // Sluggrs hotpath uses Command::Hotpath, not ModeArgs — no
                // dry-run or stop-marker surface to plumb from.
                dry_run: false,
                stop_marker: None,
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
        )?;
        Ok(result)
    })?;

    Ok(())
}
