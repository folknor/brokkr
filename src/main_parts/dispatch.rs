//! Top-level and project command dispatch.

use super::bootstrap::{
    ServeRequest, WithTarget, require_detection, run_build, run_check, run_clippy, run_deps,
    run_fmt, run_install, run_man, run_program, run_serve, run_wc,
};
use crate::clean::CleanOpts;
use crate::clean::{cargo_clean_package, cmd_clean};
use crate::cli::{Cli, Command};
use crate::command_args::{profile_choice, resolve_features};
use crate::config::project_config::ProjectConfig;
use crate::env::cmd_env;
use crate::error::DevError;
use crate::lock_cmd::{cmd_kill, cmd_lock};
use crate::passthrough_cmd::{RunOptions, cmd_run};
use crate::project::Project;
use crate::request::{ResultsQuery, SidecarQuery};
use crate::visual_approve_cmd::cmd_approve;
use crate::{
    check_cmd, cli, context, crit_cmd, history_cmd, invalidate_cmd, litehtml, lockfile, output, project, results_cmd, runnables, sidecar_cmd, sluggrs, test_cmd,
};

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(super) fn run(
    cli: Cli,
    detection: Option<&project::Detection>,
    invocation: &crate::db::RunInvocation,
) -> Result<(), DevError> {
    let spec = cli::invocation()?.spec();
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
    let _log = match spec.run_log_for(reads_only) {
        Some(kind) => {
            let Some(root) = state_root else {
                return Err(DevError::Internal(
                    "logging commands resolve a state root".to_owned(),
                ));
            };
            Some(crate::run_log::RunLog::begin(root.as_path(), kind))
        }
        None => None,
    };
    let _lock = match lock_policy {
        cli::LockPolicy::None => None,
        cli::LockPolicy::Blocking | cli::LockPolicy::NonBlocking => {
            let Some(project_root) = state_root else {
                return Err(DevError::Internal(
                    "locking commands resolve a state root".to_owned(),
                ));
            };
            let Some(build_root) = detection
                .map(|d| &d.build_root)
                .or_else(|| fallback_roots.as_ref().map(|roots| &roots.1))
            else {
                return Err(DevError::Internal(
                    "locking commands resolve a build root".to_owned(),
                ));
            };
            let ctx = lockfile::LockContext {
                project: project.as_ref().map(Project::name),
                command: cli::invocation()?,
                project_root,
                build_root,
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
            Command::Clean(cli::CleanCmd { dry_run: true, .. }) => crate::maintenance::Mode::Report,
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
        Command::Fmt(cli::FmtCmd { args }) => run_fmt(args, detection),
        Command::Man(cli::ManCmd {
            topic,
            sections,
            full,
        }) => run_man(topic.as_deref(), sections, *full, detection),
        Command::Lock => cmd_lock(),
        Command::Kill(cli::KillCmd {
            hard,
            project: named,
            all,
        }) => cmd_kill(
            &crate::lock_cmd::KillScope::resolve(named.as_deref(), *all, detection)?,
            *hard,
        ),
        Command::Strays(cli::StraysCmd { kill }) => crate::stray::cmd_strays(*kill),
        Command::Guard(cli::GuardCmd { install, remove }) => {
            crate::guard::cmd_guard(*install, *remove)
        }
        Command::Fstrim(cli::FstrimCmd { paths }) => {
            let default = match (paths.is_empty(), detection) {
                (true, Some(d)) => Some(
                    crate::context::bootstrap_config(&d.config, &d.project_root)?.scratch_dir,
                ),
                _ => None,
            };
            crate::fstrim::cmd_fstrim(paths, default)
        }
        Command::History(cli::HistoryCmd {
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
        }) => history_cmd::cmd_history(history_cmd::HistoryQuery {
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

        Command::Run(cli::RunCmd {
            name,
            program,
            args,
        }) => run_program(
            name.as_deref(),
            profile_choice(program.debug, program.release, program.profile.as_ref()),
            &program_features(program),
            program.commit.as_deref(),
            with_target(program).as_ref(),
            args,
            detection,
        ),
        Command::Serve(cli::ServeCmd {
            target,
            program,
            name,
            ready,
            ready_timeout,
            grace,
            stop,
            args,
        }) => {
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
        Command::Build(cli::BuildCmd {
            name,
            debug,
            release,
            profile,
            features,
            all_features,
            no_default_features,
            commit,
        }) => {
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
        Command::Install(cli::InstallCmd { names: true, .. }) => {
            let (bin_cfg, build_root) = match detection {
                Some(d) => (d.config.bin.as_ref(), d.build_root.as_path().to_path_buf()),
                None => (None, std::env::current_dir()?),
            };
            runnables::cmd_install_names(&build_root, bin_cfg)
        }
        Command::Install(cli::InstallCmd {
            debug,
            release,
            profile,
            unlocked,
            names: false,
        }) => run_install(
            profile_choice(*debug, *release, profile.as_ref()),
            *unlocked,
            detection,
        ),
        Command::Wc(cli::WcCmd { threshold }) => run_wc(*threshold, detection),
        Command::Crit(cli::CritCmd {
            target,
            commit,
            compare,
            baselines,
            name,
            lenient,
            args,
        }) => crit_cmd::run(
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
        Command::Bench(cli::BenchCmd {
            commit,
            iterations,
            rounds,
            versus,
            hotpath,
            alloc,
            dry_run,
            test,
            bench,
        }) => crate::bench::runner::run(
            &crate::bench::runner::BenchArgs {
                name: bench.first().cloned(),
                commits: commit.clone(),
                iterations: *iterations,
                rounds: *rounds,
                versus: versus.clone(),
                hotpath: *hotpath,
                alloc: *alloc,
                params: bench.iter().skip(1).cloned().collect(),
                dry_run: *dry_run,
                test: *test,
            },
            require_detection(detection)?,
            invocation,
        ),
        Command::Deps(cli::DepsCmd {
            json,
            no_fail,
            focus,
        }) => run_deps(*json, *no_fail, focus.clone(), detection),
        Command::Check(cli::CheckCmd {
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
        }) => run_check(
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
        Command::Clippy(cli::ClippyCmd {
            package,
            all_features,
            features,
            no_default_features,
            sweep,
            env,
            lib,
        }) => run_clippy(
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
        | Command::Kill(_)
        | Command::Strays(_)
        | Command::Guard(_)
        | Command::Fstrim(_)
        | Command::History(_)
        | Command::Check(_)
        | Command::Clippy(_)
        | Command::Fmt(_)
        | Command::Crit(_)
        | Command::Bench(_)
        | Command::Run(_)
        | Command::Serve(_)
        | Command::Build(_)
        | Command::Install(_)
        | Command::Wc(_)
        | Command::Man(_)
        | Command::Deps(_) => {
            unreachable!("top-level command dispatch must not re-enter through run_project")
        }
        Command::Env => cmd_env(
            dev_config,
            project,
            &detection.project_root,
            &detection.build_root,
            &detection.config_sources,
        ),
        Command::Passthrough(cli::PassthroughCmd {
            features,
            time,
            json,
            runs,
            no_build,
            args,
        }) => {
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
        Command::Results(cli::ResultsCmd {
            query,
            commit,
            compare,
            invocation: rounds_invocation,
            command,
            mode,
            dataset,
            meta,
            env,
            param,
            grep,
            grep_v,
            json,
            limit,
            top,
            hidden,
            retire,
            unretire,
            invalidate,
            revalidate,
            rename,
            reason,
        }) => {
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
            if json && let Some(prefix) = &query {
                return results_cmd::cmd_results_json(dev_config, project.name(), prefix);
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
            let view = if let Some(prefix) = rounds_invocation.clone() {
                crate::request::ResultsView::Invocation { prefix }
            } else if let Some(prefix) = query {
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
        Command::Sidecar(cli::SidecarCmd {
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
        }) => {
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
        Command::Invalidate(cli::InvalidateCmd {
            uuid,
            commit,
            force,
        }) => {
            invalidate_cmd::cmd_invalidate(project_root, uuid.as_deref(), commit.as_deref(), force)
        }
        Command::Clean(cli::CleanCmd {
            worktrees,
            cargo,
            archives,
            keep,
            all,
            dry_run,
        }) => {
            if archives || keep.is_some() {
                return Err(DevError::Usage(
                    "brokkr clean --archives and --keep are retired: elivagar-devkit's \
                     clean prunes elivagar's tilegen archive store"
                        .into(),
                ));
            }
            let _lock = context::reenter_command_lock()?;
            // `--all` folds in the worktrees and cargo sweeps.
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
                    dry_run,
                },
            )
        }
        // ----- visual testing commands (litehtml + sluggrs) -----
        Command::Visual(cli::VisualCmd { args, litehtml }) => {
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
        Command::Test(cli::TestCmd {
            name,
            package,
            repeat,
            jobs,
            debug,
            release,
            timeout,
            sweep,
            timings,
        }) => {
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
        Command::Approve(cli::ApproveCmd { fixture, all }) => cmd_approve(
            project_config,
            project_root,
            build_root,
            detection.config_sources.included(),
            fixture,
            all,
        ),
        Command::Report(cli::ReportCmd { run_id }) => match project {
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
        Command::Prepare(cli::PrepareCmd { input, output }) => {
            let ProjectConfig::Litehtml(cfg) = project_config else {
                unreachable!("command specification admits prepare only for litehtml")
            };
            let _interrupts = crate::shutdown::SigtermGuard::install();
            litehtml::cmd::prepare(project_root, cfg, &input, &output)
        }
        Command::HtmlExtract(cli::HtmlExtractCmd {
            input,
            selector,
            from,
            to,
            output,
        }) => litehtml::cmd::extract(
            project_root,
            &input,
            selector.as_deref(),
            from.as_deref(),
            to.as_deref(),
            &output,
        ),
        Command::Outline(cli::OutlineCmd {
            input,
            depth,
            full,
            selectors,
        }) => litehtml::cmd::outline(project_root, &input, depth, full, selectors),
    }
}
