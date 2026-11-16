//! Top-level and project command dispatch.

use super::bootstrap::{
    ServeRequest, WithTarget, require_detection, run_build, run_check, run_clippy, run_deps,
    run_fmt, run_install, run_man, run_measured, run_program, run_serve, run_sync, run_wc,
};
use crate::clean::CleanOpts;
use crate::clean::{cargo_clean_package, cmd_clean};
use crate::cli::{Cli, Command, VerifyCommand};
use crate::command_args::{profile_choice, resolve_features};
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
use crate::visual_approve_cmd::cmd_approve;
use crate::{
    check_cmd, cli, context, crit_cmd, elivagar, history_cmd, invalidate_cmd, litehtml, lockfile,
    measure, mogwai, output, piners, project, ratatoskr, results_cmd, runnables, sidecar_cmd,
    sluggrs, test_cmd,
};

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(super) fn run(
    cli: Cli,
    detection: Option<&project::Detection>,
    invocation: &crate::db::RunInvocation,
) -> Result<(), DevError> {
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
    // Before anything writes there: the first run log or journal must already
    // land in an ignored directory.
    if let Some(root) = state_root
        && spec.config != cli::ConfigNeed::None
        && let Err(e) =
            crate::clean::layout::ProjectState::new(root.as_path()).ensure_self_ignored()
    {
        crate::output::warn(&format!(
            "could not write {}/.gitignore: {e}",
            crate::clean::layout::PROJECT_STATE_DIR
        ));
    }
    // One answer for the lock, the run log and the tree below.
    let reads_only =
        cli::CommandSpec::reads_only(&cli.command, detection.map(|d| &d.config.benches));
    let lock_policy = spec.lock_for(reads_only);
    let _log = spec.run_log_for(reads_only).map(|kind| {
        crate::run_log::RunLog::begin(
            state_root
                .expect("logging commands resolve a state root")
                .as_path(),
            kind,
        )
    });
    let _lock = match lock_policy {
        cli::LockPolicy::None => None,
        cli::LockPolicy::Blocking | cli::LockPolicy::NonBlocking => {
            let ctx = lockfile::LockContext {
                project: project.as_ref().map(Project::name),
                command: cli::invocation(),
                project_root: state_root.expect("locking commands resolve a state root"),
                build_root: detection
                    .map(|d| &d.build_root)
                    .or_else(|| fallback_roots.as_ref().map(|roots| &roots.1))
                    .expect("locking commands resolve a build root"),
                // Activated explicitly below, so it can be skipped for
                // worktree commands and tracked on the hold.
                tree: None,
            };
            Some(lockfile::acquire_for_command(
                &ctx,
                lock_policy == cli::LockPolicy::NonBlocking,
            )?)
        }
    };
    // A fresh hold's maintenance phase, before the command does anything:
    // what killed brokkr commands left goes now, and nothing this one makes
    // can be swept. `clean --dry-run` removes nothing, so maintenance only
    // reports.
    if let Some(phase) = _lock
        .as_ref()
        .and_then(lockfile::LockGuard::take_maintenance)
    {
        let mode = match &cli.command {
            Command::Clean { dry_run: true, .. } => crate::maintenance::Mode::Report,
            _ => crate::maintenance::Mode::Apply,
        };
        crate::maintenance::run(phase, mode, state_root.map(project::ProjectRoot::as_path));
    }
    // Every detected tree is carried, active policy or not: activation also
    // recovers a cargo config a crashed run left filtered, which must happen
    // even after the policy that filtered it is gone.
    // A read-only invocation carries no tree, so nothing it reaches can
    // activate a policy on it.
    context::set_configured_tree(detection.filter(|_| !reads_only).map(|d| {
        (
            d.build_root.as_path().to_path_buf(),
            crate::tree_policy::TreePolicy::of(&d.config),
        )
    }));
    let historical = spec.uses_worktree(&cli.command)?;
    // Registered before activation, so the watchdog sees every applied edit;
    // declared first, so it is dropped after the edits are undone.
    let _tree_registration = match (&_lock, detection) {
        (Some(lock), Some(d)) if !historical => Some(lock.track_tree_dir(d.build_root.as_path())),
        _ => None,
    };
    let _tree = match (&_lock, detection) {
        (Some(_), Some(d)) if !historical => Some(crate::tree_policy::HeldTree::activate(
            d.build_root.as_path(),
            &crate::tree_policy::TreePolicy::of(&d.config),
        )?),
        _ => None,
    };
    match &cli.command {
        Command::Fmt { args } => run_fmt(args, detection),
        Command::Man {
            topic,
            sections,
            full,
        } => run_man(topic.as_deref(), sections, *full, detection),
        Command::Lock => cmd_lock(),
        Command::Kill {
            hard,
            project: named,
            all,
        } => cmd_kill(
            &crate::lock_cmd::KillScope::resolve(named.as_deref(), *all, detection)?,
            *hard,
        ),
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
            program,
            args,
        } => run_program(
            name.as_deref(),
            profile_choice(program.debug, program.release, program.profile.as_ref()),
            &program_features(program),
            program.commit.as_deref(),
            with_target(program).as_ref(),
            args,
            detection,
        ),
        Command::Serve {
            target,
            program,
            name,
            ready,
            ready_timeout,
            grace,
            stop,
            args,
        } => {
            if let Some(which) = stop {
                if !program.is_empty() {
                    return Err(DevError::Usage(
                        "--stop takes no build flags; it stops a serve already running".into(),
                    ));
                }
                let root = state_root.map(project::ProjectRoot::as_path);
                return crate::serve::cmd_stop(
                    Some(which.as_str()).filter(|w| !w.is_empty()),
                    root,
                );
            }
            let Some(target) = target else {
                if !program.is_empty() {
                    return Err(DevError::Usage(
                        "name the TARGET to serve; bare `brokkr serve` lists the live serves"
                            .into(),
                    ));
                }
                return crate::serve::cmd_list();
            };
            run_serve(&ServeRequest {
                target,
                choice: profile_choice(program.debug, program.release, program.profile.as_ref()),
                feat: &program_features(program),
                commit: program.commit.as_deref(),
                with: with_target(program).as_ref(),
                name: name.as_deref(),
                ready: ready.as_deref(),
                ready_timeout: ready_timeout.map_or(
                    crate::serve::DEFAULT_READY_TIMEOUT,
                    std::time::Duration::from_secs,
                ),
                grace: grace.map_or(crate::serve::DEFAULT_GRACE, std::time::Duration::from_secs),
                args,
                detection,
            })
        }
        Command::Build {
            name,
            debug,
            release,
            profile,
            features,
            all_features,
            no_default_features,
            commit,
        } => {
            let feat = runnables::FeatureArgs {
                features,
                all: *all_features,
                no_default: *no_default_features,
            };
            run_build(
                name.as_deref(),
                profile_choice(*debug, *release, profile.as_ref()),
                &feat,
                commit.as_deref(),
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
        Command::Crit {
            target,
            commit,
            compare,
            baselines,
            name,
            lenient,
            args,
        } => crit_cmd::run(
            &crit_cmd::CritArgs {
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
        Command::Bench {
            commit,
            iterations,
            hotpath,
            alloc,
            dry_run,
            test,
            bench,
        } => crate::bench::runner::run(
            &crate::bench::runner::BenchArgs {
                name: bench.first().cloned(),
                commit: commit.clone(),
                iterations: *iterations,
                hotpath: *hotpath,
                alloc: *alloc,
                params: bench.iter().skip(1).cloned().collect(),
                dry_run: *dry_run,
                test: *test,
            },
            require_detection(detection)?,
            invocation,
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
        _ => run_project(cli.command, require_detection(detection)?, invocation),
    }
}

/// The program's own feature selection from the shared `run`/`serve` flags.
fn program_features(program: &cli::ProgramArgs) -> runnables::FeatureArgs<'_> {
    runnables::FeatureArgs {
        features: &program.features,
        all: program.all_features,
        no_default: program.no_default_features,
    }
}

/// The `--with` target the shared `run`/`serve` flags name, if any.
fn with_target(program: &cli::ProgramArgs) -> Option<WithTarget<'_>> {
    program.with.as_deref().map(|name| WithTarget {
        name,
        commit: program.with_commit.as_deref(),
        feat: runnables::FeatureArgs {
            features: &program.with_features,
            all: program.with_all_features,
            no_default: program.with_no_default_features,
        },
    })
}

// The project command table: one arm per subcommand, so its size tracks the CLI's.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_project(
    command: Command,
    detection: &project::Detection,
    invocation: &crate::db::RunInvocation,
) -> Result<(), DevError> {
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

    match command {
        Command::Lock
        | Command::Kill { .. }
        | Command::Strays { .. }
        | Command::Guard { .. }
        | Command::History { .. }
        | Command::Check { .. }
        | Command::Clippy { .. }
        | Command::Fmt { .. }
        | Command::Crit { .. }
        | Command::Bench { .. }
        | Command::Run { .. }
        | Command::Serve { .. }
        | Command::Build { .. }
        | Command::Install { .. }
        | Command::Wc { .. }
        | Command::Man { .. }
        | Command::Deps { .. } => {
            unreachable!("top-level command dispatch must not re-enter through run_project")
        }
        Command::Env => cmd_env(
            dev_config,
            project,
            &detection.project_root,
            &detection.build_root,
            &detection.config_sources,
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
            run_measured(&mode, detection, &dataset, &variant, invocation, |req| {
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
                invocation,
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
                invocation,
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
            run_measured(&mode, detection, &dataset, &variant, invocation, |req| {
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
            run_measured(&mode, detection, &dataset, &variant, invocation, |req| {
                measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd))
            })
        }

        // ----- mogwai commands -----
        // Dataset/variant stay empty: brokkr does not know what a mogwai
        // surface reads. An input the argv names is captured verbatim with it;
        // one the surface defaults internally is named nowhere in the row.
        Command::Mogwai { mode, target, args } => {
            run_measured(&mode, detection, "", "", invocation, |req| {
                mogwai::cmd::run(req, target.as_deref(), &args)
            })
        }

        // ----- sluggrs commands -----
        // ----- generic commands -----
        Command::GenericHotpath {
            mode,
            dataset,
            variant,
        } => run_measured(&mode, detection, &dataset, &variant, invocation, |req| {
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
        } => run_measured(&mode, detection, &dataset, &variant, invocation, |req| {
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
                "elivagar" => {
                    project::require(project, &Project::Elivagar, "suite elivagar")?;
                    elivagar::cmd::bench_all(req)
                }
                other => Err(DevError::Usage(format!(
                    "unknown suite: {other} (expected: elivagar)"
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
            param,
            grep,
            grep_v,
            limit,
            top,
            hidden,
            retire,
            unretire,
            invalidate,
            revalidate,
            rename,
            reason,
        } => {
            use crate::bench::marking::MarkAction;
            // clap requires --reason with --retire and --invalidate, and
            // allows one mark flag at most.
            let reason = reason.unwrap_or_default();
            let action = if let Some(name) = retire {
                Some(MarkAction::Retire { name, reason })
            } else if let Some(name) = unretire {
                Some(MarkAction::Unretire { name })
            } else if let Some(run) = invalidate {
                Some(MarkAction::Invalidate { run, reason })
            } else if let Some(run) = revalidate {
                Some(MarkAction::Revalidate { run })
            } else {
                // clap takes exactly two values for --rename.
                rename.and_then(|names| match <[String; 2]>::try_from(names) {
                    Ok([from, to]) => Some(MarkAction::Rename { from, to }),
                    Err(_) => None,
                })
            };
            if let Some(action) = action {
                return crate::bench::marking::apply(
                    &action,
                    project.name(),
                    dev_config.results_server.as_ref(),
                );
            }
            let params = param
                .iter()
                .filter_map(|s| {
                    s.split_once('=')
                        .map(|(name, value)| (name.to_owned(), value.to_owned()))
                })
                .collect();
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
            let rq = ResultsQuery {
                view,
                filter,
                params,
                top,
                hidden,
                project: project.name().to_owned(),
            };
            results_cmd::cmd_results(dev_config, project_root, &rq)
        }
        Command::CorpusResults {
            run_id,
            run,
            limit,
            probe,
            diffs,
            dispositions,
            scalars,
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
                scalars,
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
            child,
            phase,
            grep,
            call,
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
                child,
                phase,
                grep,
                call,
                range,
                where_cond: r#where,
                fields,
                every,
                head,
                tail,
            };
            sidecar_cmd::cmd_sidecar(dev_config, project_root, project.name(), &sq)
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
                },
            )
        }
        Command::Verify {
            verbose,
            verify:
                VerifyCommand::ElivVerify {
                    dataset,
                    variant,
                    commit,
                    file,
                    geometry_stats,
                    unique_payloads,
                },
        } => {
            let features = resolve_features(dev_config, &[]);
            output::set_quiet(!verbose);
            // `verify pmtiles` only reads an archive, with the current binary,
            // so it belongs to the resolver family (pmtiles-inspect/diag/svg/
            // regress): no historical worktree, and its own `--commit` picks
            // the file. It builds elivagar, so it takes the command lock.
            let _lock = context::reenter_command_lock()?;
            // The build and the verifier child both poll the shutdown flag.
            let _interrupts = crate::shutdown::SigtermGuard::install();
            elivagar::cmd::verify(
                dev_config,
                &detection.project_root,
                &detection.build_root,
                detection.config_sources.included(),
                &dataset,
                &variant,
                commit.as_deref(),
                file.as_deref(),
                &features,
                geometry_stats,
                unique_payloads,
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
            // The lock serializes this with builds: it builds elivagar with
            // cargo in the build tree.
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
            // Regress takes the command lock its spec gives it, though neither
            // the archive resolution nor the in-process diff runs cargo.
            // `operational`: a failure before any verdict (lock, resolution)
            // exits regress's "could not complete" code, not 1.
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
            elivagar::cmd::download_ocean(dev_config, &detection.project_root)
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
            elivagar::cmd::download_natural_earth(dev_config, &detection.project_root)
        }
        Command::PmtilesStats { files } => cmd_pmtiles_stats(project, &files),
        // ----- visual testing commands (litehtml + sluggrs) -----
        Command::Visual { args, litehtml } => {
            // `visual` builds the renderer with cargo, so it takes the global
            // lock like every other build path: the lock is what serialises it
            // against a concurrent measured run (whose timings it would
            // otherwise contaminate) and what activates the armed tree
            // policy. It was the one cargo-driving command reaching
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
            run_measured(&mode, detection, "", "", invocation, |req| {
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
            invocation,
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
            attribute,
            no_gate,
            no_isolate,
            debug,
            release,
            profile,
            keep_artefacts,
            harness_args,
            commit,
        } => {
            let args = piners::cmd::CorpusArgs {
                keywords: keyword,
                probe,
                all,
                verify_only,
                reseed,
                bless,
                attributions: piners::bless::parse_attributions(&attribute)?,
                no_gate,
                no_isolate,
                profile_override: profile_choice(debug, release, profile.as_ref()),
                keep_artefacts,
                harness_args,
            };
            // The parity run (gate + runs.db); corpus has no measured mode
            // (see the `piners` module docs). Under `--commit` only the
            // harness build and the recorded commit move to the worktree;
            // selection, pins, corpus bytes, the harness cwd, runs.db and
            // artefacts stay on the main tree. `--verify-only` and `--reseed`
            // build nothing, so clap refuses `--commit` beside them.
            let piners_cfg = project_config.piners()?;
            with_worktree(
                Some(project),
                &detection.project_root,
                &detection.build_root,
                commit.as_deref(),
                false,
                &crate::tree_policy::TreePolicy::of(dev_config),
                context::worktree_keep(dev_config)?,
                |build_root| {
                    piners::cmd::corpus(project_root, build_root.as_path(), piners_cfg, &args)
                },
            )
        }
    }
}
