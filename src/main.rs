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

use cli::{Cli, Command, LitehtmlCommand, SluggrsCommand, VerifyCommand};
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
            runs: mm.runs(),
            features: &features,
            force: mode.force,
            mode: mm,
            no_mem_check: mode.no_mem_check,
            sidecar: mode.sidecar,
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
    if let Some((mode, pbf, pbf_cmd, osc, params)) = cli.command.as_pbfhogg() {
        return run_measured(mode, &dev_config, project, &project_root,
            &pbf.dataset, &pbf.variant, |req| {
                dispatch::run_pbfhogg_command_with_params(req, &pbf_cmd, osc, &params)
            });
    }

    match cli.command {
        // Already handled by as_pbfhogg() above the match.
        Command::Lock | Command::History { .. }
        | Command::Inspect { .. } | Command::InspectNodes { .. }
        | Command::InspectTags { .. } | Command::InspectTagsWay { .. }
        | Command::CheckRefs { .. } | Command::CheckIds { .. }
        | Command::Sort { .. } | Command::CatWay { .. }
        | Command::CatRelation { .. } | Command::CatDedupe { .. }
        | Command::TagsFilterWay { .. } | Command::TagsFilterAmenity { .. }
        | Command::TagsFilterTwopass { .. } | Command::TagsFilterOsc { .. }
        | Command::Getid { .. } | Command::Getparents { .. }
        | Command::GetidInvert { .. } | Command::Renumber { .. }
        | Command::MergeChanges { .. } | Command::ApplyChanges { .. }
        | Command::AddLocationsToWays { .. }
        | Command::ExtractSimple { .. } | Command::ExtractComplete { .. }
        | Command::ExtractSmart { .. } | Command::TimeFilter { .. }
        | Command::Diff { .. } | Command::DiffOsc { .. }
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
            run_measured(&mode, &dev_config, project, &project_root,
                &pbf.dataset, &pbf.variant, |req| {
                    if strategy == "all" {
                        for strat in pbfhogg::commands::ExtractStrategy::all() {
                            let cmd = pbfhogg::commands::PbfhoggCommand::Extract { strategy: *strat };
                            dispatch::run_pbfhogg_command_with_params(req, &cmd, None, &params)?;
                        }
                        Ok(())
                    } else {
                        let strat = pbfhogg::commands::ExtractStrategy::parse(&strategy)?;
                        let cmd = pbfhogg::commands::PbfhoggCommand::Extract { strategy: strat };
                        dispatch::run_pbfhogg_command_with_params(req, &cmd, None, &params)
                    }
                })
        }
        Command::Read { mode, pbf, modes } => {
            run_measured(&mode, &dev_config, project, &project_root,
                &pbf.dataset, &pbf.variant, |req| {
                    warn_sidecar_ignored(req);
                    pbfhogg::cmd::bench_read(req, &modes)
                })
        }
        Command::Write { mode, pbf, compression } => {
            run_measured(&mode, &dev_config, project, &project_root,
                &pbf.dataset, &pbf.variant, |req| {
                    warn_sidecar_ignored(req);
                    pbfhogg::cmd::bench_write(req, &compression)
                })
        }
        Command::MergeBench { mode, pbf, compression, uring, osc_seq } => {
            run_measured(&mode, &dev_config, project, &project_root,
                &pbf.dataset, &pbf.variant, |req| {
                    warn_sidecar_ignored(req);
                    pbfhogg::cmd::bench_merge(req, osc_seq.as_deref(), uring, &compression)
                })
        }

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
            run_measured(&mode, &dev_config, project, &project_root,
                &dataset, &variant, |req| dispatch::run_elivagar_command(req, &cmd))
        }
        Command::PmtilesWriter { mode, tiles } => {
            let cmd = elivagar::commands::ElivagarCommand::PmtilesWriter { tiles };
            run_measured(&mode, &dev_config, project, &project_root,
                "denmark", "raw", |req| dispatch::run_elivagar_command(req, &cmd))
        }
        Command::NodeStore { mode, nodes } => {
            let cmd = elivagar::commands::ElivagarCommand::NodeStore { nodes };
            run_measured(&mode, &dev_config, project, &project_root,
                "denmark", "raw", |req| dispatch::run_elivagar_command(req, &cmd))
        }
        Command::ElivPlanetiler { mode, dataset, variant } => {
            let cmd = elivagar::commands::ElivagarCommand::Planetiler;
            run_measured(&mode, &dev_config, project, &project_root,
                &dataset, &variant, |req| dispatch::run_elivagar_command(req, &cmd))
        }
        Command::ElivTilemaker { mode, dataset, variant } => {
            let cmd = elivagar::commands::ElivagarCommand::Tilemaker;
            run_measured(&mode, &dev_config, project, &project_root,
                &dataset, &variant, |req| dispatch::run_elivagar_command(req, &cmd))
        }

        // ----- nidhogg commands -----
        Command::RunApi { mode, dataset, 
 query } => {
            let query = query.clone();
            run_measured(&mode, &dev_config, project, &project_root, &dataset, "raw", |req| {
                dispatch::run_nidhogg_command(req, "api",
                    |req| nidhogg::cmd::bench_api(req, query.as_deref()),
                    |req| nidhogg::cmd::hotpath(req))
            })
        }
        Command::RunNidIngest { mode, dataset, variant } => {
            run_measured(&mode, &dev_config, project, &project_root, &dataset, &variant, |req| {
                dispatch::run_nidhogg_command(req, "nid-ingest",
                    |req| nidhogg::cmd::bench_ingest(req),
                    |req| nidhogg::cmd::hotpath(req))
            })
        }
        Command::RunTiles { mode, dataset, tiles, 
 uring } => {
            let tiles = tiles.clone();
            run_measured(&mode, &dev_config, project, &project_root, &dataset, "raw", |req| {
                dispatch::run_nidhogg_command(req, "tiles",
                    |req| nidhogg::cmd::bench_tiles(req, tiles.as_deref(), uring),
                    |req| nidhogg::cmd::hotpath(req))
            })
        }

        // ----- sluggrs commands -----
        Command::SluggrsHotpath { mode } => {
            run_measured(&mode, &dev_config, project, &project_root, "n/a", "n/a", |req| {
                project::require(project, Project::Sluggrs, "sluggrs-hotpath")?;
                if !req.is_alloc() && !matches!(req.mode, measure::MeasureMode::Hotpath { .. }) {
                    return Err(DevError::Config(
                        "sluggrs-hotpath only supports --hotpath or --alloc modes".into(),
                    ));
                }
                sluggrs::hotpath::cmd(req)
            })
        }

        // ----- generic commands -----
        Command::GenericHotpath {
            mode,
            dataset,
            variant,
            

        } => {
            run_measured(&mode, &dev_config, project, &project_root, &dataset, &variant, |req| {
                if !req.is_alloc() && !matches!(req.mode, measure::MeasureMode::Hotpath { .. }) {
                    return Err(DevError::Config(
                        "generic-hotpath only supports --hotpath or --alloc modes".into(),
                    ));
                }
                cmd_hotpath_generic(req)
            })
        }

        // ----- suites -----
        Command::Suite {
            mode,
            name,
            dataset,
            variant,
            

        } => {
            run_measured(&mode, &dev_config, project, &project_root, &dataset, &variant, |req| {
                warn_sidecar_ignored(req);
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
                        nidhogg::cmd::bench_ingest(req)
                    }
                    other => Err(DevError::Config(format!(
                        "unknown suite: {other} (expected: pbfhogg, elivagar, nidhogg)"
                    ))),
                }
            })
        }
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
            limit,
            top,
        } => {
            let rq = ResultsQuery {
                query,
                commit,
                compare,
                compare_last,
                command,
                variant,
                limit,
                top,
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
        Command::Download { region, osc_url } => {
            let _lock = acquire_cmd_lock(project, &project_root, "download")?;
            pbfhogg::cmd::download(
                &dev_config,
                project,
                &project_root,
                &region,
                osc_url.as_deref(),
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
        Command::Litehtml {
            litehtml: litehtml_cmd,
        } => {
            let litehtml_config = dev_config.litehtml.as_ref().ok_or_else(|| {
                error::DevError::Config("no [litehtml] section in brokkr.toml".into())
            })?;
            match litehtml_cmd {
                LitehtmlCommand::Test {
                    fixture,
                    suite,
                    all,
                    recapture,
                } => litehtml::cmd::test(
                    project,
                    &project_root,
                    litehtml_config,
                    fixture.as_deref(),
                    suite.as_deref(),
                    all,
                    recapture,
                ),
                LitehtmlCommand::List => {
                    litehtml::cmd::list(project, &project_root, litehtml_config)
                }
                LitehtmlCommand::Approve { fixture } => {
                    litehtml::cmd::approve(project, &project_root, litehtml_config, &fixture)
                }
                LitehtmlCommand::Report { run_id } => {
                    litehtml::cmd::report(project, &project_root, &run_id)
                }
                LitehtmlCommand::Status => {
                    litehtml::cmd::status(project, &project_root, litehtml_config)
                }
                LitehtmlCommand::Prepare { input, output } => {
                    litehtml::cmd::prepare(project, &project_root, litehtml_config, &input, &output)
                }
                LitehtmlCommand::Extract {
                    input,
                    selector,
                    from,
                    to,
                    output,
                } => litehtml::cmd::extract(
                    project,
                    &project_root,
                    &input,
                    selector.as_deref(),
                    from.as_deref(),
                    to.as_deref(),
                    &output,
                ),
                LitehtmlCommand::Outline {
                    input,
                    depth,
                    full,
                    selectors,
                } => litehtml::cmd::outline(project, &project_root, &input, depth, full, selectors),
            }
        }
        Command::Sluggrs {
            sluggrs: sluggrs_cmd,
        } => {
            let sluggrs_config = dev_config.sluggrs.as_ref().ok_or_else(|| {
                error::DevError::Config("no [sluggrs] section in brokkr.toml".into())
            })?;
            match sluggrs_cmd {
                SluggrsCommand::Test { snapshot, all } => sluggrs::cmd::test(
                    project,
                    &project_root,
                    sluggrs_config,
                    snapshot.as_deref(),
                    all,
                ),
                SluggrsCommand::List => sluggrs::cmd::list(project, &project_root, sluggrs_config),
                SluggrsCommand::Approve { snapshot } => {
                    sluggrs::cmd::approve(project, &project_root, sluggrs_config, &snapshot)
                }
                SluggrsCommand::Status => {
                    sluggrs::cmd::status(project, &project_root, sluggrs_config)
                }
                SluggrsCommand::Report { run_id } => {
                    sluggrs::cmd::report(project, &project_root, &run_id)
                }
            }
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
    if mode.sidecar && !matches!(result, measure::MeasureMode::Bench { .. }) {
        return Err(DevError::Config(
            "--sidecar currently requires --bench".into(),
        ));
    }
    Ok(result)
}

/// Warn if `--sidecar` was requested but the command does not support it.
fn warn_sidecar_ignored(req: &measure::MeasureRequest) {
    if req.sidecar {
        output::error("WARNING: --sidecar is not supported for this command — ignoring");
    }
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

fn cmd_results(project_root: &Path, q: &ResultsQuery) -> Result<(), DevError> {
    let db_path = results_db_path(project_root);

    if !db_path.exists() {
        output::result_msg("no results yet (run a benchmark first)");
        return Ok(());
    }

    let results_db = db::ResultsDb::open(&db_path)?;

    if let Some(ref uuid_prefix) = q.query {
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
        )?;
        let table = db::format_compare(commit_a, &rows_a, commit_b, &rows_b, q.top);
        println!("{table}");
    } else if q.compare_last {
        match results_db.query_compare_last(q.command.as_deref(), q.variant.as_deref())? {
            Some((commit_a, rows_a, commit_b, rows_b)) => {
                let table = db::format_compare(&commit_a, &rows_a, &commit_b, &rows_b, q.top);
                println!("{table}");
            }
            None => {
                output::result_msg("need at least two distinct commits to compare");
            }
        }
    } else {
        let filter = db::QueryFilter {
            commit: q.commit.clone(),
            command: q.command.clone(),
            variant: q.variant.clone(),
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
        runs: req.runs,
        cli_args: Some(harness::format_cli_args(&binary_str, &[])),
        metadata: vec![db::KvPair::text("meta.alloc", alloc.to_string())],
    };

    ctx.harness.run_internal(&config, |_i| {
        let (result, _stderr) = harness::run_hotpath_capture(
            &binary_str,
            &[],
            &ctx.paths.scratch_dir,
            req.project_root,
            &[],
        )?;
        Ok(result)
    })?;

    Ok(())
}
