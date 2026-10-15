//! Startup, parsing and command-context helpers.

use super::dispatch::run;
use crate::clean::CleanOpts;
use crate::clean::cmd_clean;
use crate::cli::{Cli, Command};
use crate::command_args::{parse_env_overrides, profile_override, resolve_features, resolve_mode};
use crate::context::with_worktree;
use crate::error::DevError;
use crate::fmt_cmd::cmd_fmt;
use crate::passthrough_cmd::duration_ms;
use crate::project::Project;
use crate::{
    check_cmd, cli, config, context, deps, history_cmd, man, measure, output, project, ratatoskr,
    runnables, shutdown, test_runner, wc,
};
use std::time::Instant;

// Measurement entry point: threads the resolved config/project/dataset/variant
// context through to the closure. The args are the context, not a smell.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_measured<F>(
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
    let project = detection.project.clone();
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
        Some(&project),
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
                project: project.clone(),
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
fn parse_cli(project: Option<&Project>) -> (Cli, cli::CommandId) {
    let argv = crate::runnables::bare_run_sentinel(std::env::args().collect());

    let mut cmd = <Cli as clap::CommandFactory>::command();
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();

    for name in names {
        let spec = cli::CommandId::from_name(&name)
            .expect("every clap command has an exhaustive command specification")
            .spec();
        let prefix = spec.help_prefix();
        cmd = cmd.mut_subcommand(&name, |sub| {
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            sub.about(format!("{prefix}{about}"))
                .hide(project.is_some_and(|project| !cli::visible_to(&spec.applies, project)))
        });
    }
    if matches!(project, Some(Project::Sluggrs)) {
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
    if matches!(project, Some(Project::Sluggrs))
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

pub(crate) fn main() {
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
    let (cli, command_id) = parse_cli(detection.as_ref().map(|d| &d.project));
    cli::set_invocation(command_id);

    // Lead our own process group before spawning anything or acquiring the
    // lock, so brokkr's internal `kill(-pgid, ...)` sweeps can't escape upward
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
        history_cmd::record_history(&raw_args, elapsed_ms, exit_code, detection.as_ref());
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
                    &d.project,
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

pub(super) fn require_detection(
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
pub(super) fn run_fmt(
    args: &[String],
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
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
pub(super) fn run_man(
    topic: Option<&str>,
    sections: &[String],
    full: bool,
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let project = detection.map(|d| d.project.clone());
    man::run(topic, sections, full, project)
}

/// Like `run`, `install` builds the code tree (cwd), riding the same lock +
/// toolchain-disable window. The session-workflow closer: install the
/// workspace's bins without reaching for raw cargo.
pub(super) fn run_install(
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
    runnables::cmd_install(
        &project_root,
        bin_cfg,
        debug,
        release,
        unlocked,
        Some(&lock),
    )
}

/// `wc` lists source files in the code tree (cwd).
pub(super) fn run_wc(
    threshold: usize,
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
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
pub(super) fn run_program(
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
                Some(&d.project),
                d.config.bin.as_ref(),
                &d.project_root,
                &d.build_root,
                d.config.disable_toolchain,
                keep,
            )
        }
        None => (
            None,
            None,
            &fallback_roots.0,
            &fallback_roots.1,
            false,
            crate::worktree_record::DEFAULT_KEEP,
        ),
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
pub(super) fn run_sync(
    script: &Option<String>,
    all: &bool,
    filter: &Option<String>,
    include_ignored: &bool,
    bench: &Option<std::num::NonZeroUsize>,
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
            included_config_files: detection.config_sources.included(),
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
            included_config_files: detection.config_sources.included(),
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
        Some(&detection.project),
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
pub(super) fn refuse_unmeasured_force(mode: &cli::ModeArgs) -> Result<(), DevError> {
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
pub(super) fn run_check(
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
    audit: bool,
    args: &[String],
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let no_config_roots = project::detect_roots()?;
    let user_config = if detection.is_none() {
        Some(config::load_user_only()?)
    } else {
        None
    };
    let project = detection.map(|d| d.project.clone());
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
        let user = user_config
            .as_ref()
            .expect("the unconfigured branch loaded user config");
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
        None => (no_config_roots.1.as_path(), no_config_roots.0.as_path()),
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
    let _ceiling = check_cmd::CheckWatchdog::arm(check_cmd::CHECK_CEILING)?;
    // The `[lints]` config, loaded once; a tree without one allows nothing.
    let no_lints = config::LintsConfig::default();
    let lints = clippy_cfg.unwrap_or(&no_lints);
    if audit {
        return check_cmd::cmd_check_audit(
            project_root,
            gremlins_cfg,
            header_cfg,
            textlint_rules,
            manifest_cfg,
        );
    }
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
pub(super) fn run_clippy(
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
        None => (&[][..], None, no_config_roots.1.as_path()),
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
pub(super) fn run_deps(
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
