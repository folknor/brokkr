//! `brokkr run` / `brokkr install`: workspace runnable discovery + dispatch.
//!
//! Both commands discover their targets from `cargo metadata --no-deps`
//! rather than requiring config: every workspace member's bin targets (and,
//! for `run`, example targets) are runnable by target name. The `[bin]`
//! section curates what discovery leaves ambiguous - a `default` for bare
//! `brokkr run`, an `install` package list for multi-bin workspaces, and a
//! `profile`/`debug` default shared by both commands (release when unset,
//! matching `brokkr test`; `--debug`/`--release`/`--profile` override).

use std::path::{Path, PathBuf};

use crate::cargo::metadata::{self, Query};
use crate::config::BinConfig;
use crate::error::DevError;
use crate::output;

/// The environment variable `run --with` and `serve --with` hand the program
/// the `--with` target's executable path in.
pub(crate) const WITH_EXE_ENV: &str = "BROKKR_WITH_EXE";

/// `run`'s cargo feature selection, forwarded verbatim.
///
/// Brokkr stays out of feature resolution: the value of `--features` goes to
/// cargo unparsed, so cargo's grammar and cargo's error message both apply.
pub struct FeatureArgs<'a> {
    /// Each `--features` occurrence, in order.
    pub features: &'a [String],
    /// `--all-features` (clap-exclusive with `features`).
    pub all: bool,
    /// `--no-default-features`.
    pub no_default: bool,
}

/// One discovered runnable target.
struct Runnable {
    /// Target name (`cargo run`'s `--bin`/`--example` namespace).
    name: String,
    /// Owning package name (the `-p` selector).
    package: String,
    /// Directory containing the owning package's `Cargo.toml` (the
    /// `cargo install --path` target).
    package_dir: PathBuf,
    /// True for an example target, false for a bin.
    example: bool,
}

/// The profile `run --commit` builds: the CLI choice > `[bin] profile` >
/// `[bin] debug` > release.
fn resolve_profile(
    cfg: Option<&BinConfig>,
    choice: Option<crate::build::RustProfile>,
) -> crate::build::RustProfile {
    let configured = cfg.and_then(BinConfig::configured_profile);
    choice.or(configured).unwrap_or_default()
}

/// The profile `run` and `install` resolve through the build-purpose
/// policy: the CLI choice and the `[bin]` default.
fn bin_profile(
    purpose: crate::build_purpose::Purpose,
    cfg: Option<&BinConfig>,
    choice: Option<crate::build::RustProfile>,
) -> Result<crate::build_purpose::Profile, DevError> {
    crate::build_purpose::profile(
        purpose,
        crate::build_purpose::Profiles {
            invocation: choice,
            configured: cfg.and_then(BinConfig::configured_profile),
        },
    )
}

/// Discover every bin and example target in the workspace via
/// `cargo metadata --no-deps`.
fn discover(project_root: &Path) -> Result<Vec<Runnable>, DevError> {
    discover_workspace(project_root).map(|(runnables, _)| runnables)
}

/// [`discover`], plus metadata's `workspace_root` - the directory cargo reads
/// `Cargo.lock` from for every member, wherever the invocation started.
fn discover_workspace(project_root: &Path) -> Result<(Vec<Runnable>, PathBuf), DevError> {
    let metadata = metadata::load(project_root, Query::workspace(), &[], None)?;
    runnables_of(&metadata)
}

/// Every bin and example target, and the workspace root, of one snapshot.
fn runnables_of(
    metadata: &crate::cargo::metadata::CargoMetadata,
) -> Result<(Vec<Runnable>, PathBuf), DevError> {
    let workspace_root = PathBuf::from(&metadata.workspace_root);

    let mut out = Vec::new();
    for pkg in &metadata.packages {
        let package_dir = Path::new(&pkg.manifest_path)
            .parent()
            .ok_or_else(|| {
                DevError::Build(format!(
                    "manifest path has no parent: {}",
                    pkg.manifest_path
                ))
            })?
            .to_owned();
        for target in &pkg.targets {
            let example = target.kind.iter().any(|kind| kind == "example");
            if !example && !target.kind.iter().any(|kind| kind == "bin") {
                continue;
            }
            out.push(Runnable {
                name: target.name.clone(),
                package: pkg.name.clone(),
                package_dir: package_dir.clone(),
                example,
            });
        }
    }
    Ok((out, workspace_root))
}

/// One index line per runnable: `name (bin, pkg)` / `name (example, pkg)`.
fn index_line(r: &Runnable) -> String {
    let kind = if r.example { "example" } else { "bin" };
    if r.name == r.package {
        format!("  {} ({kind})", r.name)
    } else {
        format!("  {} ({kind}, {})", r.name, r.package)
    }
}

/// Resolve `run`'s target: `name` > `[bin] default` > the sole runnable.
/// `None` means the bare index was printed and there is nothing to run.
fn resolve_target<'a>(
    runnables: &'a [Runnable],
    cfg: Option<&BinConfig>,
    name: Option<&str>,
) -> Result<Option<&'a Runnable>, DevError> {
    if runnables.is_empty() {
        return Err(DevError::Refused(
            "no bin or example targets in this workspace - nothing to run".into(),
        ));
    }

    let default = cfg.and_then(|c| c.default.as_deref());
    let wanted = name.or(default);
    let target = match wanted {
        Some(n) => {
            let matches: Vec<&Runnable> = runnables.iter().filter(|r| r.name == n).collect();
            match matches.as_slice() {
                [] => {
                    let origin = if name.is_none() {
                        " ([bin] default)"
                    } else {
                        ""
                    };
                    return Err(DevError::Refused(format!(
                        "no bin or example target named '{n}'{origin}; bare \
                         `brokkr run` lists what exists"
                    )));
                }
                [one] => *one,
                many => {
                    let mut msg = format!("'{n}' names {} targets:\n", many.len());
                    for r in many {
                        msg.push_str(&index_line(r));
                        msg.push('\n');
                    }
                    msg.push_str("disambiguate the target names in Cargo.toml");
                    return Err(DevError::Refused(msg));
                }
            }
        }
        None if runnables.len() == 1 => &runnables[0],
        None => {
            // Bare-is-an-index: several candidates and no default is a
            // listing, not an error (the sync/service shape).
            let mut msg = format!("{} runnable targets:\n", runnables.len());
            for r in runnables {
                msg.push_str(&index_line(r));
                msg.push('\n');
            }
            msg.push_str("run one with `brokkr run <name>`, or set [bin] default");
            output::run_msg(msg.trim_end());
            return Ok(None);
        }
    };
    Ok(Some(target))
}

/// `brokkr run [NAME]`: resolve the runnable, build it with cargo's output
/// captured, then exec `cargo run --quiet [--release] -p <pkg> --bin|--example
/// <name> [-- <args>]`.
#[allow(clippy::too_many_arguments)]
pub fn cmd_run(
    project_root: &Path,
    cfg: Option<&BinConfig>,
    name: Option<&str>,
    choice: Option<crate::build::RustProfile>,
    feat: &FeatureArgs<'_>,
    args: &[String],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<(), DevError> {
    let runnables = discover(project_root)?;
    let Some(target) = resolve_target(&runnables, cfg, name)? else {
        return Ok(());
    };

    let purpose = crate::build_purpose::Purpose::Run;
    let shape = crate::build_purpose::resolve(crate::build_purpose::Request {
        purpose,
        profile: bin_profile(purpose, cfg, choice)?,
        packages: vec!["-p".into(), target.package.clone()],
        features: crate::build_purpose::Features::values(
            feat.features,
            feat.all,
            feat.no_default,
            crate::build_purpose::FeatureRendering::Occurrences,
        ),
        unification: Vec::new(),
        targets: Vec::new(),
        env: Vec::new(),
        build_root: Some(project_root),
    });
    // Always name the target explicitly. `-p` alone leaves a multi-bin
    // package ambiguous, and cargo bails rather than guessing.
    let mut selection = shape.args;
    selection.push(if target.example { "--example" } else { "--bin" }.into());
    selection.push(target.name.clone());

    // Build first with the output captured, then let a quiet `cargo run` of
    // the same selection launch it. The run finds the units fresh, so it
    // compiles nothing as a rule, and cargo still launches the program
    // itself: its runner, loader paths and `[env]` apply, which a direct
    // exec of the artifact would lose.
    // `--color=never` keeps the captured stderr parseable whatever
    // `CARGO_TERM_COLOR` says; colour is presentation, not a build input.
    let mut build_args: Vec<String> = vec!["build".into(), "--color=never".into()];
    build_args.extend(selection.iter().cloned());
    build_args.push("--message-format=json".into());
    // Held across the handoff: the prebuild's own guard drops when it
    // returns, and a stop landing between the two cargos must still be seen.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    crate::build::cargo_captured(
        &build_args,
        &shape.env,
        project_root,
        "cargo build failed",
        crate::build::CompilerWarnings::LeaveToRun,
    )?;
    if crate::shutdown::is_shutdown_requested() {
        return Err(DevError::Interrupted);
    }

    let mut cargo_args: Vec<String> = vec!["run".into(), "--quiet".into()];
    cargo_args.extend(selection);
    if !args.is_empty() {
        cargo_args.push("--".into());
        cargo_args.extend(args.iter().cloned());
    }
    // The program's own exit code is the command's: cargo has nothing of its
    // own to add once the program ran, so echoing it as a brokkr error and
    // exiting 1 only lost the code.
    let ended = run_cargo_with_env(&cargo_args, &shape.env, lock);
    report_runtime(&target.name, &ended);
    match ended?.0 {
        0 => Ok(()),
        code => Err(DevError::ExitCode(code)),
    }
}

/// `[run] NAME finished after X (exit CODE)` on stderr once a launched run
/// ends with a code: the supervisor's time from the launch to the end, the
/// build and the launch's preparation not counted. "finished", not "exited":
/// on the plain path a non-zero code can be cargo's own, from a rebuild it
/// failed. An interrupt or a signal death carries no timing out of the
/// supervisor, so it says only how the run ended; a launch that never
/// happened says nothing, its error being the report.
fn report_runtime(name: &str, ended: &Result<(i32, std::time::Duration), DevError>) {
    match ended {
        Ok((code, elapsed)) => output::run_msg(&format!(
            "{name} finished after {} (exit {code})",
            output::format_duration(*elapsed)
        )),
        Err(DevError::Interrupted) => output::run_msg(&format!("{name} interrupted")),
        Err(_) => {}
    }
}

/// `brokkr run --commit REF [NAME]`: build the target in the commit's
/// worktree, then run it from `run_dir` - the directory `brokkr run` was
/// invoked from.
///
/// Building and running are split here, unlike [`cmd_run`]'s single `cargo
/// run`, because the two must happen in different trees. The build belongs in
/// the worktree, so the commit's own manifests, lockfile and `.cargo` config
/// decide what gets compiled, into the worktree's own `target/`
/// ([`crate::build::cargo_build`] isolates it). The program belongs in the live
/// tree, so relative paths in `args` and any cache or data the program reads
/// relative to its working directory resolve exactly as they do for a plain
/// `brokkr run`. An A/B pair - `run --commit A NAME -- ARGS` against `run NAME
/// -- ARGS` - then varies the code and nothing else, with no hand-made target
/// dirs or source copies.
///
/// Targets are discovered in the worktree: the commit may predate a target, or
/// carry one the live tree has since dropped.
///
/// `run --with` takes this shape for the live tree too (`worktree` is then
/// the build root), so `env` - the `--with` path - reaches the program alone
/// and never cargo or a build script, whichever tree the program came from.
#[allow(clippy::too_many_arguments)]
pub fn cmd_run_built(
    worktree: &Path,
    run_dir: &Path,
    cfg: Option<&BinConfig>,
    name: Option<&str>,
    choice: Option<crate::build::RustProfile>,
    feat: &FeatureArgs<'_>,
    args: &[String],
    env: &[(&str, &str)],
    lock: &crate::lockfile::LockGuard,
) -> Result<(), DevError> {
    let runnables = discover(worktree)?;
    let Some(target) = resolve_target(&runnables, cfg, name)? else {
        return Ok(());
    };
    let build = crate::build::BuildConfig {
        package: Some(target.package.clone()),
        bin: (!target.example).then(|| target.name.clone()),
        example: target.example.then(|| target.name.clone()),
        features: feat.features.to_vec(),
        default_features: !feat.no_default,
        all_features: feat.all,
        profile: resolve_profile(cfg, choice),
    };
    // Held across the build-to-exec handoff, as in `cmd_run`.
    let _interrupts = crate::shutdown::SigtermGuard::install();
    let exe = crate::build::cargo_build(&build, worktree)?;
    if crate::shutdown::is_shutdown_requested() {
        return Err(DevError::Interrupted);
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let ended = output::run_passthrough_in(
        &exe.path,
        &arg_refs,
        Some(run_dir),
        env,
        output::SpawnProfile::Plain,
        Some(lock),
    )
    .map(|out| {
        (
            out.failed_code(crate::output::child::Exits::SUCCESS)
                .unwrap_or(0),
            out.elapsed,
        )
    });
    report_runtime(&target.name, &ended);
    // Same contract as `cmd_run`: the program's code, silently.
    match ended?.0 {
        0 => Ok(()),
        code => Err(DevError::ExitCode(code)),
    }
}

/// `brokkr build [NAME]`: build the target in `build_root` - the live tree,
/// or a `--commit` worktree - and copy the executable into `store`, returning
/// the copy's path.
///
/// NAME resolves as for `run`, except that a bare form with several
/// candidates is a refusal after the index: a command whose output is a path
/// has nothing to print when it chose nothing, and exiting 0 would hand a
/// script an empty value.
pub fn cmd_build(
    build_root: &Path,
    store: &Path,
    cfg: Option<&BinConfig>,
    name: Option<&str>,
    choice: Option<crate::build::RustProfile>,
    feat: &FeatureArgs<'_>,
) -> Result<PathBuf, DevError> {
    let runnables = discover(build_root)?;
    let Some(target) = resolve_target(&runnables, cfg, name)? else {
        return Err(DevError::Refused(
            "no target chosen: name one (`brokkr build <name>`) or set [bin] default".into(),
        ));
    };
    let build = crate::build::BuildConfig {
        package: Some(target.package.clone()),
        bin: (!target.example).then(|| target.name.clone()),
        example: target.example.then(|| target.name.clone()),
        features: feat.features.to_vec(),
        default_features: !feat.no_default,
        all_features: feat.all,
        profile: resolve_profile(cfg, choice),
    };
    let exe = crate::build::cargo_build(&build, build_root)?;
    snapshot_executable(&exe.path, store).map(|(path, _)| path)
}

/// A script bench's build target, resolved as `brokkr build` resolves NAME
/// (`name` > `[bin] default` > the sole runnable) in `build_root`: its
/// package, target name and whether it is an example.
///
/// Read fresh rather than through the invocation's metadata cache: a bench
/// resolves once per arm, and the cache would hand every later arm the first
/// arm's answer - a failed query (a spawn or resource failure the next arm
/// might not meet) failing them all, and a live-tree edit between arms (a
/// second package gaining the same target name, which must refuse) unseen,
/// even by an arm whose own tree reads clean.
pub(crate) fn resolve_build_target(
    build_root: &Path,
    cfg: Option<&BinConfig>,
    name: Option<&str>,
) -> Result<(String, String, bool), DevError> {
    let metadata = metadata::load_fresh(build_root, &Query::workspace(), &[], None)?;
    let (runnables, _) = runnables_of(&metadata)?;
    let Some(target) = resolve_target(&runnables, cfg, name)? else {
        return Err(DevError::Refused(
            "no target chosen: name one in the bench's `target` or set [bin] default".into(),
        ));
    };
    Ok((target.package.clone(), target.name.clone(), target.example))
}

/// The profile `brokkr build` and a script bench resolve: the given choice
/// > `[bin] profile` > `[bin] debug` > release.
pub(crate) fn build_profile(
    cfg: Option<&BinConfig>,
    choice: Option<crate::build::RustProfile>,
) -> crate::build::RustProfile {
    resolve_profile(cfg, choice)
}

/// Copy `exe` to `store/<xxh128>/<file name>` and return that path and the
/// digest naming its directory.
///
/// The cargo-uplifted path is rewritten by the next build of anything sharing
/// it, so a script holding it runs whatever was built last. The snapshot is
/// keyed by its own content and never written again once in place: the copy
/// is hashed (not the source, which a build could replace in between), then
/// renamed into its digest directory, and an identical earlier snapshot is
/// reused as it is. The file keeps its name, so it is the same executable
/// under another directory - which is all it is: a program that looks beside
/// itself for siblings or resources sees the snapshot directory.
pub(crate) fn snapshot_executable(exe: &Path, store: &Path) -> Result<(PathBuf, String), DevError> {
    let file_name = exe.file_name().ok_or_else(|| {
        DevError::Build(format!(
            "built executable has no file name: {}",
            output::path(exe)
        ))
    })?;
    std::fs::create_dir_all(store)?;
    let staging = Staging(store.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        file_name.to_string_lossy()
    )));
    std::fs::copy(exe, &staging.0)?;
    let digest = crate::preflight::compute_file_digest(&staging.0)?.to_string();
    let dir = store.join(&digest);
    let dest = dir.join(file_name);
    if !dest.is_file() {
        std::fs::create_dir_all(&dir)?;
        std::fs::rename(&staging.0, &dest)?;
    }
    Ok((dest, digest))
}

/// Whether `name` is a staging copy [`snapshot_executable`] makes,
/// `.staging-PID-NAME`: what a killed snapshot leaves in the artifact store,
/// which store maintenance removes (`crate::maintenance`).
pub(crate) fn is_staging_copy_name(name: &str) -> bool {
    name.strip_prefix(".staging-")
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(pid, file)| {
            !file.is_empty() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
        })
}

/// The staging copy, removed however the snapshot ends - published (renamed
/// away, so nothing is left to remove), reused, or failed partway. A failed
/// copy is the case that matters: an out-of-space write leaves a partial
/// executable on the disk that just ran out, under a name no later run reuses.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => output::warn(&format!(
                "could not remove snapshot staging copy {}: {e}",
                output::path(&self.0)
            )),
        }
    }
}

struct InstallPackage {
    name: String,
    package_dir: PathBuf,
    /// The bins `cargo install` installs for it, or why brokkr cannot name
    /// them ([`crate::build_info::installable_bins`]).
    bins: Result<Vec<String>, DevError>,
}

/// Resolve configured install package names against one metadata snapshot:
/// package names, never bin names or directory basenames, and an unknown
/// package refused.
fn resolve_install_packages(
    metadata: &crate::cargo::metadata::CargoMetadata,
    runnables: &[Runnable],
    install: &[String],
) -> Result<Vec<InstallPackage>, DevError> {
    let mut out = Vec::new();
    for pkg in install {
        let selected: Vec<&Runnable> = runnables
            .iter()
            .filter(|r| !r.example && &r.package == pkg)
            .collect();
        let Some(first) = selected.first() else {
            return Err(DevError::Refused(format!(
                "[bin] install names '{pkg}', which has no bin target \
                 in this workspace"
            )));
        };
        let package = metadata
            .packages
            .iter()
            .find(|p| &p.name == pkg && Path::new(&p.manifest_path).parent() == Some(first.package_dir.as_path()))
            .ok_or_else(|| DevError::Internal(format!("package '{pkg}' left the metadata snapshot")))?;
        out.push(InstallPackage {
            name: pkg.clone(),
            package_dir: first.package_dir.clone(),
            bins: crate::build_info::installable_bins(package),
        });
    }
    Ok(out)
}

/// The packages `install` installs: `[bin] install`, or the workspace's sole
/// bin-carrying package; and metadata's `workspace_root`.
fn select_install_packages(
    build_root: &Path,
    cfg: Option<&BinConfig>,
) -> Result<(Vec<InstallPackage>, PathBuf), DevError> {
    let metadata = metadata::load(build_root, Query::workspace(), &[], None)?;
    let (runnables, workspace_root) = runnables_of(&metadata)?;
    let bins: Vec<&Runnable> = runnables.iter().filter(|r| !r.example).collect();

    let install = match cfg.map(|c| c.install.as_slice()) {
        Some(list) if !list.is_empty() => list.to_vec(),
        _ => {
            // Auto: exactly one package with bin targets.
            let mut pkgs: Vec<&Runnable> = Vec::new();
            for r in &bins {
                if !pkgs.iter().any(|p| p.package == r.package) {
                    pkgs.push(r);
                }
            }
            match pkgs.as_slice() {
                [] => {
                    return Err(DevError::Refused(
                        "no bin targets in this workspace - nothing to install".into(),
                    ));
                }
                [one] => vec![one.package.clone()],
                many => {
                    let mut msg = format!("{} packages carry bin targets:\n", many.len());
                    for r in many {
                        msg.push_str(&format!("  {}\n", r.package));
                    }
                    msg.push_str("list the ones to install under [bin] install");
                    return Err(DevError::Refused(msg));
                }
            }
        }
    };
    let selected = resolve_install_packages(&metadata, &runnables, &install)?;
    Ok((selected, workspace_root))
}

/// `brokkr install --names`: the bin names `install` installs, one per line
/// on stdout, in package order. Builds nothing and takes no lock. A package
/// whose bins brokkr cannot name refuses the whole listing: a partial one
/// would hide a binary from whoever audits the list.
pub fn cmd_install_names(build_root: &Path, cfg: Option<&BinConfig>) -> Result<(), DevError> {
    let (selected, _) = select_install_packages(build_root, cfg)?;
    let mut names = Vec::new();
    for package in selected {
        names.extend(package.bins?);
    }
    for name in names {
        output::data_stdout(&name);
    }
    Ok(())
}

/// `brokkr install`: install `[bin] install`'s packages, or the sole
/// bin-carrying package, via `cargo install [--debug] --locked --path
/// <pkg dir>`.
///
/// `--locked` is the default because `cargo install --path` otherwise
/// ignores the workspace `Cargo.lock` entirely and re-resolves every
/// dependency to the newest semver-compatible version. Every other brokkr
/// path - `check`, `test`, `run`, the member-check phase - reads the
/// lockfile, so without it `install` is the one command that ships a
/// dependency set no gate ever validated, and an upstream crate publishing
/// a breaking minor breaks the deploy of a tree nobody touched. Whatever
/// `check` was green on is what gets installed.
///
/// `unlocked` restores cargo's default. A tree with no `Cargo.lock` is
/// re-resolved either way - cargo refuses `--locked` without one - and says
/// so rather than failing.
///
/// `steps` are the project's `[[install]]` steps: checked (and interrupted
/// copies recovered) before the first package, run in order after the last
/// one succeeded. See [`crate::install_step`].
pub fn cmd_install(
    build_root: &Path,
    cfg: Option<&BinConfig>,
    steps: Option<crate::install_step::Steps<'_>>,
    choice: Option<crate::build::RustProfile>,
    unlocked: bool,
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<(), DevError> {
    let purpose = crate::build_purpose::Purpose::Install;
    // Resolved once, before the first package: a refused profile refuses
    // the whole install, and the override warning prints once.
    let profile = bin_profile(purpose, cfg, choice)?;
    let (selected, workspace_root) = select_install_packages(build_root, cfg)?;
    // Sampled here, under the lock and before anything installs: each
    // package's stamp names its own checkout, which need not be the build
    // root's (`crate::build_info`).
    let mut stamps = Vec::with_capacity(selected.len());
    for package in &selected {
        stamps.push(crate::build_info::sample(&package.package_dir)?);
    }
    let plan = crate::install_step::preflight(steps, build_root)?;

    // A lock cargo cannot find is a lock `--locked` would only make it abort
    // over. Look where cargo does: every package discovered here is a member
    // of one workspace, and `cargo install --path <member>` reads that
    // workspace's `Cargo.lock` at metadata's `workspace_root` - not the member
    // directory, and not wherever brokkr happened to be invoked from.
    let has_lock = workspace_root.join("Cargo.lock").is_file();
    for (package, stamp) in selected.into_iter().zip(stamps) {
        let locked = !unlocked && has_lock;
        if !unlocked && !locked {
            output::run_msg(&format!(
                "no Cargo.lock for {} - installing with cargo's own \
                 dependency resolution, not the one check validated",
                package.name
            ));
        }
        let mut cargo_args: Vec<String> = vec!["install".into()];
        let shape = crate::build_purpose::resolve(crate::build_purpose::Request {
            purpose,
            profile: profile.clone(),
            packages: vec!["--path".into(), package.package_dir.display().to_string()],
            features: crate::build_purpose::Features::default(),
            unification: Vec::new(),
            targets: Vec::new(),
            env: Vec::new(),
            build_root: Some(build_root),
        });
        cargo_args.extend(shape.args);
        if locked {
            cargo_args.push("--locked".into());
        }
        let env_refs: Vec<(&str, &str)> = shape
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let root = crate::build_info::install_root(
            build_root,
            &package.package_dir,
            crate::rustflags::EffectiveEnv::new(&env_refs),
        )?;
        if let crate::build_info::InstallRoot::Default(root) = &root {
            cargo_args.push("--root".into());
            cargo_args.push(root.display().to_string());
        }
        // Captured like `run`'s build: progress held back, diagnostics on
        // failure. Nothing replays compiler warnings after an install, so
        // they print here on success.
        cargo_args.splice(1..1, ["--color=never".into(), "--message-format=json".into()]);
        crate::build::cargo_captured(
            &cargo_args,
            &shape.env,
            build_root,
            &format!("cargo install failed for {}", package.name),
            crate::build::CompilerWarnings::Print,
        )?;
        stamp_installed(&package, stamp.as_ref(), &root)?;
    }
    plan.execute(lock)
}

/// Stamp the bins cargo just installed for `package`, or say why they carry
/// no stamp. A stamp brokkr should write and cannot fails the install: the
/// file is already in place, unstamped, and a fleet audit reads it as
/// unknown, so the failure has to be as loud as a failed build.
fn stamp_installed(
    package: &InstallPackage,
    stamp: Option<&crate::build_info::Stamp>,
    root: &crate::build_info::InstallRoot,
) -> Result<(), DevError> {
    let Some(stamp) = stamp else {
        output::run_msg(&format!(
            "{} is not in a git checkout with a commit - its bins carry no build stamp",
            package.name
        ));
        return Ok(());
    };
    let root = match root {
        crate::build_info::InstallRoot::Default(root) => root,
        crate::build_info::InstallRoot::Overridden(why) => {
            output::warn(&format!(
                "{why}, so cargo chose where {}'s bins went - they carry no build stamp",
                package.name
            ));
            return Ok(());
        }
    };
    let bins = match &package.bins {
        Ok(bins) => bins,
        Err(e) => {
            output::warn(&format!("{e} - {}'s bins carry no build stamp", package.name));
            return Ok(());
        }
    };
    if bins.is_empty() {
        return Ok(());
    }
    for bin in bins {
        let path = root
            .join("bin")
            .join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
        crate::build_info::stamp_file(&path, stamp).map_err(|e| {
            DevError::Build(format!(
                "{bin} is installed but unstamped, so it reads as unknown: {e}"
            ))
        })?;
    }
    output::run_msg(&format!(
        "stamped {} with {}",
        bins.join(", "),
        stamp.content.trim_end()
    ));
    Ok(())
}

/// Spawn cargo with inherited stdio and return its exit code. A signal death
/// is an `Err` (`DevError::Subprocess`, from the passthrough runner); a
/// non-zero exit is the caller's to map, since `run` propagates it and
/// `install` reports it.
///
/// Goes through [`output::run_passthrough_in`] (cargo spawn profile) rather than a bare
/// `Command::status()`. A `brokkr run` child is a real, long-running workload,
/// and a bare status call leaves brokkr with the default SIGTERM disposition:
/// `brokkr kill` signals brokkr's PID alone, so brokkr dies and the child is
/// orphaned and keeps running. The passthrough runner installs a `SigtermGuard`,
/// marks the child for the OOM killer ahead of brokkr, and publishes its PID
/// into the lockfile so `kill --hard` and `brokkr lock` can see it.
///
/// No `current_dir`: `build_root` is cwd by construction, which is where the
/// caller's `project_root` argument comes from.
pub(crate) fn run_cargo(
    args: &[String],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<i32, DevError> {
    run_cargo_with_env(args, &[], lock).map(|(code, _)| code)
}

/// The exit code, and the elapsed time from cargo's launch to its end.
fn run_cargo_with_env(
    args: &[String],
    env: &[(String, String)],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<(i32, std::time::Duration), DevError> {
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let out = output::run_passthrough_in(
        "cargo",
        &arg_refs,
        None,
        &env_refs,
        output::SpawnProfile::Cargo { phase: None },
        lock,
    )?;
    Ok((
        out.failed_code(crate::output::child::Exits::SUCCESS)
            .unwrap_or(0),
        out.elapsed,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_snapshot_is_keyed_by_content_and_survives_a_rebuild() {
        let root = crate::test_scratch::scratch("runnables", "snapshot");
        let exe = root.join("target/release/tool");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        let store = crate::clean::layout::ProjectState::new(&root).artifacts_dir();

        std::fs::write(&exe, b"first build").unwrap();
        let (first, digest) = snapshot_executable(&exe, &store).unwrap();
        assert_eq!(first.file_name().unwrap(), "tool", "the name is kept");
        assert_eq!(
            first.parent().unwrap(),
            store.join(&digest),
            "named by its digest"
        );
        // The same bytes again reuse the snapshot rather than rewrite it.
        assert_eq!(snapshot_executable(&exe, &store).unwrap().0, first);

        // A rebuild replaces the uplifted file; the snapshot a script holds
        // keeps the bytes it was handed, and the new build gets its own.
        std::fs::write(&exe, b"second build").unwrap();
        let (second, _) = snapshot_executable(&exe, &store).unwrap();
        assert_ne!(second, first);
        assert_eq!(std::fs::read(&first).unwrap(), b"first build");
        assert_eq!(std::fs::read(&second).unwrap(), b"second build");

        // No staging copy is left behind.
        let staged = |store: &Path| -> usize {
            std::fs::read_dir(store)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
                .count()
        };
        assert_eq!(staged(&store), 0);

        // Nor by a snapshot that fails after the copy: a file sitting where
        // its digest directory belongs makes the publish step fail.
        std::fs::write(&exe, b"third build").unwrap();
        let digest = crate::preflight::compute_file_digest(&exe).unwrap();
        std::fs::write(store.join(digest.to_string()), b"in the way").unwrap();
        assert!(snapshot_executable(&exe, &store).is_err());
        assert_eq!(
            staged(&store),
            0,
            "a failed snapshot leaves no staging copy"
        );
    }
}
