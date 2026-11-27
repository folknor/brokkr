//! Startup, parsing and command-context helpers.

use super::dispatch::run;
use crate::cli::{Cli, Command};
use crate::command_args::parse_env_overrides;
use crate::error::DevError;
use crate::fmt_cmd::cmd_fmt;
use crate::passthrough_cmd::duration_ms;
use crate::project::Project;
use crate::{
    check_cmd, cli, config, context, deps, history_cmd, man, output, project, runnables, shutdown, test_runner, wc,
};
use std::time::Instant;

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
///
/// Also captures the invocation a results row records, here because only the
/// parse knows which trailing tokens are positional values rather than
/// brokkr's options (`run_invocation::positional_tail`).
fn parse_cli(project: Option<&Project>) -> (Cli, cli::CommandId, crate::db::RunInvocation) {
    let raw_argv: Vec<String> = std::env::args().collect();

    let mut cmd = <Cli as clap::CommandFactory>::command();
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();

    for name in names {
        let Some(command_id) = cli::CommandId::from_name(&name) else {
            clap::Error::raw(
                clap::error::ErrorKind::InvalidSubcommand,
                "every clap command has an exhaustive command specification",
            )
            .exit();
        };
        let spec = command_id.spec();
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

    let matches = match cmd.try_get_matches_from_mut(raw_argv.clone()) {
        Ok(matches) => matches,
        Err(e) => e.exit(),
    };
    let positional_tail = match crate::run_invocation::positional_tail(&cmd, &matches, &raw_argv) {
        Ok(tail) => tail,
        Err(message) => clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, message).exit(),
    };
    let invocation = crate::run_invocation::capture(raw_argv, positional_tail);
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
        && let Command::Visual(cli::VisualCmd { litehtml, .. }) = &cli.command
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
    (cli, command_id, invocation)
}

/// Drop the variables brokkr hands the children it attaches a marker FIFO or
/// a harness artefact directory to, when this brokkr inherited them: a FIFO
/// is a contract between one brokkr and the children it attached it to, so
/// one handed to this brokkr reaches no child of it, on any spawn path. A
/// child that should have them gets them set explicitly. Not done for the
/// harness shim, which forwards its environment to the program cargo meant
/// to run unchanged.
fn shed_inherited_run_env() {
    for name in [
        crate::sidecar::MARKER_FIFO_ENV,
        "BROKKR_HARNESS_ARTEFACT_DIR",
    ] {
        // SAFETY: called from `main` before brokkr starts any thread, so no
        // other thread can read or write the environment concurrently.
        unsafe { std::env::remove_var(name) };
    }
}

pub(crate) fn main() {
    if let Some(code) = test_runner::maybe_run_harness_shim() {
        std::process::exit(code);
    }
    // A `brokkr serve` supervisor is no command: it skips detection, parsing,
    // history and every lock (`crate::serve::supervisor`).
    if let Some(code) = crate::serve::maybe_run_supervisor() {
        std::process::exit(code);
    }
    shed_inherited_run_env();
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    // The version banner needs no project, no config beyond the user-wide
    // results server, and no history row: answered before any of them.
    if matches!(raw_args.as_slice(), [flag] if flag == "--version" || flag == "-V") {
        crate::version_cmd::print_version();
        std::process::exit(0);
    }
    let start = Instant::now();

    // Resolve the project once for help filtering, the toolchain setting, command
    // dispatch and history. Help still fails open on a bad config;
    // the command decides below whether that detection error is fatal.
    let (detection, detection_error) = match project::detect_optional() {
        Ok(detection) => (detection, None),
        Err(error) => (None, Some(error)),
    };
    if let Ok(cwd) = std::env::current_dir() {
        let root = detection
            .as_ref()
            .map_or(cwd.as_path(), |d| d.project_root.as_path());
        output::set_path_anchor(&cwd, root);
    }
    let (cli, command_id, invocation) = parse_cli(detection.as_ref().map(|d| &d.project));
    cli::set_invocation(command_id);
    // `build`'s stdout is its value - the snapshot path - and `run`'s is the
    // program's, so everything else the run says, from the lock wait on, goes
    // to stderr.
    if matches!(command_id, cli::CommandId::Build | cli::CommandId::Run)
        || matches!(
            cli.command,
            Command::Install(cli::InstallCmd { names: true, .. })
        )
    {
        output::reserve_stdout();
    }

    // Lead our own process group before spawning anything or acquiring the
    // lock, so brokkr's internal `kill(-pgid, ...)` sweeps can't escape upward
    // into a launcher that spawned us without its own session. No-op for the
    // common interactive-foreground case; see `shutdown::isolate_process_group`.
    shutdown::isolate_process_group();

    // Don't record `history` itself (avoids recursive noise).
    let is_history = matches!(cli.command, Command::History(_));

    // Handlers return errors rather than exiting: all command-owned guards
    // (lock, disabled toolchain, scratch and streams) drop before we record
    // history and choose the process exit, including propagated child codes.
    // A `[cargo_env]` failure some path tolerated still fails the command.
    let result = crate::cargo_env::seal(match detection_error {
        Some(error) if !detection_error_is_ignored(command_id) => Err(error),
        _ => run(cli, detection.as_ref(), &invocation),
    });
    let elapsed_ms = duration_ms(start.elapsed());

    let exit_code = crate::error::result_exit_code(&result);

    if !is_history {
        history_cmd::record_history(&raw_args, elapsed_ms, exit_code, detection.as_ref());
    }

    match result {
        Ok(()) => {}
        Err(DevError::ExitCode(code)) => std::process::exit(code),
        // No project-wide sweep follows an interrupt. It would run after every
        // guard, the lock included, has dropped, and the routine set holds what
        // runs keep on purpose: hung-test snapshots, failure artefacts, caches.
        // A measured attempt discards its own scratch output while it unwinds,
        // under its hold (`crate::measure`); other disposable working state
        // an interrupted run leaves sits under a fixed name its next run
        // clears, or goes with a `brokkr clean`.
        Err(DevError::Interrupted) => std::process::exit(130),
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
                output::path(&cwd)
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
    choice: Option<crate::build::RustProfile>,
    unlocked: bool,
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let (bin_cfg, build_root) = match detection {
        Some(d) => (d.config.bin.as_ref(), d.build_root.as_path().to_path_buf()),
        None => (None, std::env::current_dir()?),
    };
    let steps = detection.map(|d| crate::install_step::Steps {
        steps: &d.config.install_steps,
        project: d.project.name(),
        project_root: d.project_root.as_path(),
    });
    let lock = context::reenter_command_lock()?;
    // The plan was read before the lock wait, and the build stamp samples
    // the checkout after it: a checkout moved in between would pair a new
    // commit's stamp with the old commit's plan. Recompose and compare.
    if let Some(d) = detection {
        let (now, _) = crate::config::compose(&d.project_root.as_path().join("brokkr.toml"))?;
        if &now != d.config_sources.composed() {
            return Err(DevError::Refused(
                "brokkr.toml or a file it includes changed while install waited for the \
                 lock - run install again"
                    .into(),
            ));
        }
    }
    runnables::cmd_install(&build_root, bin_cfg, steps, choice, unlocked, Some(&lock))
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
    choice: Option<&crate::build::RustProfile>,
    feat: &runnables::FeatureArgs<'_>,
    commit: Option<&str>,
    with: Option<&WithTarget<'_>>,
    args: &[String],
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    // The `--with` target is built and snapshotted first, its worktree stage
    // (if any) finished before the program's begins, so the program's build
    // can neither replace it nor evict anything it still needs.
    let with_exe = match with {
        Some(w) => Some(build_snapshot(
            Some(w.name),
            choice.cloned(),
            &w.feat,
            w.commit,
            detection,
        )?),
        None => None,
    };
    let with_env: Vec<(&str, String)> = with_exe
        .iter()
        .map(|p| (WITH_EXE_VAR, p.display().to_string()))
        .collect();
    let env: Vec<(&str, &str)> = with_env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    // Build in `build_root`, run in `run_dir`. Without `--with`, cargo
    // launches the program through brokkr-launch, so the commit's own cargo
    // config shapes the launch under `--commit` too. With `--with` it is
    // built and executed directly, so the path is in the program's
    // environment only, never in cargo's or a build script's.
    let launch = |build_root: &std::path::Path,
                  run_dir: &std::path::Path,
                  bin_cfg: Option<&crate::config::BinConfig>|
     -> Result<(), DevError> {
        let lock = context::reenter_command_lock()?;
        if with.is_some() {
            return runnables::cmd_run_built(
                build_root,
                run_dir,
                bin_cfg,
                name,
                choice.cloned(),
                feat,
                args,
                &env,
                &lock,
            );
        }
        runnables::cmd_run(
            build_root,
            run_dir,
            bin_cfg,
            name,
            choice.cloned(),
            feat,
            args,
            Some(&lock),
        )
    };
    let Some(commit) = commit else {
        let (bin_cfg, project_root) = match detection {
            Some(d) => (d.config.bin.as_ref(), d.build_root.as_path().to_path_buf()),
            None => (None, std::env::current_dir()?),
        };
        return launch(&project_root, &project_root, bin_cfg);
    };
    // `--commit`: `crit`'s shape. The worktree is cut from the build
    // root, its bookkeeping lives under the project root, and the lock is
    // taken inside the closure so the toolchain-disable lands on the
    // worktree rather than the live tree.
    let fallback_roots = project::detect_roots()?;
    let (project, bin_cfg, project_root, build_root, tree, keep) = match detection {
        Some(d) => {
            let keep = context::worktree_keep(&d.config)?;
            (
                Some(&d.project),
                d.config.bin.as_ref(),
                &d.project_root,
                &d.build_root,
                crate::tree_policy::TreePolicy::of(&d.config),
                keep,
            )
        }
        None => (
            None,
            None,
            &fallback_roots.0,
            &fallback_roots.1,
            crate::tree_policy::TreePolicy::default(),
            crate::worktree_record::DEFAULT_KEEP,
        ),
    };
    context::with_worktree(
        project,
        project_root,
        build_root,
        Some(commit),
        false,
        &tree,
        keep,
        |wt| launch(wt.as_path(), build_root.as_path(), bin_cfg),
    )
}

const WITH_EXE_VAR: &str = runnables::WITH_EXE_ENV;

/// `brokkr serve TARGET`: what dispatch parsed.
pub(super) struct ServeRequest<'a> {
    pub(super) target: &'a str,
    pub(super) choice: Option<crate::build::RustProfile>,
    pub(super) feat: &'a runnables::FeatureArgs<'a>,
    pub(super) commit: Option<&'a str>,
    pub(super) with: Option<&'a WithTarget<'a>>,
    pub(super) name: Option<&'a str>,
    pub(super) ready: Option<&'a str>,
    pub(super) ready_timeout: std::time::Duration,
    pub(super) grace: std::time::Duration,
    pub(super) args: &'a [String],
    pub(super) detection: Option<&'a project::Detection>,
}

/// `brokkr serve TARGET`: build the program and any `--with` target as
/// `brokkr build` does - snapshots, each worktree stage finished before the
/// next begins, the `--with` target first as for `run` - then hand them to
/// `crate::serve::start`, all under the dispatch hold.
pub(super) fn run_serve(req: &ServeRequest<'_>) -> Result<(), DevError> {
    crate::serve::refuse_nested_start()?;
    let with = match req.with {
        Some(w) => Some(build_snapshot(
            Some(w.name),
            req.choice.clone(),
            &w.feat,
            w.commit,
            req.detection,
        )?),
        None => None,
    };
    let program = build_snapshot(
        Some(req.target),
        req.choice.clone(),
        req.feat,
        req.commit,
        req.detection,
    )?;
    let fallback = match req.detection {
        Some(_) => None,
        None => Some(project::detect_roots()?),
    };
    let (project, root, cwd) = match (req.detection, fallback.as_ref()) {
        (Some(d), _) => (
            d.project.name().to_owned(),
            d.project_root.as_path(),
            d.build_root.as_path(),
        ),
        (None, Some((root, build))) => ("brokkr".to_owned(), root.as_path(), build.as_path()),
        (None, None) => {
            return Err(DevError::Internal(
                "serve roots were not resolved for an unconfigured tree".into(),
            ));
        }
    };
    crate::serve::start(&crate::serve::StartRequest {
        project: &project,
        configured: req.detection.is_some(),
        root,
        cwd,
        name: req.name,
        program,
        with,
        args: req.args,
        grace: req.grace,
        ready: req.ready,
        ready_timeout: req.ready_timeout,
    })
}

/// `run --with TARGET`: the second target, built as `brokkr build` builds it,
/// with its own commit and features and the program's profile.
pub(super) struct WithTarget<'a> {
    pub(super) name: &'a str,
    pub(super) commit: Option<&'a str>,
    pub(super) feat: runnables::FeatureArgs<'a>,
}

/// `build` builds the code tree (cwd), or a `--commit` worktree of it, and
/// prints the snapshot's path as its one stdout line (startup reserved stdout
/// for it). The snapshot store is the project's own `.brokkr/artifacts`
/// whichever tree was built, so `brokkr clean` finds every snapshot.
pub(super) fn run_build(
    name: Option<&str>,
    choice: Option<crate::build::RustProfile>,
    feat: &runnables::FeatureArgs<'_>,
    commit: Option<&str>,
    detection: Option<&project::Detection>,
) -> Result<(), DevError> {
    let path = build_snapshot(name, choice, feat, commit, detection)?;
    output::data_stdout(&path.display().to_string());
    Ok(())
}

/// `build`'s work without the stdout line: build the live tree or a
/// `--commit` worktree and return the snapshot's path. `run --with` builds
/// its second target through this too.
fn build_snapshot(
    name: Option<&str>,
    choice: Option<crate::build::RustProfile>,
    feat: &runnables::FeatureArgs<'_>,
    commit: Option<&str>,
    detection: Option<&project::Detection>,
) -> Result<std::path::PathBuf, DevError> {
    let fallback_roots = project::detect_roots()?;
    let (project, bin_cfg, project_root, build_root, tree, keep) = match detection {
        Some(d) => (
            Some(&d.project),
            d.config.bin.as_ref(),
            &d.project_root,
            &d.build_root,
            crate::tree_policy::TreePolicy::of(&d.config),
            context::worktree_keep(&d.config)?,
        ),
        None => (
            None,
            None,
            &fallback_roots.0,
            &fallback_roots.1,
            crate::tree_policy::TreePolicy::default(),
            crate::worktree_record::DEFAULT_KEEP,
        ),
    };
    let store = crate::clean::layout::ProjectState::new(project_root.as_path()).artifacts_dir();
    match commit {
        None => {
            let _lock = context::reenter_command_lock()?;
            runnables::cmd_build(build_root.as_path(), &store, bin_cfg, name, choice, feat)
        }
        Some(commit) => context::with_worktree(
            project,
            project_root,
            build_root,
            Some(commit),
            false,
            &tree,
            keep,
            |wt| {
                let _lock = context::reenter_command_lock()?;
                runnables::cmd_build(wt.as_path(), &store, bin_cfg, name, choice, feat)
            },
        ),
    }
}

/// `check` builds, scans, and diffs the code tree, so it anchors on the build
/// root (cwd). The config values it uses (`[[check]]` sweeps, dependency rules,
/// gremlin excludes) come from wherever brokkr.toml was found, in cwd or one
/// level up; the no-config path still composes the user-wide rules on their
/// own.
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
        lint_sweeps,
        dependency_rules,
        quarantine,
        test_cfg,
        gated,
        gremlins_cfg,
        header_cfg,
        textlint_rules,
        script_checks,
        manifest_cfg,
        benches,
        rustdoc_cfg,
        clippy_cfg,
    ) = if let Some(d) = detection {
        (
            d.config.check.as_slice(),
            d.config.lint_sweeps.as_slice(),
            d.config.dependency_rules.as_slice(),
            d.config.quarantine.as_slice(),
            d.config.test.as_ref(),
            check_cmd::GatedPhases {
                member_check: d.config.member_check.as_ref(),
                fixture_lints: d.config.fixture_lints.as_slice(),
            },
            d.config.gremlins.as_ref(),
            d.config.header.as_ref(),
            d.config.textlint.as_slice(),
            d.config.script_checks.as_slice(),
            d.config.manifest.as_ref(),
            check_cmd::Benches {
                benches: &d.config.benches,
                registry: d.config.bench_registry.as_ref(),
                tests: d.config.bench_tests.as_deref(),
                composed: Some(d.config_sources.composed()),
                project_root: d.project_root.as_path(),
                library: d
                    .config
                    .bench_registry
                    .as_ref()
                    .and_then(|registry| registry.library.as_deref()),
            },
            d.config.rustdoc.as_ref(),
            d.config.lints.as_ref(),
        )
    } else {
        // No project config, but the user-wide layer applies to any tree
        // brokkr checks. A repo without brokkr.toml is exactly where a
        // personal convention may be the only rule in force.
        let Some(user) = user_config.as_ref() else {
            return Err(DevError::Internal(
                "the unconfigured branch loaded user config".to_owned(),
            ));
        };
        (
            &[][..],
            &[][..],
            &[][..],
            &[][..],
            None,
            check_cmd::GatedPhases::default(),
            None,
            None,
            user.textlint.as_slice(),
            user.script_checks.as_slice(),
            None,
            check_cmd::Benches {
                benches: &user.benches,
                registry: user.bench_registry.as_ref(),
                tests: user.bench_tests.as_deref(),
                composed: None,
                project_root: no_config_roots.0.as_path(),
                library: None,
            },
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
            &check_cmd::PlanInputs {
                project_root,
                check_entries,
                lint_sweeps,
                quarantine,
                test_cfg,
                gated,
                script_checks,
                rustdoc_configured: rustdoc_cfg.is_some(),
                features,
                no_default_features: *no_default_features,
                packages: package,
                profile_name: profile.as_deref(),
                gate: *gate,
                force_rust: *force_rust,
                extra_args: args,
            },
            &check_cmd::AuditInputs {
                gremlins_cfg,
                header_cfg,
                textlint_rules,
                manifest_cfg,
                benches,
                dependency_rules,
                lints,
            },
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
        lint_sweeps,
        dependency_rules,
        quarantine,
        test_cfg,
        gated,
        gremlins_cfg,
        header_cfg,
        textlint_rules,
        script_checks,
        manifest_cfg,
        benches,
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
    let (check_entries, lint_sweeps, clippy_cfg, project_root) = match detection {
        Some(d) => (
            d.config.check.as_slice(),
            d.config.lint_sweeps.as_slice(),
            d.config.lints.as_ref(),
            d.build_root.as_path(),
        ),
        None => (&[][..], &[][..], None, no_config_roots.1.as_path()),
    };
    let env_overrides = parse_env_overrides(env)?;
    let _lock = context::reenter_command_lock()?;
    let no_lints = config::LintsConfig::default();
    check_cmd::cmd_clippy(
        project_root,
        check_entries,
        lint_sweeps,
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
