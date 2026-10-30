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

/// Stand-in for an absent `brokkr run` target name.
///
/// `brokkr run --release -- ARGS` puts `--` in the name position as far as
/// clap is concerned, so the first raw argument would be swallowed as a
/// target name. `bare_run_sentinel` rewrites that `--` into this token
/// before parsing; `cmd_run` reads it back as `None` and falls through to
/// `[bin] default` / the sole runnable. It contains a NUL, so no real
/// target name can collide with it.
pub const NO_NAME: &str = "\u{0}brokkr-bare-run";

/// Flags of `brokkr run` that take a separate value argument. Derived from
/// clap so adding an option cannot silently desynchronise the argv pre-pass.
/// Build on a larger-stack helper thread: constructing brokkr's full clap
/// tree overflows the small stacks used by libtest workers.
fn run_value_flags() -> Option<&'static [String]> {
    static FLAGS: std::sync::OnceLock<Option<Vec<String>>> = std::sync::OnceLock::new();
    FLAGS
        .get_or_init(|| {
            let built = std::thread::Builder::new().stack_size(8 << 20).spawn(|| {
                use clap::CommandFactory;

                let mut cli = crate::cli::Cli::command();
                let Some(run) = cli.find_subcommand_mut("run") else {
                    return Vec::new();
                };
                run.get_arguments()
                    .filter(|arg| arg.get_action().takes_values())
                    .flat_map(|arg| {
                        arg.get_long()
                            .map(|name| format!("--{name}"))
                            .into_iter()
                            .chain(arg.get_short().map(|name| format!("-{name}")))
                    })
                    .collect()
            });
            built.ok().and_then(|thread| thread.join().ok())
        })
        .as_deref()
}

/// Rewrite `run [flags] -- ARGS...` so the `--` no longer lands in the
/// name position. Returns the argv unchanged in every other shape,
/// including `run NAME -- ARGS...`.
///
/// Anchored on the subcommand position, `argv[1]`: `Cli` has no top-level
/// options that take a value (only clap's `--help`/`--version`, which end
/// parsing), so the subcommand is always the first argument. Matching the
/// first `run` anywhere would rewrite a `run` that is some other command's
/// value - a mogwai target named `run`, `history --command run` - whenever a
/// `--` followed it.
pub fn bare_run_sentinel(args: Vec<String>) -> Vec<String> {
    let run_at = 1;
    if args.get(run_at).is_none_or(|a| a != "run") {
        return args;
    }
    let Some(value_flags) = run_value_flags() else {
        // If the helper thread cannot be created, leave argv untouched so
        // clap reports the invocation instead of using a stale hand-maintained
        // list of value-taking options.
        return args;
    };
    let mut i = run_at + 1;
    while args.get(i).is_some_and(|a| a.starts_with('-') && a != "--") {
        // `--features=a` carries its value inline; `--features a` does not.
        let takes_value = value_flags.contains(&args[i]);
        i += if takes_value { 2 } else { 1 };
    }
    let mut args = args;
    if args.get(i).is_some_and(|a| a == "--") {
        args[i] = NO_NAME.to_owned();
    }
    args
}

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

    let name = name.filter(|n| *n != NO_NAME);
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

/// `brokkr run [NAME]`: resolve the runnable, then exec
/// `cargo run [--release] -p <pkg> [--example <name>] [-- <args>]`.
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
    let mut cargo_args: Vec<String> = vec!["run".into()];
    cargo_args.extend(shape.args);
    // Always name the target explicitly. `-p` alone leaves a multi-bin
    // package ambiguous, and cargo bails rather than guessing.
    cargo_args.push(if target.example { "--example" } else { "--bin" }.into());
    cargo_args.push(target.name.clone());
    if !args.is_empty() {
        cargo_args.push("--".into());
        cargo_args.extend(args.iter().cloned());
    }
    // The program's own exit code is the command's: cargo has nothing of its
    // own to add once the program ran, so echoing it as a brokkr error and
    // exiting 1 only lost the code.
    match run_cargo_with_env(&cargo_args, &shape.env, lock)? {
        0 => Ok(()),
        code => Err(DevError::ExitCode(code)),
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
#[allow(clippy::too_many_arguments)]
pub fn cmd_run_commit(
    worktree: &Path,
    run_dir: &Path,
    cfg: Option<&BinConfig>,
    name: Option<&str>,
    choice: Option<crate::build::RustProfile>,
    feat: &FeatureArgs<'_>,
    args: &[String],
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
    let exe = crate::build::cargo_build(&build, worktree)?;
    let exe_str = exe.path.display().to_string();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = output::run_passthrough_in(
        &exe_str,
        &arg_refs,
        Some(run_dir),
        &[],
        output::SpawnProfile::Plain,
        Some(lock),
    )?;
    // Same contract as `cmd_run`: the program's code, silently.
    match out.failed_code(crate::output::child::Exits::SUCCESS) {
        None => Ok(()),
        Some(code) => Err(DevError::ExitCode(code)),
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
    snapshot_executable(&exe.path, store)
}

/// Copy `exe` to `store/<xxh128>/<file name>` and return that path.
///
/// The cargo-uplifted path is rewritten by the next build of anything sharing
/// it, so a script holding it runs whatever was built last. The snapshot is
/// keyed by its own content and never written again once in place: the copy
/// is hashed (not the source, which a build could replace in between), then
/// renamed into its digest directory, and an identical earlier snapshot is
/// reused as it is. The file keeps its name, so it is the same executable
/// under another directory - which is all it is: a program that looks beside
/// itself for siblings or resources sees the snapshot directory.
fn snapshot_executable(exe: &Path, store: &Path) -> Result<PathBuf, DevError> {
    let file_name = exe.file_name().ok_or_else(|| {
        DevError::Build(format!(
            "built executable has no file name: {}",
            exe.display()
        ))
    })?;
    std::fs::create_dir_all(store)?;
    let staging = Staging(store.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        file_name.to_string_lossy()
    )));
    std::fs::copy(exe, &staging.0)?;
    let digest = crate::preflight::compute_file_digest(&staging.0)?;
    let dir = store.join(digest.to_string());
    let dest = dir.join(file_name);
    if !dest.is_file() {
        std::fs::create_dir_all(&dir)?;
        std::fs::rename(&staging.0, &dest)?;
    }
    Ok(dest)
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
                self.0.display()
            )),
        }
    }
}

struct InstallPackage {
    name: String,
    package_dir: PathBuf,
}

/// Resolve configured install package names against one metadata snapshot:
/// package names, never bin names or directory basenames, and an unknown
/// package refused.
fn resolve_install_packages(
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
        out.push(InstallPackage {
            name: pkg.clone(),
            package_dir: first.package_dir.clone(),
        });
    }
    Ok(out)
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
    let (runnables, workspace_root) = discover_workspace(build_root)?;
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
    let selected = resolve_install_packages(&runnables, &install)?;
    let plan = crate::install_step::preflight(steps, build_root)?;

    // A lock cargo cannot find is a lock `--locked` would only make it abort
    // over. Look where cargo does: every package discovered here is a member
    // of one workspace, and `cargo install --path <member>` reads that
    // workspace's `Cargo.lock` at metadata's `workspace_root` - not the member
    // directory, and not wherever brokkr happened to be invoked from.
    let has_lock = workspace_root.join("Cargo.lock").is_file();
    for package in selected {
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
        output::run_msg(&format!("cargo {}", cargo_args.join(" ")));
        let code = run_cargo_with_env(&cargo_args, &shape.env, lock)?;
        if code != 0 {
            // Cargo printed its own error; this names which brokkr step died.
            return Err(DevError::Reported(format!(
                "cargo install failed for {} (exit code {code})",
                package.name
            )));
        }
    }
    plan.execute(lock)
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
/// into the lockfile so `kill --hard` and `brokkr lock` can see it. This is the
/// same regression `run_passthrough_timed` was written to fix for elivagar's
/// passthrough paths; `run`/`install` were spawning cargo directly and so never
/// got the fix.
///
/// No `current_dir`: `build_root` is cwd by construction, which is where the
/// caller's `project_root` argument comes from.
pub(crate) fn run_cargo(
    args: &[String],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<i32, DevError> {
    run_cargo_with_env(args, &[], lock)
}

fn run_cargo_with_env(
    args: &[String],
    env: &[(String, String)],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<i32, DevError> {
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
    Ok(out
        .failed_code(crate::output::child::Exits::SUCCESS)
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(args: &[&str]) -> Vec<String> {
        bare_run_sentinel(args.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_snapshot_is_keyed_by_content_and_survives_a_rebuild() {
        let root = crate::test_scratch::scratch("runnables", "snapshot");
        let exe = root.join("target/release/tool");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        let store = crate::clean::layout::ProjectState::new(&root).artifacts_dir();

        std::fs::write(&exe, b"first build").unwrap();
        let first = snapshot_executable(&exe, &store).unwrap();
        assert_eq!(first.file_name().unwrap(), "tool", "the name is kept");
        // The same bytes again reuse the snapshot rather than rewrite it.
        assert_eq!(snapshot_executable(&exe, &store).unwrap(), first);

        // A rebuild replaces the uplifted file; the snapshot a script holds
        // keeps the bytes it was handed, and the new build gets its own.
        std::fs::write(&exe, b"second build").unwrap();
        let second = snapshot_executable(&exe, &store).unwrap();
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

    #[test]
    fn bare_run_with_separator_yields_sentinel() {
        assert_eq!(
            rewrite(&["brokkr", "run", "--release", "--", "--variations", "64"]),
            vec!["brokkr", "run", "--release", NO_NAME, "--variations", "64"]
        );
    }

    #[test]
    fn named_run_is_untouched() {
        let argv = [
            "brokkr",
            "run",
            "--release",
            "bench",
            "--",
            "--threads",
            "1",
        ];
        assert_eq!(rewrite(&argv), argv.to_vec());
    }

    #[test]
    fn value_flag_does_not_end_the_scan() {
        assert_eq!(
            rewrite(&["brokkr", "run", "--features", "a,b", "--", "--help"]),
            vec!["brokkr", "run", "--features", "a,b", NO_NAME, "--help"]
        );
    }

    #[test]
    fn inline_value_flag_does_not_end_the_scan() {
        assert_eq!(
            rewrite(&["brokkr", "run", "--features=a", "--", "--help"]),
            vec!["brokkr", "run", "--features=a", NO_NAME, "--help"]
        );
    }

    #[test]
    fn commit_value_does_not_end_the_scan() {
        assert_eq!(
            rewrite(&[
                "brokkr", "run", "--commit", "a5cc1f8", "--", "--preset", "mtf"
            ]),
            vec![
                "brokkr", "run", "--commit", "a5cc1f8", NO_NAME, "--preset", "mtf"
            ]
        );
    }

    #[test]
    fn named_run_after_features_is_untouched() {
        let argv = ["brokkr", "run", "--features", "a", "bench", "--", "-h"];
        assert_eq!(rewrite(&argv), argv.to_vec());
    }

    #[test]
    fn other_commands_are_untouched() {
        let argv = ["brokkr", "check", "--", "x"];
        assert_eq!(rewrite(&argv), argv.to_vec());
    }

    #[test]
    fn run_as_another_commands_value_is_untouched() {
        let argv = ["brokkr", "mogwai", "run", "--", "--threads", "1"];
        assert_eq!(rewrite(&argv), argv.to_vec());
        let argv = ["brokkr", "history", "--command", "run", "--", "x"];
        assert_eq!(rewrite(&argv), argv.to_vec());
    }
}
