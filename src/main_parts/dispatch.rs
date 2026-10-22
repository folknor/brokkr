//! Top-level and project command dispatch.

use super::bootstrap::{
    refuse_unmeasured_force, require_detection, run_check, run_clippy, run_deps, run_fmt,
    run_install, run_man, run_measured, run_program, run_sync, run_wc,
};
use crate::clean::CleanOpts;
use crate::clean::{cargo_clean_package, cmd_clean};
use crate::cli::{Cli, Command, VerifyCommand};
use crate::command_args::{profile_choice, resolve_features, resolve_mode};
use crate::config::project_config::ProjectConfig;
use crate::context::with_worktree;
use crate::env::cmd_env;
use crate::error::DevError;
use crate::generic_hotpath_cmd::cmd_hotpath_generic;
use crate::lock_cmd::{cmd_kill, cmd_lock};
use crate::passthrough_cmd::{RunOptions, cmd_run};
use crate::pmtiles_stats_cmd::cmd_pmtiles_stats;
use crate::project::Project;
use crate::request::{CorpusQuery, ResultsQuery, SidecarQuery};
use crate::verify_cmd::cmd_verify;
use crate::visual_approve_cmd::cmd_approve;
use crate::{
    bench_cmd, check_cmd, cli, command_input, context, dellingr, elivagar, history_cmd,
    invalidate_cmd, litehtml, lockfile, measure, mogwai, nidhogg, output, pbfhogg, piners, project,
    ratatoskr, results_cmd, runnables, sidecar_cmd, sluggrs, test_cmd, toolchain,
};

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(super) fn run(cli: Cli, detection: Option<&project::Detection>) -> Result<(), DevError> {
    let spec = cli::invocation().spec();
    if spec.config == cli::ConfigNeed::Required {
        spec.require(&require_detection(detection)?.project)?;
    } else if let Some(detection) = detection {
        spec.require(&detection.project)?;
    }
    let fallback_roots = if detection.is_none() && spec.config != cli::ConfigNeed::None {
        Some(project::detect_roots()?)
    } else {
        None
    };
    let state_root = detection
        .map(|d| &d.project_root)
        .or_else(|| fallback_roots.as_ref().map(|roots| &roots.0));
    let project = detection.map(|d| d.project.clone());
    let _log = spec.run_log.map(|kind| {
        crate::run_log::RunLog::begin(
            state_root
                .expect("logging commands resolve a state root")
                .as_path(),
            kind,
        )
    });
    let _lock = match spec.lock {
        cli::LockPolicy::None => None,
        cli::LockPolicy::Blocking | cli::LockPolicy::NonBlocking => {
            let ctx = lockfile::LockContext {
                project: project.as_ref().map_or("brokkr", Project::name),
                command: cli::invocation(),
                project_root: state_root.expect("locking commands resolve a state root"),
                build_root: detection
                    .map(|d| &d.build_root)
                    .or_else(|| fallback_roots.as_ref().map(|roots| &roots.1))
                    .expect("locking commands resolve a build root"),
                // Activated explicitly below, so it can be skipped for
                // worktree commands and tracked on the hold.
                disable_toolchain_dir: None,
            };
            Some(if spec.lock == cli::LockPolicy::NonBlocking {
                lockfile::try_acquire_without_toolchain(&ctx)?
            } else {
                lockfile::acquire_without_toolchain(&ctx)?
            })
        }
    };
    let disable_dir = detection
        .filter(|d| d.config.disable_toolchain)
        .map(|d| d.build_root.as_path().to_path_buf());
    context::set_configured_toolchain_dir(disable_dir);
    let historical = spec.uses_worktree(&cli.command)?;
    let _toolchain = if !historical && spec.lock != cli::LockPolicy::None {
        detection
            .filter(|d| d.config.disable_toolchain)
            .map(|d| toolchain::DisabledToolchain::activate(d.build_root.as_path()))
            .transpose()?
    } else {
        None
    };
    let _toolchain_registration = _toolchain.as_ref().and_then(|_| {
        _lock.as_ref().map(|lock| {
            lock.track_toolchain_dir(
                detection
                    .expect("toolchain suppression requires config")
                    .build_root
                    .as_path(),
            )
        })
    });
    match &cli.command {
        Command::Fmt { args } => run_fmt(args, detection),
        Command::Man {
            topic,
            sections,
            full,
        } => run_man(topic.as_deref(), sections, *full, detection),
        Command::Lock => cmd_lock(),
        Command::Kill { hard } => cmd_kill(*hard),
        Command::Strays { kill } => crate::stray::cmd_strays(*kill),
        Command::Guard { install, remove } => crate::guard::cmd_guard(*install, *remove),
        Command::History {
            id,
            command,
            project,
            failed,
            status,
            since,
            until,
            slow,
            limit,
            all,
        } => history_cmd::cmd_history(history_cmd::HistoryQuery {
            id: *id,
            command: command.clone(),
            project: project.clone(),
            failed: *failed,
            status: *status,
            since: since.clone(),
            until: until.clone(),
            slow: *slow,
            limit: *limit,
            all: *all,
        }),

        Command::Run {
            name,
            debug,
            release,
            profile,
            features,
            all_features,
            no_default_features,
            commit,
            args,
        } => {
            let feat = runnables::FeatureArgs {
                features,
                all: *all_features,
                no_default: *no_default_features,
            };
            run_program(
                name.as_deref(),
                profile_choice(*debug, *release, profile.as_ref()),
                &feat,
                commit.as_deref(),
                args,
                detection,
            )
        }
        Command::Install {
            debug,
            release,
            profile,
            unlocked,
        } => run_install(
            profile_choice(*debug, *release, profile.as_ref()),
            *unlocked,
            detection,
        ),
        Command::Wc { threshold } => run_wc(*threshold, detection),
        Command::Bench {
            target,
            commit,
            compare,
            baselines,
            name,
            lenient,
            args,
        } => bench_cmd::run(
            &bench_cmd::BenchArgs {
                target: target.clone(),
                commit: commit.clone(),
                compare: compare
                    .clone()
                    .map(|values| crate::request::compare_pair(values, "--compare"))
                    .transpose()?,
                baselines: *baselines,
                name: name.clone(),
                lenient: *lenient,
                args: args.clone(),
            },
            detection,
        ),
        Command::Deps {
            json,
            no_fail,
            focus,
        } => run_deps(*json, *no_fail, focus.clone(), detection),
        Command::Check {
            features,
            no_default_features,
            package,
            profile,
            gate,
            force_rust,
            json,
            fix_gremlins,
            timings,
            commands,
            textlint_names,
            script_names,
            audit,
            args,
        } => run_check(
            features,
            no_default_features,
            package,
            profile,
            gate,
            force_rust,
            json,
            fix_gremlins,
            timings,
            commands,
            textlint_names,
            script_names,
            *audit,
            args,
            detection,
        ),
        Command::Clippy {
            package,
            all_features,
            features,
            no_default_features,
            sweep,
            env,
            lib,
        } => run_clippy(
            package,
            all_features,
            features,
            no_default_features,
            sweep,
            env,
            lib,
            detection,
        ),
        _ => run_project(cli.command, require_detection(detection)?),
    }
}

// The project command table: one arm per subcommand, so its size tracks the CLI's.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_project(command: Command, detection: &project::Detection) -> Result<(), DevError> {
    let project = &detection.project;
    let dev_config = &detection.config;
    // The section parsed for `project`. Load already refused a missing
    // required section and any section belonging to another project, so the
    // handlers below match on it instead of re-checking presence.
    let project_config = &detection.project_config;
    // The config dir (brokkr.toml's directory). Anchors data/ and .brokkr/.
    // The code tree is stored separately in `Detection.build_root`.
    let project_root = detection.project_root.as_path();
    // The code tree (cwd), where cargo and git run. Coincides with
    // `project_root` in the common case (config in cwd) and differs only under
    // the one-level-up layout used to drive a foreign checkout.
    let build_root = detection.build_root.as_path();
    let invocation = crate::run_invocation::capture();

    let command = match command.into_route() {
        command_input::RoutedCommand::Pbf {
            mode,
            pbf,
            command,
            osc,
            mut params,
        } => {
            params.io = crate::pbfhogg::context::IoMode {
                direct_io: pbf.direct_io,
                io_uring: pbf.io_uring,
            };
            params.compression = pbf.compression.clone();
            return run_measured(
                &mode,
                detection,
                &pbf.dataset,
                &pbf.variant,
                &invocation,
                |req| {
                    measure::runner::run(
                        req,
                        &pbfhogg::plan::PbfhoggPlan {
                            command: &command,
                            osc_seq: osc.as_deref(),
                            params: &params,
                        },
                    )
                },
            );
        }
        command_input::RoutedCommand::Other(command) => *command,
    };
    match command {
        Command::Lock
        | Command::Kill { .. }
        | Command::Strays { .. }
        | Command::Guard { .. }
        | Command::History { .. }
        | Command::Check { .. }
        | Command::Clippy { .. }
        | Command::Fmt { .. }
        | Command::Bench { .. }
        | Command::Run { .. }
        | Command::Install { .. }
        | Command::Wc { .. }
        | Command::Man { .. }
        | Command::Deps { .. }
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
        | Command::BuildGeocodeIndex { .. } => {
            unreachable!("top-level command dispatch must not re-enter through run_project")
        }
        Command::Env => cmd_env(
            dev_config,
            project,
            &detection.project_root,
            &detection.build_root,
            &detection.config_sources,
        ),
        Command::DiffSnapshots {
            mode,
            dataset,
            from,
            to,
            variant,
            format,
            jobs,
        } => {
            let params = crate::pbfhogg::context::CommandParams {
                from_snapshot: Some(from.clone()),
                to_snapshot: Some(to.clone()),
                jobs,
                ..Default::default()
            };
            run_measured(&mode, detection, &dataset, &variant, &invocation, |req| {
                let cmd = pbfhogg::commands::PbfhoggCommand::DiffSnapshots { format };
                measure::runner::run(
                    req,
                    &pbfhogg::plan::PbfhoggPlan {
                        command: &cmd,
                        osc_seq: None,
                        params: &params,
                    },
                )
            })
        }
        Command::Extract {
            mode,
            pbf,
            strategy,
            bbox,
            snapshot,
        } => {
            let params = crate::pbfhogg::context::CommandParams {
                bbox: bbox.clone(),
                snapshot: snapshot.clone(),
                ..Default::default()
            };
            run_measured(
                &mode,
                detection,
                &pbf.dataset,
                &pbf.variant,
                &invocation,
                |req| {
                    if strategy == "all" {
                        for strat in pbfhogg::commands::ExtractStrategy::all() {
                            let cmd =
                                pbfhogg::commands::PbfhoggCommand::Extract { strategy: *strat };
                            measure::runner::run(
                                req,
                                &pbfhogg::plan::PbfhoggPlan {
                                    command: &cmd,
                                    osc_seq: None,
                                    params: &params,
                                },
                            )?;
                        }
                        Ok(())
                    } else {
                        let strat = pbfhogg::commands::ExtractStrategy::parse(&strategy)?;
                        let cmd = pbfhogg::commands::PbfhoggCommand::Extract { strategy: strat };
                        measure::runner::run(
                            req,
                            &pbfhogg::plan::PbfhoggPlan {
                                command: &cmd,
                                osc_seq: None,
                                params: &params,
                            },
                        )
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
            let params = crate::pbfhogg::context::CommandParams {
                bbox: bbox.clone(),
                snapshot: snapshot.clone(),
                ..Default::default()
            };
            run_measured(
                &mode,
                detection,
                &pbf.dataset,
                &pbf.variant,
                &invocation,
                |req| {
                    if strategy == "all" {
                        for strat in pbfhogg::commands::ExtractStrategy::all() {
                            let cmd = pbfhogg::commands::PbfhoggCommand::MultiExtract {
                                regions,
                                strategy: *strat,
                            };
                            measure::runner::run(
                                req,
                                &pbfhogg::plan::PbfhoggPlan {
                                    command: &cmd,
                                    osc_seq: None,
                                    params: &params,
                                },
                            )?;
                        }
                        Ok(())
                    } else {
                        let strat = pbfhogg::commands::ExtractStrategy::parse(&strategy)?;
                        let cmd = pbfhogg::commands::PbfhoggCommand::MultiExtract {
                            regions,
                            strategy: strat,
                        };
                        measure::runner::run(
                            req,
                            &pbfhogg::plan::PbfhoggPlan {
                                command: &cmd,
                                osc_seq: None,
                                params: &params,
                            },
                        )
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
            detection,
            &pbf.dataset,
            &pbf.variant,
            &invocation,
            |req| pbfhogg::cmd::bench_read(req, &modes, snapshot.as_ref()),
        ),
        Command::Write {
            mode,
            pbf,
            compressions,
        } => run_measured(
            &mode,
            detection,
            &pbf.dataset,
            &pbf.variant,
            &invocation,
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
            detection,
            &pbf.dataset,
            &pbf.variant,
            &invocation,
            |req| pbfhogg::cmd::bench_merge(req, osc_seq.as_deref(), uring, &compressions),
        ),

        // ----- elivagar commands -----
        Command::Tilegen {
            mode,
            dataset,
            variant,
            skip_to,
        } => {
            // The pipeline contract comes from the named block. Input
            // assertions are attached later to the resolved PBF entry.
            let tilegen = elivagar::resolve_tilegen(dev_config, elivagar::DEFAULT_TILEGEN)?;
            let cmd = elivagar::commands::ElivagarCommand::Built(
                elivagar::commands::BuiltCommand::MainBinary {
                    tilegen,
                    skip_to: skip_to.as_deref(),
                },
            );
            run_measured(&mode, detection, &dataset, &variant, &invocation, |req| {
                measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd))
            })
        }
        Command::PmtilesWriter { mode, tiles } => {
            let cmd = elivagar::commands::ElivagarCommand::Built(
                elivagar::commands::BuiltCommand::Example(
                    elivagar::commands::ExampleCommand::PmtilesWriter { tiles },
                ),
            );
            run_measured(
                &mode,
                detection,
                cli::DEFAULT_DATASET,
                cli::DEFAULT_RAW_VARIANT,
                &invocation,
                |req| measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd)),
            )
        }
        Command::NodeStore { mode, nodes } => {
            let cmd = elivagar::commands::ElivagarCommand::Built(
                elivagar::commands::BuiltCommand::Example(
                    elivagar::commands::ExampleCommand::NodeStore { nodes },
                ),
            );
            run_measured(
                &mode,
                detection,
                cli::DEFAULT_DATASET,
                cli::DEFAULT_RAW_VARIANT,
                &invocation,
                |req| measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd)),
            )
        }
        Command::ElivPlanetiler {
            mode,
            dataset,
            variant,
        } => {
            let cmd = elivagar::commands::ElivagarCommand::External(
                elivagar::commands::ExternalTool::Planetiler,
            );
            run_measured(&mode, detection, &dataset, &variant, &invocation, |req| {
                measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd))
            })
        }
        Command::ElivTilemaker {
            mode,
            dataset,
            variant,
        } => {
            let cmd = elivagar::commands::ElivagarCommand::External(
                elivagar::commands::ExternalTool::Tilemaker,
            );
            run_measured(&mode, detection, &dataset, &variant, &invocation, |req| {
                measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd))
            })
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
                detection,
                &dataset,
                cli::DEFAULT_RAW_VARIANT,
                &invocation,
                |req| measure::runner::run(req, &nidhogg::plan::NidhoggPlan(&cmd)),
            )
        }
        Command::RunNidIngest {
            mode,
            dataset,
            variant,
        } => {
            let cmd = nidhogg::commands::NidhoggCommand::Ingest;
            run_measured(&mode, detection, &dataset, &variant, &invocation, |req| {
                measure::runner::run(req, &nidhogg::plan::NidhoggPlan(&cmd))
            })
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
                detection,
                &dataset,
                cli::DEFAULT_RAW_VARIANT,
                &invocation,
                |req| measure::runner::run(req, &nidhogg::plan::NidhoggPlan(&cmd)),
            )
        }

        // ----- dellingr commands -----
        // Dataset/variant are the `run_measured` scaffolding every measured
        // command shares; dellingr has neither, so both stay empty rather
        // than inventing a dataset name that would land in the DB.
        Command::Dellingr { mode, lua } => {
            run_measured(&mode, detection, "", "", &invocation, |req| {
                dellingr::cmd::run(req, &lua)
            })
        }

        // ----- mogwai commands -----
        // Dataset/variant stay empty: an input a mogwai surface reads is named
        // in its argv, which the row captures verbatim, so filling these would
        // duplicate it under a second name that pairing also keys on.
        Command::Mogwai { mode, target, args } => {
            run_measured(&mode, detection, "", "", &invocation, |req| {
                mogwai::cmd::run(req, target.as_deref(), &args)
            })
        }

        // ----- sluggrs commands -----
        // ----- generic commands -----
        Command::GenericHotpath {
            mode,
            dataset,
            variant,
        } => run_measured(&mode, detection, &dataset, &variant, &invocation, |req| {
            if req.dry_run {
                return Err(DevError::Usage(
                    "--dry-run is not yet supported for generic-hotpath".into(),
                ));
            }
            if req.mode.profiled().is_none() {
                return Err(DevError::Usage(
                    "generic-hotpath only supports --hotpath or --alloc modes".into(),
                ));
            }
            cmd_hotpath_generic(req)
        }),

        // ----- suites -----
        Command::Suite {
            mode,
            name,
            dataset,
            variant,
        } => run_measured(&mode, detection, &dataset, &variant, &invocation, |req| {
            if req.dry_run {
                return Err(DevError::Usage(
                    "--dry-run is not yet supported for suites".into(),
                ));
            }
            if !matches!(
                req.mode,
                measure::MeasureMode::Run | measure::MeasureMode::Bench { .. }
            ) {
                return Err(DevError::Usage(
                    "suite mode only supports wall-clock timing".into(),
                ));
            }
            match name.as_str() {
                "pbfhogg" => {
                    project::require(project, &Project::Pbfhogg, "suite pbfhogg")?;
                    pbfhogg::cmd::bench_all(req)
                }
                "elivagar" => {
                    project::require(project, &Project::Elivagar, "suite elivagar")?;
                    elivagar::cmd::bench_all(req)
                }
                "nidhogg" => {
                    project::require(project, &Project::Nidhogg, "suite nidhogg")?;
                    measure::runner::run(
                        req,
                        &nidhogg::plan::NidhoggPlan(&nidhogg::commands::NidhoggCommand::Api {
                            query: None,
                        }),
                    )?;
                    measure::runner::run(
                        req,
                        &nidhogg::plan::NidhoggPlan(&nidhogg::commands::NidhoggCommand::Ingest),
                    )
                }
                other => Err(DevError::Usage(format!(
                    "unknown suite: {other} (expected: pbfhogg, elivagar, nidhogg)"
                ))),
            }
        }),
        Command::Passthrough {
            features,
            time,
            json,
            runs,
            no_build,
            args,
        } => {
            let _lock = context::reenter_command_lock()?;
            let features = resolve_features(dev_config, &features);
            let opts = RunOptions {
                time,
                json,
                runs,
                no_build,
            };
            cmd_run(
                dev_config,
                project,
                &detection.project_root,
                &detection.build_root,
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
            let filter = crate::db::QueryFilter {
                commit,
                command,
                mode,
                dataset,
                meta: meta
                    .iter()
                    .filter_map(|s| {
                        s.split_once('=')
                            .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    })
                    .collect(),
                env: env
                    .iter()
                    .filter_map(|s| {
                        s.split_once('=')
                            .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    })
                    .collect(),
                grep,
                grep_v,
                limit,
            };
            let view = if let Some(prefix) = query {
                crate::request::ResultsView::Uuid { prefix }
            } else if let Some(values) = compare {
                crate::request::ResultsView::Compare {
                    commits: crate::request::compare_pair(values, "--compare")?,
                }
            } else {
                crate::request::ResultsView::Table
            };
            let rq = ResultsQuery { view, filter, top };
            results_cmd::cmd_results(dev_config, project_root, &rq)
        }
        Command::CorpusResults {
            run_id,
            run,
            limit,
            probe,
            diffs,
            dispositions,
            columns,
            runtimes,
            over,
            trend,
            where_expr,
            sql,
            full,
            compare,
        } => {
            let cq = CorpusQuery {
                run_id,
                run,
                limit,
                probe,
                diffs,
                dispositions,
                columns,
                runtimes,
                over,
                trend,
                where_expr,
                sql,
                full,
                compare: compare
                    .map(|values| crate::request::compare_pair(values, "--compare"))
                    .transpose()?,
            };
            piners::corpus_query::cmd(project_root, &cq)
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
            let view = if let Some(values) = compare {
                crate::request::SidecarView::Compare {
                    uuids: crate::request::compare_pair(values, "--compare")?,
                }
            } else {
                let uuid = query.ok_or_else(|| {
                    DevError::Usage("sidecar requires a UUID unless --compare is used".into())
                })?;
                if samples {
                    crate::request::SidecarView::Samples { uuid }
                } else if markers {
                    crate::request::SidecarView::Markers { uuid }
                } else if durations {
                    crate::request::SidecarView::Durations { uuid }
                } else if counters {
                    crate::request::SidecarView::Counters { uuid }
                } else if stalls {
                    crate::request::SidecarView::Stalls { uuid }
                } else if stat.is_some() {
                    crate::request::SidecarView::Stat { uuid }
                } else {
                    crate::request::SidecarView::Summary { uuid }
                }
            };
            let sq = SidecarQuery {
                view,
                stat,
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
            sidecar_cmd::cmd_sidecar(project_root, &sq)
        }
        Command::Invalidate {
            uuid,
            commit,
            force,
        } => {
            invalidate_cmd::cmd_invalidate(project_root, uuid.as_deref(), commit.as_deref(), force)
        }
        Command::Clean {
            worktrees,
            cargo,
            archives,
            keep,
            all,
            dry_run,
        } => {
            let _lock = context::reenter_command_lock()?;
            // `--all` folds in the worktrees, archives, and cargo sweeps.
            let cargo = if all {
                Some(cargo.unwrap_or(None))
            } else {
                cargo
            };
            let cargo_swept = cargo.is_some();
            if let Some(pkg) = cargo {
                let pkg = pkg.unwrap_or_else(|| project.name().to_owned());
                if dry_run {
                    output::run_msg(&format!("would run cargo clean -p {pkg}"));
                } else {
                    cargo_clean_package(build_root, &pkg)?;
                }
            }
            cmd_clean(
                dev_config,
                project,
                &detection.project_root,
                &detection.build_root,
                CleanOpts {
                    cargo_swept,
                    worktrees: worktrees || all,
                    archives: archives || all,
                    keep,
                    dry_run,
                    interrupted: false,
                },
            )
        }
        Command::Verify {
            verbose,
            commit,
            verify,
        } => {
            let features = resolve_features(dev_config, &[]);
            output::set_quiet(!verbose);
            // `verify pmtiles` only reads an archive, with the current binary,
            // so it belongs to the resolver family (pmtiles-inspect/diag/svg/
            // regress) rather than to the pbfhogg cross-validators: no
            // historical worktree, and its own `--commit` picks the file. It
            // takes the same non-blocking lock they do, so it cannot read an
            // archive a concurrent `tilegen` is mid-write.
            if matches!(verify, VerifyCommand::ElivVerify { .. }) {
                if commit.is_some() {
                    return Err(DevError::Usage(
                        "'brokkr verify --commit' rebuilds a historical binary, which \
                         'verify pmtiles' never needs - it reads the archive with the \
                         current one. Use 'brokkr verify pmtiles --commit <hash>' to \
                         choose which archive to open."
                            .to_string(),
                    ));
                }
                let _lock = context::reenter_command_lock()?;
                return cmd_verify(
                    dev_config,
                    project,
                    &detection.project_root,
                    &detection.build_root,
                    detection.config_sources.included(),
                    verify,
                    &features,
                    verbose,
                );
            }
            with_worktree(
                Some(project),
                &detection.project_root,
                &detection.build_root,
                commit.as_deref(),
                false,
                dev_config.disable_toolchain,
                context::worktree_keep(dev_config)?,
                |build_root| {
                    cmd_verify(
                        dev_config,
                        project,
                        &detection.project_root,
                        build_root,
                        detection.config_sources.included(),
                        verify,
                        &features,
                        verbose,
                    )
                },
            )
        }
        Command::Download {
            region,
            osc_seq,
            as_snapshot,
            refresh,
            force,
        } => {
            let _lock = context::reenter_command_lock()?;
            // The binary build, the downloads and the osmium/pbfhogg steps are
            // all captured children that poll the shutdown flag.
            let _interrupts = crate::shutdown::SigtermGuard::install();
            pbfhogg::cmd::download(
                dev_config,
                &detection.project_root,
                &detection.build_root,
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
            // Native since the corpus redesign: it opens the two archives
            // in-process, so there is no cargo build and no `cargo metadata`
            // bootstrap to serialize behind the lock.
            elivagar::cmd::compare_tiles(&file_a, &file_b, sample)
        }
        Command::PmtilesInspect {
            dataset,
            variant,
            commit,
            file,
        } => {
            // The lock serializes this with builds: its bootstrap runs
            // `cargo metadata` against the build tree, which must not run
            // beside another holder's compile, and the archive read must not
            // race a `tilegen` mid-write.
            let _lock = context::reenter_command_lock()?;
            let _interrupts = crate::shutdown::SigtermGuard::install();
            elivagar::cmd::inspect(
                dev_config,
                &detection.project_root,
                &detection.build_root,
                detection.config_sources.included(),
                &dataset,
                &variant,
                commit.as_deref(),
                file.as_deref(),
            )
        }
        Command::Diag {
            dataset,
            variant,
            commit,
            file,
            z,
            x,
            y,
        } => {
            let _lock = context::reenter_command_lock()?;
            let _interrupts = crate::shutdown::SigtermGuard::install();
            elivagar::cmd::diag(
                dev_config,
                &detection.project_root,
                &detection.build_root,
                detection.config_sources.included(),
                &dataset,
                &variant,
                commit.as_deref(),
                file.as_deref(),
                z,
                x,
                y,
            )
        }
        Command::Svg {
            dataset,
            variant,
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
            let _lock = context::reenter_command_lock()?;
            let _interrupts = crate::shutdown::SigtermGuard::install();
            elivagar::cmd::svg(
                dev_config,
                &detection.project_root,
                &detection.build_root,
                detection.config_sources.included(),
                &dataset,
                &variant,
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
            variant,
            commit,
            file,
            against_variant,
            against_commit,
            against,
            tol,
            max_moved,
            max_examples,
            overlay,
            overlay_max,
            json,
        } => {
            // The archive resolver still bootstraps through `cargo metadata`,
            // so the lock is still held; the diff itself is in-process.
            // `operational`: a failure before any verdict (lock, bootstrap,
            // resolution) exits regress's "could not complete" code, not 1.
            elivagar::cmd::operational(elivagar::regress::INCOMPLETE_EXIT, || {
                let _lock = context::reenter_command_lock()?;
                elivagar::cmd::regress(
                    dev_config,
                    &detection.project_root,
                    &detection.build_root,
                    detection.config_sources.included(),
                    &dataset,
                    &variant,
                    commit.as_deref(),
                    file.as_deref(),
                    &against_variant,
                    against_commit.as_deref(),
                    against.as_deref(),
                    tol,
                    max_moved,
                    max_examples,
                    overlay.as_deref(),
                    overlay_max,
                    json,
                )
            })
        }
        Command::PmtilesCorpus { cmd } => {
            elivagar::cmd::operational(elivagar::corpus::INCOMPLETE_EXIT, || {
                let _lock = context::reenter_command_lock()?;
                elivagar::cmd::corpus(
                    dev_config,
                    &detection.project_root,
                    &detection.build_root,
                    detection.config_sources.included(),
                    &cmd,
                )
            })
        }
        Command::DownloadOcean => {
            let _lock = context::reenter_command_lock()?;
            let _interrupts = crate::shutdown::SigtermGuard::install();
            elivagar::cmd::download_ocean(
                dev_config,
                &detection.project_root,
                &detection.build_root,
            )
        }
        Command::OceanBuild { dry_run } => {
            let _lock = context::reenter_command_lock()?;
            let _interrupts = crate::shutdown::SigtermGuard::install();
            elivagar::ocean_build::run(
                dev_config,
                project,
                &detection.project_root,
                &detection.build_root,
                dry_run,
                Some(&_lock),
            )
        }
        Command::DownloadNaturalEarth => {
            let _lock = context::reenter_command_lock()?;
            let _interrupts = crate::shutdown::SigtermGuard::install();
            elivagar::cmd::download_natural_earth(
                dev_config,
                &detection.project_root,
                &detection.build_root,
            )
        }
        Command::PmtilesStats { files } => cmd_pmtiles_stats(project, &files),
        Command::Serve {
            data_dir,
            dataset,
            tiles,
        } => {
            let _lock = context::reenter_command_lock()?;
            let features = resolve_features(dev_config, &[]);
            nidhogg::cmd::serve(
                dev_config,
                &detection.project_root,
                &detection.build_root,
                data_dir.as_deref(),
                &dataset,
                tiles.as_deref(),
                &features,
            )
        }
        Command::Stop => nidhogg::cmd::stop(project_root),
        Command::Status => nidhogg::cmd::status(dev_config),
        Command::Ingest { variant, dataset } => {
            let _lock = context::reenter_command_lock()?;
            let _interrupts = crate::shutdown::SigtermGuard::install();
            let features = resolve_features(dev_config, &[]);
            nidhogg::cmd::ingest(
                dev_config,
                &detection.project_root,
                &detection.build_root,
                &variant,
                &dataset,
                &features,
            )
        }
        Command::Update { args } => {
            let _lock = context::reenter_command_lock()?;
            let _interrupts = crate::shutdown::SigtermGuard::install();
            let features = resolve_features(dev_config, &[]);
            nidhogg::cmd::update(project_root, build_root, &args, &features)
        }
        Command::Query { json } => nidhogg::cmd::query(dev_config, json.as_deref()),
        Command::Geocode { term } => nidhogg::cmd::geocode(dev_config, &term),
        // ----- visual testing commands (litehtml + sluggrs) -----
        Command::Visual { args, litehtml } => {
            // `visual` builds the renderer with cargo, so it takes the global
            // lock like every other build path: the lock is what serialises it
            // against a concurrent measured run (whose timings it would
            // otherwise contaminate) and what activates the armed
            // toolchain-disable. It was the one cargo-driving command reaching
            // its handler unlocked.
            let _lock = context::reenter_command_lock()?;
            // The whole run: build, Chrome capture, renderer children and the
            // scoring between them all poll the flag or finish quickly.
            let _interrupts = crate::shutdown::SigtermGuard::install();
            match project_config {
                ProjectConfig::Litehtml(litehtml_config) => litehtml::cmd::test(
                    project_root,
                    build_root,
                    detection.config_sources.included(),
                    litehtml_config,
                    &args,
                    &litehtml,
                ),
                ProjectConfig::Sluggrs(sluggrs_config) => sluggrs::cmd::test(
                    project_root,
                    build_root,
                    detection.config_sources.included(),
                    sluggrs_config,
                    &args,
                ),
                _ => unreachable!("command specification admits visual only for litehtml/sluggrs"),
            }
        }
        // ----- cargo single-test runner -----
        Command::Test {
            name,
            package,
            repeat,
            jobs,
            debug,
            release,
            timeout,
            sweep,
            timings,
        } => {
            // The lock lives in the config dir (.brokkr); cargo runs
            // against the code tree (build_root), which differs under
            // the one-level-up layout.
            // The run log is reserved before the lock and opened under it (so its narration
            // lands in it) and outlives the watchdog `test_cmd::run` arms, so
            // a 124 or 130 keeps everything up to the stop.
            let _log = check_cmd::RunLog::begin(project_root, check_cmd::run_log_kind::TEST);
            let _lock = context::reenter_command_lock()?;
            // cargo runs in the code tree (build_root); brokkr's own
            // `.brokkr` state (hung-test snapshots) belongs under the
            // config dir (project_root), which differs under the
            // one-level-up foreign-checkout layout.
            test_cmd::run(
                dev_config,
                project.clone(),
                build_root,
                project_root,
                &name,
                package.as_deref(),
                repeat,
                jobs,
                crate::command_args::profile_override(debug, release),
                timeout,
                sweep.as_deref(),
                timings,
            )
        }
        Command::List => match project_config {
            ProjectConfig::Litehtml(cfg) => litehtml::cmd::list(project_root, cfg),
            ProjectConfig::Sluggrs(cfg) => sluggrs::cmd::list(project_root, cfg),
            _ => unreachable!("command specification admits list only for litehtml/sluggrs"),
        },
        Command::Approve { fixture, all } => cmd_approve(
            project_config,
            project_root,
            build_root,
            detection.config_sources.included(),
            fixture,
            all,
        ),
        Command::Report { run_id } => match project {
            Project::Litehtml => litehtml::cmd::report(project_root, &run_id),
            Project::Sluggrs => sluggrs::cmd::report(project_root, &run_id),
            _ => unreachable!("command specification admits report only for litehtml/sluggrs"),
        },
        Command::VisualStatus => match project_config {
            ProjectConfig::Litehtml(cfg) => litehtml::cmd::status(project_root, cfg),
            ProjectConfig::Sluggrs(cfg) => sluggrs::cmd::status(project_root, cfg),
            _ => {
                unreachable!("command specification admits visual-status only for litehtml/sluggrs")
            }
        },
        // ----- litehtml-only commands -----
        Command::Prepare { input, output } => {
            let ProjectConfig::Litehtml(cfg) = project_config else {
                unreachable!("command specification admits prepare only for litehtml")
            };
            let _interrupts = crate::shutdown::SigtermGuard::install();
            litehtml::cmd::prepare(project_root, cfg, &input, &output)
        }
        Command::HtmlExtract {
            input,
            selector,
            from,
            to,
            output,
        } => litehtml::cmd::extract(
            project_root,
            &input,
            selector.as_deref(),
            from.as_deref(),
            to.as_deref(),
            &output,
        ),
        Command::Outline {
            input,
            depth,
            full,
            selectors,
        } => litehtml::cmd::outline(project_root, &input, depth, full, selectors),
        // ----- sluggrs-only commands -----
        Command::Hotpath { mode, target } => {
            run_measured(&mode, detection, "", "", &invocation, |req| {
                sluggrs::hotpath::cmd(req, &target)
            })
        }
        // ----- ratatoskr-only commands -----
        Command::Service {
            script,
            all,
            filter,
            include_ignored,
            keep_artefacts,
            debug,
            release,
            profile,
            repeat,
            keep_going,
        } => {
            let choice = profile_choice(debug, release, profile.as_ref());
            // Same bare-is-an-index shape as `sync`: bare lists, SCRIPT
            // runs one (a directory runs that cohort), --all runs the
            // discovered cohort.
            if all {
                ratatoskr::cmd::service_suite(&crate::request::ServiceSuiteRequest {
                    project_root,
                    build_root: &detection.build_root,
                    included_config_files: detection.config_sources.included(),
                    ratatoskr: project_config.ratatoskr()?,
                    filter: filter.as_deref(),
                    keep_artefacts,
                    profile_override: choice.as_ref(),
                    keep_going,
                    include_ignored,
                    repeat,
                })
            } else if let Some(script) = script {
                ratatoskr::cmd::service_test(&crate::request::ServiceTestRequest {
                    project_root,
                    build_root: &detection.build_root,
                    included_config_files: detection.config_sources.included(),
                    ratatoskr: project_config.ratatoskr()?,
                    script: &script,
                    keep_artefacts,
                    profile_override: choice.as_ref(),
                    repeat,
                    keep_going,
                })
            } else {
                ratatoskr::cmd::service_list(project_root)
            }
        }
        Command::Sync {
            script,
            all,
            filter,
            include_ignored,
            bench,
            force,
            keep_artefacts,
            debug,
            release,
            profile,
            gate,
            as_baseline,
            commit,
        } => run_sync(
            &script,
            &all,
            &filter,
            &include_ignored,
            &bench,
            &force,
            &keep_artefacts,
            profile_choice(debug, release, profile.as_ref()).as_ref(),
            &gate,
            &as_baseline,
            &commit,
            detection,
            &invocation,
        ),
        Command::MockServe { fixture } => {
            let cfg = project_config.ratatoskr()?;
            ratatoskr::saehrimnir::run_mock_serve(&ratatoskr::saehrimnir::MockServeRequest {
                project_root,
                config: cfg,
                fixture: &fixture,
            })
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
            no_isolate,
            debug,
            release,
            profile,
            keep_artefacts,
            harness_args,
            mode,
        } => {
            refuse_unmeasured_force(&mode)?;
            let args = piners::cmd::CorpusArgs {
                keywords: keyword,
                probe,
                all,
                verify_only,
                reseed,
                bless,
                no_gate,
                no_isolate,
                profile_override: profile_choice(debug, release, profile.as_ref()),
                keep_artefacts,
                harness_args,
            };
            if resolve_mode(&mode) != measure::MeasureMode::Run {
                // --hotpath/--alloc: build the harness with the hotpath feature
                // and record to results.db, skipping the gate and runs.db.
                run_measured(&mode, detection, "", "", &invocation, |req| {
                    piners::measured::run(req, &args)
                })
            } else {
                // Bare corpus: the parity run (gate + runs.db), unchanged.
                piners::cmd::corpus(project_root, project_config.piners()?, &args)
            }
        }
    }
}
