// Measurement entry point: threads the resolved config/project/dataset/variant
// context through to the closure. The args are the context, not a smell.
#[allow(clippy::too_many_arguments)]
fn run_measured<F>(
    mode: &cli::ModeArgs,
    detection: &project::Detection,
    dataset: &str,
    variant: &str,
    invocation: &crate::db::RunInvocation,
    f: F,
) -> Result<(), DevError>
where
    F: FnOnce(&measure::MeasureRequest) -> Result<(), DevError>,
{
    let dev_config = &detection.config;
    let project = detection.project;
    let project_root = detection.project_root.as_path();
    let requested_mode = resolve_mode(mode);
    let mm = if requested_mode == measure::MeasureMode::Run {
        cli::invocation().spec().default_mode
    } else {
        requested_mode
    };
    let features = resolve_features(dev_config, &mode.features);
    output::set_quiet(!mode.verbose);
    // The narration a measured run keeps off the console (per-run sidecar
    // attach and counts, the hotpath header, worktree and toolchain lines)
    // goes here; without a log it would be discarded.
    let _log = check_cmd::RunLog::begin(project_root, check_cmd::run_log_kind::MEASURE);
    // Detection carries the code tree (cwd) separately from the config/state
    // root, so worktree setup does not have to derive it again.
    context::with_worktree(
        Some(project),
        &detection.project_root,
        &detection.build_root,
        mode.commit.as_deref(),
        mode.dry_run,
        dev_config.disable_toolchain,
        context::worktree_keep(dev_config)?,
        |build_root| {
            let req = measure::MeasureRequest {
                dev_config,
                project_config: &detection.project_config,
                project,
                project_root: &detection.project_root,
                build_root,
                dataset,
                variant,
                features: &features,
                force: mode.force,
                mode: mm,
                invocation,
                dry_run: mode.dry_run,
                stop_marker: mode.stop.as_deref(),
            };
            f(&req)
        },
    )
}

/// Parse argv, with subcommands that don't apply to the detected project
/// hidden from `--help`.
///
/// Hidden is not disabled: a hidden subcommand still parses and still reaches
/// dispatch, which checks the same specification before entering the handler.
/// The help menu and the command refusal therefore use the same applicability
/// rule.
///
/// The shared visual command has a separate `LitehtmlVisualArgs` group. Those
/// options are hidden from sluggrs help and rejected during parsing there, so
/// sluggrs dispatch never needs to receive or interpret them.
///
/// Fails open in every direction. Detection runs before parsing, so it cannot
/// use the CLI, and any failure to detect - no `brokkr.toml`, a malformed one,
/// an unreadable cwd - leaves the full command list visible rather than
/// hiding commands on the strength of a guess. A `brokkr.toml` so broken that
/// the project is unknown must not also cost the user their `--help`.
fn parse_cli(project: Option<Project>) -> (Cli, cli::CommandId) {
    let argv = crate::runnables::bare_run_sentinel(std::env::args().collect());

    let mut cmd = <Cli as clap::CommandFactory>::command();
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();

    for name in names {
        let spec = cli::CommandId::from_name(&name)
            .expect("every clap command has an exhaustive command specification").spec();
        let prefix = spec.help_prefix();
        cmd = cmd.mut_subcommand(&name, |sub| {
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            sub.about(format!("{prefix}{about}"))
                .hide(project.is_some_and(|project| !cli::visible_to(&spec.applies, project)))
        });
    }
    if project == Some(Project::Sluggrs) {
        cmd = cmd.mut_subcommand("visual", |visual| {
            visual
                .mut_arg("suite", |arg| arg.hide(true))
                .mut_arg("recapture", |arg| arg.hide(true))
        });
    }

    let matches = cmd.get_matches_from(argv);
    let Some(command_id) = matches
        .subcommand_name()
        .and_then(cli::CommandId::from_name)
    else {
        clap::Error::raw(
            clap::error::ErrorKind::InvalidSubcommand,
            "parsed subcommand is missing from the command specification",
        )
        .exit();
    };
    let cli = match <Cli as clap::FromArgMatches>::from_arg_matches(&matches) {
        Ok(cli) => cli,
        // The matches came from the same derived command, so a conversion
        // failure is a clap-internal contract break, not user error: let clap
        // render and exit exactly as `parse()` would.
        Err(e) => e.exit(),
    };
    if project == Some(Project::Sluggrs)
        && let Command::Visual { litehtml, .. } = &cli.command
        && (litehtml.suite.is_some() || litehtml.recapture)
    {
        let flag = if litehtml.suite.is_some() {
            "--suite"
        } else {
            "--recapture"
        };
        clap::Error::raw(
            clap::error::ErrorKind::UnknownArgument,
            format!("unexpected argument '{flag}' for sluggrs visual tests"),
        )
        .exit();
    }
    (cli, command_id)
}

fn main() {
    if let Some(code) = test_runner::maybe_run_harness_shim() {
        std::process::exit(code);
    }
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let start = Instant::now();

    // Resolve the project once for help filtering, the toolchain setting, command
    // dispatch and interrupt cleanup. Help still fails open on a bad config;
    // the command decides below whether that detection error is fatal.
    let (detection, detection_error) = match project::detect_optional() {
        Ok(detection) => (detection, None),
        Err(error) => (None, Some(error)),
    };
    let help_project = detection.as_ref().map(|d| d.project);
    let (cli, command_id) = parse_cli(help_project);
    cli::set_invocation(command_id);

    // Lead our own process group before spawning anything or acquiring the
    // lock, so brokkr's internal `kill(-pgid, …)` sweeps can't escape upward
    // into a launcher that spawned us without its own session. No-op for the
    // common interactive-foreground case; see `shutdown::isolate_process_group`.
    shutdown::isolate_process_group();

    // Don't record `history` itself (avoids recursive noise).
    let is_history = matches!(cli.command, Command::History { .. });

    // Handlers return errors rather than exiting: all command-owned guards
    // (lock, disabled toolchain, scratch and streams) drop before we record
    // history and choose the process exit, including propagated child codes.
    // A `[cargo_env]` failure some path tolerated still fails the command.
    let result = crate::cargo_env::seal(match detection_error {
        Some(error) if !detection_error_is_ignored(command_id) => Err(error),
        _ => run(cli, detection.as_ref()),
    });
    let elapsed_ms = duration_ms(start.elapsed());

    let exit_code = crate::error::result_exit_code(&result);

    if !is_history {
        history_cmd::record_history(&raw_args, elapsed_ms, exit_code);
    }

    match result {
        Ok(()) => {}
        Err(DevError::ExitCode(code)) => std::process::exit(code),
        Err(DevError::Interrupted) => {
            output::run_msg("interrupted - running scratch cleanup");
            crate::shutdown::admit_exit_cleanup();
            // Best-effort cleanup from the same config that governed this
            // invocation; the user already has `brokkr clean` as a follow-up
            // when startup could not detect a project.
            if let Some(d) = detection.as_ref()
                && let Err(e) = cmd_clean(
                    &d.config,
                    d.project,
                    &d.project_root,
                    &d.build_root,
                    CleanOpts::after_interrupt(),
                )
            {
                output::error(&format!("cleanup failed: {e}"));
            }
            std::process::exit(130);
        }
        Err(e) => {
            output::error(&e.to_string());
            std::process::exit(1);
        }
    }
}

/// Only commands that never read a project config run past a detection error.
/// `Optional` means a command can run when no config file exists; it does not
/// mean a malformed file may be ignored. Otherwise `check` could run without
/// the project's gates and report green.
fn detection_error_is_ignored(command: cli::CommandId) -> bool {
    command.spec().config == cli::ConfigNeed::None
}

fn require_detection(
    detection: Option<&project::Detection>,
) -> Result<&project::Detection, DevError> {
    match detection {
        Some(detection) => Ok(detection),
        None => {
            let cwd = std::env::current_dir().map_err(|e| {
                DevError::Config(format!("cannot determine current directory: {e}"))
            })?;
            Err(DevError::Config(format!(
                "no brokkr.toml found in {} or its parent directory",
                cwd.display()
            )))
        }
    }
}

/// `cargo fmt` runs rustfmt from the pinned toolchain, so honour
/// disable_toolchain. When it's set we must move the pin aside; do it by
/// riding the global lock's activation of the configured build root - the same
/// mechanism every build path uses - rather than a bare guard. A bare guard
/// would race a concurrent locked build that had already moved the same file
/// aside: fmt would adopt that build's sidecar and restore it mid-command.
/// Under the lock, fmt and the build can't overlap. When disable_toolchain is
/// off there is nothing to move, so fmt stays lock-free as before.
fn run_fmt(args: &[String], detection: Option<&project::Detection>) -> Result<(), DevError> {
    match detection {
        Some(d) if d.config.disable_toolchain => {
            let lock = context::reenter_command_lock()?;
            cmd_fmt(args, Some(&lock))
        }
        _ => cmd_fmt(args, None),
    }
}

/// Reading the docs must work in a tree brokkr knows nothing about, so a
/// missing or malformed config gets the project-agnostic topics. No lock:
/// this reads compiled-in strings and touches nothing on disk.
fn run_man(
    topic: Option<&str>,
    sections: &[String],
    full: bool,
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let project = detection.map(|d| d.project);
    man::run(topic, sections, full, project)
}

/// Like `run`, `install` builds the code tree (cwd), riding the same lock +
/// toolchain-disable window. The session-workflow closer: install the
/// workspace's bins without reaching for raw cargo.
fn run_install(
    debug: bool,
    release: bool,
    unlocked: bool,
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let (bin_cfg, project_root) = match detection {
        Some(d) => (d.config.bin.as_ref(), d.build_root.as_path().to_path_buf()),
        None => (None, std::env::current_dir()?),
    };
    let lock = context::reenter_command_lock()?;
    runnables::cmd_install(&project_root, bin_cfg, debug, release, unlocked, Some(&lock))
}

/// `wc` lists source files in the code tree (cwd).
fn run_wc(threshold: usize, detection: Option<&project::Detection>) -> Result<(), DevError> {
    match detection {
        Some(d) => wc::run(d.build_root.as_path(), threshold),
        None => {
            let cwd = std::env::current_dir()?;
            wc::run(&cwd, threshold)
        }
    }
}

/// `run` builds and runs the code, so it anchors on the build root (cwd).
/// Detection also supplies the `[bin]` section - target resolution itself
/// comes from cargo metadata, so `run` still works with no brokkr.toml at all.
fn run_program(
    name: Option<&str>,
    debug: bool,
    release: bool,
    feat: &runnables::FeatureArgs<'_>,
    commit: Option<&str>,
    args: &[String],
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let Some(commit) = commit else {
        let (bin_cfg, project_root) = match detection {
            Some(d) => (d.config.bin.as_ref(), d.build_root.as_path().to_path_buf()),
            None => (None, std::env::current_dir()?),
        };
        let lock = context::reenter_command_lock()?;
        return runnables::cmd_run(
            &project_root,
            bin_cfg,
            name,
            debug,
            release,
            feat,
            args,
            Some(&lock),
        );
    };
    // `--commit`: `bench`'s shape. The worktree is cut from the build
    // root, its bookkeeping lives under the project root, and the lock is
    // taken inside the closure so the toolchain-disable lands on the
    // worktree rather than the live tree.
    let fallback_roots = project::detect_roots()?;
    let (project, bin_cfg, project_root, build_root, disable_toolchain, keep) = match detection {
        Some(d) => {
            let keep = context::worktree_keep(&d.config)?;
            (
                Some(d.project),
                d.config.bin.as_ref(),
                &d.project_root,
                &d.build_root,
                d.config.disable_toolchain,
                keep,
            )
        }
        None => {
            (None, None, &fallback_roots.0, &fallback_roots.1, false, crate::worktree_record::DEFAULT_KEEP)
        }
    };
    context::with_worktree(
        project,
        project_root,
        build_root,
        Some(commit),
        false,
        disable_toolchain,
        keep,
        |wt| {
            let worktree = wt.as_path();
            let lock = context::reenter_command_lock()?;
            runnables::cmd_run_commit(
                worktree,
                build_root.as_path(),
                bin_cfg,
                name,
                debug,
                release,
                feat,
                args,
                &lock,
            )
        },
    )
}

/// `sync`: bare lists, a script runs, `--bench` measures, `--all` and
/// `--gate all` run a cohort.
#[allow(clippy::too_many_arguments)]
fn run_sync(
    script: &Option<String>,
    all: &bool,
    filter: &Option<String>,
    include_ignored: &bool,
    bench: &Option<usize>,
    force: &bool,
    keep_artefacts: &bool,
    debug: &bool,
    release: &bool,
    gate: &Option<String>,
    as_baseline: &bool,
    commit: &Option<String>,
    detection: &project::Detection,
    invocation: &crate::db::RunInvocation,
) -> Result<(), DevError> {
    let project = detection.project;
    let dev_config = &detection.config;
    let project_root = detection.project_root.as_path();
    let ratatoskr_config = detection.project_config.ratatoskr()?;
    let cohort_gate = gate.as_deref() == Some(ratatoskr::sync::GATE_ALL);

    // Refuse before anything is built: a cohort rebase re-anchors
    // every baseline-relative rule at once, and a gated sweep
    // supplies its own scripts.
    ratatoskr::sync::validate_gate_selection(gate.as_deref(), script.is_some(), *as_baseline)?;
    // A named gate must exist before a `--commit` worktree is cut for it.
    // (A gate literally named `all` is refused when the config loads.)
    if let Some(name) = gate.as_deref()
        && !cohort_gate
        && !ratatoskr_config.gate.contains_key(name)
    {
        return Err(DevError::Config(format!(
            "sync: gate `{name}` not found in [ratatoskr.gate.*] of brokkr.toml"
        )));
    }

    // Modes off one shape: --all runs the discovered cohort,
    // --gate all runs the configured gate cohort, no script
    // lists, a script runs, a script plus --bench measures.
    if *all {
        return ratatoskr::sync::run_sync_all(&ratatoskr::sync::SyncAllRequest {
            project_root,
            build_root: &detection.build_root,
            ratatoskr: ratatoskr_config,
            filter: filter.as_deref(),
            include_ignored: *include_ignored,
            keep_artefacts: *keep_artefacts,
            profile_override: profile_override(*debug, *release),
        });
    }
    if script.is_none() && !cohort_gate {
        if bench.is_some() {
            return Err(DevError::Usage(
                "sync: --bench needs a SCRIPT to measure (or --gate all to \
                 sweep every configured gate)"
                    .into(),
            ));
        }
        return ratatoskr::sync::run_sync_list(project_root, ratatoskr_config);
    }
    // `--gate all` implies `--bench` (clap `requires`), so a
    // missing bench here means the plain script-run shape.
    let Some(bench) = *bench else {
        let script = script.as_deref().unwrap_or_default();
        return ratatoskr::sync::run_sync_smoke(&ratatoskr::sync::SyncSmokeRequest {
            project_root,
            build_root: &detection.build_root,
            ratatoskr: ratatoskr_config,
            script,
            keep_artefacts: *keep_artefacts,
            profile_override: profile_override(*debug, *release),
        });
    };
    // A sync bench runs the sidecar, whose per-run narration is
    // run-log only: give it the measured runs' log.
    let _log = check_cmd::RunLog::begin(project_root, check_cmd::run_log_kind::MEASURE);
    with_worktree(
        Some(project),
        &detection.project_root,
        &detection.build_root,
        commit.as_deref(),
        false,
        dev_config.disable_toolchain,
        context::worktree_keep(dev_config)?,
        |build_root| {
            let req = ratatoskr::sync::SyncBenchRequest {
                project_root: &detection.project_root,
                build_root,
                dev_config,
                ratatoskr: ratatoskr_config,
                // Empty for the cohort: `run_gate_cohort`
                // substitutes each gate's configured script.
                script: script.as_deref().unwrap_or_default(),
                bench,
                force: *force,
                keep_artefacts: *keep_artefacts,
                profile_override: profile_override(*debug, *release),
                invocation: invocation.clone(),
                gate: gate.as_deref(),
                as_baseline: *as_baseline,
            };
            if cohort_gate {
                ratatoskr::sync::run_gate_cohort(&req)
            } else {
                ratatoskr::sync::run_sync_bench(&req)
            }
        },
    )
}

/// `--force` is a measured-run flag (run on a dirty tree, handled by
/// run_measured/MeasurementSession). A corpus parity run has nothing for it to
/// override, and a flag accepted and ignored would read as if it did something.
fn refuse_unmeasured_force(mode: &cli::ModeArgs) -> Result<(), DevError> {
    if mode.force && resolve_mode(mode) == measure::MeasureMode::Run {
        return Err(DevError::Usage(
            "corpus: `--force` applies to a measured run (`--hotpath`/`--alloc`) only; \
             a parity run has no runtime limit to bypass"
                .into(),
        ));
    }
    Ok(())
}

/// `check` builds, scans, and diffs the code tree, so it anchors on the build
/// root (cwd). The config values it uses (`[[check]]` sweeps, dependency rules,
/// gremlin excludes) come from wherever brokkr.toml was found, in cwd or one
/// level up. The loaded Detection stays available to main's interrupt cleanup;
/// the no-config path still composes the user-wide rules on their own.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn run_check(
    features: &[String],
    no_default_features: &bool,
    package: &[String],
    profile: &Option<String>,
    gate: &bool,
    force_rust: &bool,
    json: &bool,
    fix_gremlins: &bool,
    timings: &bool,
    commands: &bool,
    textlint_names: &[String],
    script_names: &[String],
    args: &[String],
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let no_config_roots = project::detect_roots()?;
    let user_config = if detection.is_none() {
        Some(config::load_user_only()?)
    } else {
        None
    };
    let project = detection.map(|d| d.project);
    let (
        check_entries,
        dependency_rules,
        quarantine,
        test_cfg,
        bin_cfg,
        gremlins_cfg,
        header_cfg,
        textlint_rules,
        script_checks,
        manifest_cfg,
        rustdoc_cfg,
        clippy_cfg,
    ) = if let Some(d) = detection {
        (
            d.config.check.as_slice(),
            d.config.dependency_rules.as_slice(),
            d.config.quarantine.as_slice(),
            d.config.test.as_ref(),
            d.config.bin.as_ref(),
            d.config.gremlins.as_ref(),
            d.config.header.as_ref(),
            d.config.textlint.as_slice(),
            d.config.script_checks.as_slice(),
            d.config.manifest.as_ref(),
            d.config.rustdoc.as_ref(),
            d.config.lints.as_ref(),
        )
    } else {
        // No project config, but the user-wide layer applies to any tree
        // brokkr checks. A repo without brokkr.toml is exactly where a
        // personal convention may be the only rule in force.
        let user = user_config.as_ref().expect("the unconfigured branch loaded user config");
        (
            &[][..],
            &[][..],
            &[][..],
            None,
            None,
            None,
            None,
            user.textlint.as_slice(),
            user.script_checks.as_slice(),
            None,
            None,
            None,
        )
    };
    // cargo/git run in the code tree; check's `.brokkr` state anchors to
    // the config dir. They differ under the one-level-up layout.
    let (project_root, state_root) = match detection {
        Some(d) => (d.build_root.as_path(), d.project_root.as_path()),
        None => {
            (no_config_roots.1.as_path(), no_config_roots.0.as_path())
        }
    };
    // The run log is reserved before the lock and opened under it, so its narration
    // lands in it, and closes after the watchdog's join (declared first,
    // dropped last).
    let _log = check_cmd::RunLog::begin(state_root, check_cmd::run_log_kind::CHECK);
    let _lock = context::reenter_command_lock()?;
    // The whole-run ceiling, armed once the lock is ours so a wait behind
    // another brokkr command is not charged against it. Fires by
    // SIGKILLing every descendant and letting this command unwind to
    // exit 124. Declared after `_lock`, so it disarms before the lock's
    // drop restores the toolchain pin.
    let _ceiling = check_cmd::CheckWatchdog::arm(check_cmd::CHECK_CEILING);
    // The `[lints]` config, loaded once; a tree without one allows nothing.
    let no_lints = config::LintsConfig::default();
    let lints = clippy_cfg.unwrap_or(&no_lints);
    if !textlint_names.is_empty() || !script_names.is_empty() {
        return check_cmd::cmd_check_selected(
            project_root,
            textlint_rules,
            script_checks,
            textlint_names,
            script_names,
        );
    }
    check_cmd::cmd_check(
        project,
        project_root,
        state_root,
        check_entries,
        dependency_rules,
        quarantine,
        test_cfg,
        bin_cfg,
        gremlins_cfg,
        header_cfg,
        textlint_rules,
        script_checks,
        manifest_cfg,
        rustdoc_cfg,
        lints,
        features,
        *no_default_features,
        package,
        profile.as_deref(),
        *gate,
        *force_rust,
        *json,
        *fix_gremlins,
        *timings,
        *commands,
        args,
    )
}

/// Like `check`, `clippy` builds the code tree (cwd), reading `[[check]]`
/// env/features from wherever brokkr.toml was found. The disable dir armed in
/// `run` activates when `acquire_cmd_lock_opt` takes the global lock below, so
/// the clippy build honours pin-suppression. Detection is optional - `clippy`
/// runs in any Rust+git repo with no config.
#[allow(clippy::too_many_arguments)]
fn run_clippy(
    package: &[String],
    all_features: &bool,
    features: &[String],
    no_default_features: &bool,
    sweep: &Option<String>,
    env: &[String],
    lib: &bool,
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let no_config_roots = project::detect_roots()?;
    let (check_entries, clippy_cfg, project_root) = match detection {
        Some(d) => (
            d.config.check.as_slice(),
            d.config.lints.as_ref(),
            d.build_root.as_path(),
        ),
        None => (
            &[][..],
            None,
            no_config_roots.1.as_path(),
        ),
    };
    let env_overrides = parse_env_overrides(env)?;
    let _lock = context::reenter_command_lock()?;
    let no_lints = config::LintsConfig::default();
    check_cmd::cmd_clippy(
        project_root,
        check_entries,
        package,
        *all_features,
        features,
        *no_default_features,
        sweep.as_deref(),
        *lib,
        &env_overrides,
        clippy_cfg.unwrap_or(&no_lints),
    )
}

/// `deps` audits the code tree's Cargo.lock / metadata (cwd). It shells out to
/// `cargo metadata` and `ccu`/`rustc`, all rustup-mediated, so it must take the
/// global lock like the other build paths: the lock is what activates the armed
/// toolchain-disable, moving a foreign checkout's uninstalled pin aside for the
/// window these tools run in.
fn run_deps(
    json: bool,
    no_fail: bool,
    focus: Option<String>,
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let (project_root, workspace_dep_ignore) = match detection {
        Some(d) => (
            d.build_root.as_path().to_path_buf(),
            d.config
                .deps
                .as_ref()
                .map(|c| c.workspace_dep_ignore.clone())
                .unwrap_or_default(),
        ),
        None => (std::env::current_dir()?, Vec::new()),
    };
    let _lock = context::reenter_command_lock()?;
    deps::run(
        &project_root,
        &deps::DepsArgs {
            json,
            no_fail,
            focus,
            workspace_dep_ignore,
        },
    )
}

// The command table: one arm per subcommand, so its size tracks the CLI's.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run(cli: Cli, detection: Option<&project::Detection>) -> Result<(), DevError> {
    let spec = cli::invocation().spec();
    if spec.config == cli::ConfigNeed::Required {
        spec.require(require_detection(detection)?.project)?;
    } else if let Some(detection) = detection {
        spec.require(detection.project)?;
    }
    let fallback_roots = if detection.is_none() && spec.config != cli::ConfigNeed::None {
        Some(project::detect_roots()?)
    } else { None };
    let state_root = detection.map(|d| &d.project_root)
        .or_else(|| fallback_roots.as_ref().map(|roots| &roots.0));
    let project = detection.map(|d| d.project);
    let _log = spec.run_log.map(|kind|
        crate::run_log::RunLog::begin(state_root.expect("logging commands resolve a state root").as_path(), kind));
    let _lock = match spec.lock {
        cli::LockPolicy::None => None,
        cli::LockPolicy::Blocking | cli::LockPolicy::NonBlocking => {
            let ctx = lockfile::LockContext {
                project: project.map_or("brokkr", Project::name),
                command: cli::invocation(),
                project_root: state_root.expect("locking commands resolve a state root"),
                build_root: detection.map(|d| &d.build_root)
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
    let disable_dir = detection.filter(|d| d.config.disable_toolchain)
        .map(|d| d.build_root.as_path().to_path_buf());
    context::set_configured_toolchain_dir(disable_dir);
    let historical = cli.command.uses_worktree();
    let _toolchain = if !historical && spec.lock != cli::LockPolicy::None {
        detection.filter(|d| d.config.disable_toolchain)
            .map(|d| toolchain::DisabledToolchain::activate(d.build_root.as_path())).transpose()?
    } else { None };
    let _toolchain_registration = _toolchain.as_ref().and_then(|_| {
        _lock.as_ref().map(|lock| lock.track_toolchain_dir(
            detection.expect("toolchain suppression requires config").build_root.as_path()))
    });
    match &cli.command {
        Command::Fmt { args } => run_fmt(args, detection),
        Command::Man { topic, sections, full } => {
            run_man(topic.as_deref(), sections, *full, detection)
        }
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
                *debug,
                *release,
                &feat,
                commit.as_deref(),
                args,
                detection,
            )
        }
        Command::Install {
            debug,
            release,
            unlocked,
        } => run_install(*debug, *release, *unlocked, detection),
        Command::Wc { threshold } => run_wc(*threshold, detection),
        Command::Bench { target, commit, compare, baselines, name, lenient, args } =>
            bench_cmd::run(&bench_cmd::BenchArgs {
                target: target.clone(), commit: commit.clone(),
                compare: compare.clone().map(|values| crate::request::compare_pair(values, "--compare")).transpose()?,
                baselines: *baselines, name: name.clone(), lenient: *lenient, args: args.clone(),
            }),
        Command::Deps { json, no_fail, focus } => run_deps(*json, *no_fail, focus.clone(), detection),
        Command::Check { features, no_default_features, package, profile, gate, force_rust, json, fix_gremlins, timings, commands, textlint_names, script_names, args } => run_check(features, no_default_features, package, profile, gate, force_rust, json, fix_gremlins, timings, commands, textlint_names, script_names, args, detection),
        Command::Clippy { package, all_features, features, no_default_features, sweep, env, lib } => run_clippy(package, all_features, features, no_default_features, sweep, env, lib, detection),
        _ => run_project(cli.command, require_detection(detection)?),
    }
}

// The project command table: one arm per subcommand, so its size tracks the CLI's.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn run_project(command: Command, detection: &project::Detection) -> Result<(), DevError> {
    let project = detection.project;
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
        command_input::RoutedCommand::Pbf { mode, pbf, command, osc, mut params } => {
            params.io = crate::pbfhogg::context::IoMode {
                direct_io: pbf.direct_io,
                io_uring: pbf.io_uring,
            };
            params.compression = pbf.compression.clone();
            return run_measured(&mode, detection, &pbf.dataset, &pbf.variant, &invocation,
                |req| measure::runner::run(req, &pbfhogg::plan::PbfhoggPlan {
                    command: &command, osc_seq: osc.as_deref(), params: &params,
                }));
        }
        command_input::RoutedCommand::Other(command) => *command,
    };
    match command {
        Command::Lock | Command::Kill { .. } | Command::Strays { .. }
        | Command::Guard { .. } | Command::History { .. } | Command::Check { .. }
        | Command::Clippy { .. } | Command::Fmt { .. } | Command::Bench { .. }
        | Command::Run { .. } | Command::Install { .. } | Command::Wc { .. }
        | Command::Man { .. } | Command::Deps { .. }
        | Command::Inspect { .. } | Command::CheckRefs { .. } | Command::CheckIds { .. }
        | Command::Sort { .. } | Command::Cat { .. } | Command::TagsFilter { .. }
        | Command::Getid { .. } | Command::Getparents { .. } | Command::Renumber { .. }
        | Command::MergeChanges { .. } | Command::ApplyChanges { .. }
        | Command::AddLocationsToWays { .. } | Command::Repack { .. }
        | Command::Degrade { .. } | Command::TimeFilter { .. } | Command::Diff { .. }
        | Command::BuildGeocodeIndex { .. } => unreachable!(
            "top-level command dispatch must not re-enter through run_project"
        ),
        Command::Env => cmd_env(dev_config, project, &detection.project_root, &detection.build_root),
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
            run_measured(
                &mode,
                detection,
                &dataset,
                &variant,
                &invocation,
                |req| {
                    let cmd = pbfhogg::commands::PbfhoggCommand::DiffSnapshots { format };
                    measure::runner::run(req, &pbfhogg::plan::PbfhoggPlan { command: &cmd, osc_seq: None, params: &params })
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
                            measure::runner::run(req, &pbfhogg::plan::PbfhoggPlan { command: &cmd, osc_seq: None, params: &params })?;
                        }
                        Ok(())
                    } else {
                        let strat = pbfhogg::commands::ExtractStrategy::parse(&strategy)?;
                        let cmd = pbfhogg::commands::PbfhoggCommand::Extract { strategy: strat };
                        measure::runner::run(req, &pbfhogg::plan::PbfhoggPlan { command: &cmd, osc_seq: None, params: &params })
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
                            measure::runner::run(req, &pbfhogg::plan::PbfhoggPlan { command: &cmd, osc_seq: None, params: &params })?;
                        }
                        Ok(())
                    } else {
                        let strat = pbfhogg::commands::ExtractStrategy::parse(&strategy)?;
                        let cmd = pbfhogg::commands::PbfhoggCommand::MultiExtract {
                            regions,
                            strategy: strat,
                        };
                        measure::runner::run(req, &pbfhogg::plan::PbfhoggPlan { command: &cmd, osc_seq: None, params: &params })
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
            |req| pbfhogg::cmd::bench_read(req, &modes, snapshot.as_deref()),
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
            // The pipeline contract comes from the named block. Input
            // assertions are attached later to the resolved PBF entry.
            let tilegen = elivagar::resolve_tilegen(dev_config, elivagar::DEFAULT_TILEGEN)?;
            let cmd = elivagar::commands::ElivagarCommand::Built(
                elivagar::commands::BuiltCommand::MainBinary {
                    tilegen,
                    skip_to: skip_to.as_deref(),
                },
            );
            run_measured(
                &mode,
                detection,
                &dataset,
                &variant,
                &invocation,
                |req| measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd)),
            )
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
            run_measured(
                &mode,
                detection,
                &dataset,
                &variant,
                &invocation,
                |req| measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd)),
            )
        }
        Command::ElivTilemaker {
            mode,
            dataset,
            variant,
        } => {
            let cmd = elivagar::commands::ElivagarCommand::External(
                elivagar::commands::ExternalTool::Tilemaker,
            );
            run_measured(
                &mode,
                detection,
                &dataset,
                &variant,
                &invocation,
                |req| measure::runner::run(req, &elivagar::plan::ElivagarPlan(&cmd)),
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
            run_measured(
                &mode,
                detection,
                &dataset,
                &variant,
                &invocation,
                |req| measure::runner::run(req, &nidhogg::plan::NidhoggPlan(&cmd)),
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
        Command::Dellingr { mode, lua } => run_measured(
            &mode,
            detection,
            "",
            "",
            &invocation,
            |req| dellingr::cmd::run(req, &lua),
        ),

        // ----- mogwai commands -----
        // Dataset/variant stay empty: an input a mogwai surface reads is named
        // in its argv, which the row captures verbatim, so filling these would
        // duplicate it under a second name that pairing also keys on.
        Command::Mogwai { mode, target, args } => run_measured(
            &mode,
            detection,
            "",
            "",
            &invocation,
            |req| mogwai::cmd::run(req, target.as_deref(), &args),
        ),

        // ----- sluggrs commands -----
        // ----- generic commands -----
        Command::GenericHotpath {
            mode,
            dataset,
            variant,
        } => run_measured(
            &mode,
            detection,
            &dataset,
            &variant,
            &invocation,
            |req| {
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
            detection,
            &dataset,
            &variant,
            &invocation,
            |req| {
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
                        project::require(project, Project::Pbfhogg, "suite pbfhogg")?;
                        pbfhogg::cmd::bench_all(req)
                    }
                    "elivagar" => {
                        project::require(project, Project::Elivagar, "suite elivagar")?;
                        elivagar::cmd::bench_all(req)
                    }
                    "nidhogg" => {
                        project::require(project, Project::Nidhogg, "suite nidhogg")?;
                        measure::runner::run(req, &nidhogg::plan::NidhoggPlan(&nidhogg::commands::NidhoggCommand::Api { query: None }))?;
                        measure::runner::run(req, &nidhogg::plan::NidhoggPlan(&nidhogg::commands::NidhoggCommand::Ingest))
                    }
                    other => Err(DevError::Usage(format!(
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
            let rq = ResultsQuery {
                view,
                filter,
                top,
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
        Command::Invalidate { uuid, commit, force } => {
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
            let cargo = if all { Some(cargo.unwrap_or(None)) } else { cargo };
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
                    verify,
                    &features,
                    verbose,
                );
            }
            with_worktree(Some(project), &detection.project_root, &detection.build_root, commit.as_deref(), false, dev_config.disable_toolchain, context::worktree_keep(dev_config)?, |build_root| {
                cmd_verify(
                    dev_config,
                    project,
                    &detection.project_root,
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
            let _lock = context::reenter_command_lock()?;
            pbfhogg::cmd::download(
                dev_config,
                project,
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
            elivagar::cmd::compare_tiles(project, &file_a, &file_b, sample)
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
            elivagar::cmd::inspect(
                dev_config,
                project,
                &detection.project_root,
                &detection.build_root,
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
            elivagar::cmd::diag(
                dev_config,
                project,
                &detection.project_root,
                &detection.build_root,
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
            elivagar::cmd::svg(
                dev_config,
                project,
                &detection.project_root,
                &detection.build_root,
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
                    project,
                    &detection.project_root,
                    &detection.build_root,
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
                    project,
                    &detection.project_root,
                    &detection.build_root,
                    &cmd,
                )
            })
        }
        Command::DownloadOcean => {
            let _lock = context::reenter_command_lock()?;
            elivagar::cmd::download_ocean(dev_config, project, &detection.project_root, &detection.build_root)
        }
        Command::OceanBuild { dry_run } => {
            let _lock = context::reenter_command_lock()?;
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
            elivagar::cmd::download_natural_earth(dev_config, project, &detection.project_root, &detection.build_root)
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
                project,
                &detection.project_root,
                &detection.build_root,
                data_dir.as_deref(),
                &dataset,
                tiles.as_deref(),
                &features,
            )
        }
        Command::Stop => nidhogg::cmd::stop(project, project_root),
        Command::Status => nidhogg::cmd::status(dev_config, project, project_root),
        Command::Ingest { variant, dataset } => {
            let _lock = context::reenter_command_lock()?;
            let features = resolve_features(dev_config, &[]);
            nidhogg::cmd::ingest(
                dev_config,
                project,
                &detection.project_root,
                &detection.build_root,
                &variant,
                &dataset,
                &features,
            )
        }
        Command::Update { args } => {
            let _lock = context::reenter_command_lock()?;
            let features = resolve_features(dev_config, &[]);
            nidhogg::cmd::update(project, project_root, build_root, &args, &features)
        }
        Command::Query { json } => {
            nidhogg::cmd::query(dev_config, project, project_root, json.as_deref())
        }
        Command::Geocode { term } => {
            nidhogg::cmd::geocode(dev_config, project, project_root, &term)
        }
        // ----- visual testing commands (litehtml + sluggrs) -----
        Command::Visual { args, litehtml } => {
            // `visual` builds the renderer with cargo, so it takes the global
            // lock like every other build path: the lock is what serialises it
            // against a concurrent measured run (whose timings it would
            // otherwise contaminate) and what activates the armed
            // toolchain-disable. It was the one cargo-driving command reaching
            // its handler unlocked.
            let _lock = context::reenter_command_lock()?;
            match project_config {
                ProjectConfig::Litehtml(litehtml_config) => litehtml::cmd::test(
                    project,
                    project_root,
                    build_root,
                    litehtml_config,
                    &args,
                    &litehtml,
                ),
                ProjectConfig::Sluggrs(sluggrs_config) => sluggrs::cmd::test(
                    project,
                    project_root,
                    build_root,
                    sluggrs_config,
                    &args,
                ),
                _ => unreachable!("command specification admits visual only for litehtml/sluggrs"),
            }
        }
        // ----- cargo single-test runner -----
        Command::Test { name, package, repeat, jobs, debug, release, timeout, sweep } => {
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
            test_cmd::run(dev_config, project, build_root, project_root, &name, package.as_deref(), repeat, jobs, profile_override(debug, release), timeout, sweep.as_deref())
        }
        Command::List => {
            match project_config {
                ProjectConfig::Litehtml(cfg) => litehtml::cmd::list(project, project_root, cfg),
                ProjectConfig::Sluggrs(cfg) => sluggrs::cmd::list(project, project_root, cfg),
                _ => unreachable!("command specification admits list only for litehtml/sluggrs"),
            }
        }
        Command::Approve { fixture, all } => {
            cmd_approve(project_config, project, project_root, build_root, fixture, all)
        }
        Command::Report { run_id } => {
            match project {
                Project::Litehtml => litehtml::cmd::report(project, project_root, &run_id),
                Project::Sluggrs => sluggrs::cmd::report(project, project_root, &run_id),
                _ => unreachable!("command specification admits report only for litehtml/sluggrs"),
            }
        }
        Command::VisualStatus => {
            match project_config {
                ProjectConfig::Litehtml(cfg) => litehtml::cmd::status(project, project_root, cfg),
                ProjectConfig::Sluggrs(cfg) => sluggrs::cmd::status(project, project_root, cfg),
                _ => unreachable!("command specification admits visual-status only for litehtml/sluggrs"),
            }
        }
        // ----- litehtml-only commands -----
        Command::Prepare { input, output } => {
            let ProjectConfig::Litehtml(cfg) = project_config else {
                unreachable!("command specification admits prepare only for litehtml")
            };
            litehtml::cmd::prepare(project, project_root, cfg, &input, &output)
        }
        Command::HtmlExtract { input, selector, from, to, output } => {
            litehtml::cmd::extract(project, project_root, &input, selector.as_deref(), from.as_deref(), to.as_deref(), &output)
        }
        Command::Outline { input, depth, full, selectors } => {
            litehtml::cmd::outline(project, project_root, &input, depth, full, selectors)
        }
        // ----- sluggrs-only commands -----
        Command::Hotpath { mode, target } => {
            run_measured(
                &mode,
                detection,
                "",
                "",
                &invocation,
                |req| sluggrs::hotpath::cmd(req, &target),
            )
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
            repeat,
            keep_going,
        } => {
            // Same bare-is-an-index shape as `sync`: bare lists, SCRIPT
            // runs one (a directory runs that cohort), --all runs the
            // discovered cohort.
            if all {
                ratatoskr::cmd::service_suite(
                    &crate::request::ServiceSuiteRequest {
                        project_root,
                        build_root: &detection.build_root,
                        ratatoskr: project_config.ratatoskr()?,
                        filter: filter.as_deref(),
                        keep_artefacts,
                        profile_override: profile_override(debug, release),
                        keep_going,
                        include_ignored,
                        repeat,
                    },
                )
            } else if let Some(script) = script {
                ratatoskr::cmd::service_test(
                    &crate::request::ServiceTestRequest {
                        project_root,
                        build_root: &detection.build_root,
                        ratatoskr: project_config.ratatoskr()?,
                        script: &script,
                        keep_artefacts,
                        profile_override: profile_override(debug, release),
                        repeat,
                        keep_going,
                    },
                )
            } else {
                ratatoskr::cmd::service_list(project_root)
            }
        }
        Command::Sync { script, all, filter, include_ignored, bench, force, keep_artefacts, debug, release, gate, as_baseline, commit } => run_sync(&script, &all, &filter, &include_ignored, &bench, &force, &keep_artefacts, &debug, &release, &gate, &as_baseline, &commit, detection, &invocation),
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
                profile_override: profile_override(debug, release),
                keep_artefacts,
                harness_args,
            };
            if resolve_mode(&mode) != measure::MeasureMode::Run {
                // --hotpath/--alloc: build the harness with the hotpath feature
                // and record to results.db, skipping the gate and runs.db.
                run_measured(
                    &mode,
                    detection,
                    "",
                    "",
                    &invocation,
                    |req| piners::measured::run(req, &args),
                )
            } else {
                // Bare corpus: the parity run (gate + runs.db), unchanged.
                piners::cmd::corpus(project_root, project_config.piners()?, &args)
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
            piners::lint::cmd::lint_corpus(project_root, project_config.piners()?, &args)
        }
        Command::LintResults {
            run_id,
            run,
            limit,
            full,
        } => {
            let lq = piners::lint::query::LintQuery {
                run_id,
                run,
                limit,
                full,
            };
            piners::lint::query::cmd(project_root, &lq)
        }
    }
}
