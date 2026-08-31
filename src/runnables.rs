//! `brokkr run` / `brokkr install`: workspace runnable discovery + dispatch.
//!
//! Both commands discover their targets from `cargo metadata --no-deps`
//! rather than requiring config: every workspace member's bin targets (and,
//! for `run`, example targets) are runnable by target name. The `[bin]`
//! section curates what discovery leaves ambiguous - a `default` for bare
//! `brokkr run`, an `install` package list for multi-bin workspaces, and a
//! `debug` profile default shared by both commands (release when unset,
//! matching `brokkr test`; `--debug`/`--release` override).

use std::path::{Path, PathBuf};

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

/// Flags of `brokkr run` that take a separate value argument. The
/// pre-pass has to know them, or `run --features x -- ARGS` stops its scan
/// on `x` and never sees the `--` it was looking for.
const RUN_VALUE_FLAGS: [&str; 2] = ["--features", "-F"];

/// Rewrite `run [flags] -- ARGS...` so the `--` no longer lands in the
/// name position. Returns the argv unchanged in every other shape,
/// including `run NAME -- ARGS...`.
pub fn bare_run_sentinel(args: Vec<String>) -> Vec<String> {
    let Some(run_at) = args.iter().position(|a| a == "run") else {
        return args;
    };
    let mut i = run_at + 1;
    while args.get(i).is_some_and(|a| a.starts_with('-') && a != "--") {
        // `--features=a` carries its value inline; `--features a` does not.
        let takes_value = RUN_VALUE_FLAGS.contains(&args[i].as_str());
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

impl FeatureArgs<'_> {
    /// Append the cargo flags this selection implies.
    fn extend(&self, cargo_args: &mut Vec<String>) {
        for list in self.features {
            cargo_args.push("--features".into());
            cargo_args.push(list.clone());
        }
        if self.all {
            cargo_args.push("--all-features".into());
        }
        if self.no_default {
            cargo_args.push("--no-default-features".into());
        }
    }
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

/// Whether the CLI flags / `[bin] debug` resolve to the dev profile.
/// Precedence: `--debug` / `--release` (mutually exclusive, clap-enforced)
/// > `[bin] debug` > release.
fn resolve_debug(cfg: Option<&BinConfig>, debug: bool, release: bool) -> bool {
    if debug {
        return true;
    }
    if release {
        return false;
    }
    cfg.is_some_and(|c| c.debug)
}

/// Discover every bin and example target in the workspace via
/// `cargo metadata --no-deps`.
fn discover(project_root: &Path) -> Result<Vec<Runnable>, DevError> {
    let captured = output::run_captured(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps"],
        project_root,
    )?;
    if !captured.status.success() {
        let stderr = String::from_utf8_lossy(&captured.stderr);
        return Err(DevError::Build(format!("cargo metadata failed: {stderr}")));
    }
    let stdout = String::from_utf8_lossy(&captured.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout)?;
    let packages = val
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| DevError::Build("cargo metadata missing \"packages\"".into()))?;

    let mut out = Vec::new();
    for pkg in packages {
        let pkg_name = pkg
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DevError::Build("package metadata missing \"name\"".into()))?;
        let manifest = pkg
            .get("manifest_path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| DevError::Build("package metadata missing \"manifest_path\"".into()))?;
        let package_dir = Path::new(manifest)
            .parent()
            .ok_or_else(|| DevError::Build(format!("manifest path has no parent: {manifest}")))?
            .to_owned();
        let Some(targets) = pkg.get("targets").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for target in targets {
            let Some(kinds) = target.get("kind").and_then(serde_json::Value::as_array) else {
                continue;
            };
            let kind_of = |k: &str| kinds.iter().any(|v| v.as_str() == Some(k));
            let example = kind_of("example");
            if !example && !kind_of("bin") {
                continue;
            }
            let Some(name) = target.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            out.push(Runnable {
                name: name.to_owned(),
                package: pkg_name.to_owned(),
                package_dir: package_dir.clone(),
                example,
            });
        }
    }
    Ok(out)
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

/// `brokkr run [NAME]`: resolve the runnable, then exec
/// `cargo run [--release] -p <pkg> [--example <name>] [-- <args>]`.
#[allow(clippy::too_many_arguments)]
pub fn cmd_run(
    project_root: &Path,
    cfg: Option<&BinConfig>,
    name: Option<&str>,
    debug: bool,
    release: bool,
    feat: &FeatureArgs<'_>,
    args: &[String],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<(), DevError> {
    let runnables = discover(project_root)?;
    if runnables.is_empty() {
        return Err(DevError::Build(
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
                    let origin = if name.is_none() { " ([bin] default)" } else { "" };
                    return Err(DevError::Build(format!(
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
                    return Err(DevError::Build(msg));
                }
            }
        }
        None if runnables.len() == 1 => &runnables[0],
        None => {
            // Bare-is-an-index: several candidates and no default is a
            // listing, not an error (the sync/service shape).
            let mut msg = format!("{} runnable targets:\n", runnables.len());
            for r in &runnables {
                msg.push_str(&index_line(r));
                msg.push('\n');
            }
            msg.push_str("run one with `brokkr run <name>`, or set [bin] default");
            output::run_msg(msg.trim_end());
            return Ok(());
        }
    };

    let mut cargo_args: Vec<String> = vec!["run".into()];
    if !resolve_debug(cfg, debug, release) {
        cargo_args.push("--release".into());
    }
    cargo_args.push("-p".into());
    cargo_args.push(target.package.clone());
    // Always name the target explicitly. `-p` alone leaves a multi-bin
    // package ambiguous, and cargo bails rather than guessing.
    cargo_args.push(if target.example { "--example" } else { "--bin" }.into());
    cargo_args.push(target.name.clone());
    feat.extend(&mut cargo_args);
    if !args.is_empty() {
        cargo_args.push("--".into());
        cargo_args.extend(args.iter().cloned());
    }
    forward_cargo(&cargo_args, lock)
}

/// The `[bin] install` packages with their bin target names, through the
/// same discovery `cmd_install` resolves against - package names, never bin
/// names or directory basenames, and an unknown package refused in the same
/// words. `check`'s install-feature phase reads this so the checked set and
/// the installed set cannot drift.
pub fn install_bin_targets(
    project_root: &Path,
    install: &[String],
) -> Result<Vec<(String, Vec<String>)>, DevError> {
    let runnables = discover(project_root)?;
    let mut out = Vec::new();
    for pkg in install {
        let bins: Vec<String> = runnables
            .iter()
            .filter(|r| !r.example && &r.package == pkg)
            .map(|r| r.name.clone())
            .collect();
        if bins.is_empty() {
            return Err(DevError::Build(format!(
                "[bin] install names '{pkg}', which has no bin target \
                 in this workspace"
            )));
        }
        out.push((pkg.clone(), bins));
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
/// path - `check`, `test`, `run`, the install-feature phase - reads the
/// lockfile, so without it `install` is the one command that ships a
/// dependency set no gate ever validated, and an upstream crate publishing
/// a breaking minor breaks the deploy of a tree nobody touched. Whatever
/// `check` was green on is what gets installed.
///
/// `unlocked` restores cargo's default. A tree with no `Cargo.lock` is
/// re-resolved either way - cargo refuses `--locked` without one - and says
/// so rather than failing.
pub fn cmd_install(
    project_root: &Path,
    cfg: Option<&BinConfig>,
    debug: bool,
    release: bool,
    unlocked: bool,
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<(), DevError> {
    let runnables = discover(project_root)?;
    let bins: Vec<&Runnable> = runnables.iter().filter(|r| !r.example).collect();

    let selected: Vec<&Runnable> = match cfg.map(|c| c.install.as_slice()) {
        Some(list) if !list.is_empty() => {
            let mut out = Vec::new();
            for pkg in list {
                // First bin of the package carries the package_dir; `cargo
                // install --path` installs every bin the package has.
                let Some(r) = bins.iter().find(|r| &r.package == pkg) else {
                    return Err(DevError::Build(format!(
                        "[bin] install names '{pkg}', which has no bin target \
                         in this workspace"
                    )));
                };
                out.push(*r);
            }
            out
        }
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
                    return Err(DevError::Build(
                        "no bin targets in this workspace - nothing to install".into(),
                    ))
                }
                [_] => pkgs,
                many => {
                    let mut msg =
                        format!("{} packages carry bin targets:\n", many.len());
                    for r in many {
                        msg.push_str(&format!("  {}\n", r.package));
                    }
                    msg.push_str("list the ones to install under [bin] install");
                    return Err(DevError::Build(msg));
                }
            }
        }
    };

    let dev = resolve_debug(cfg, debug, release);
    for r in selected {
        // A lock cargo cannot find is a lock `--locked` would only make it
        // abort over. Look where cargo does: the package's own directory
        // first, then the workspace root that owns it.
        let locked = !unlocked
            && (r.package_dir.join("Cargo.lock").is_file()
                || project_root.join("Cargo.lock").is_file());
        if !unlocked && !locked {
            output::run_msg(&format!(
                "no Cargo.lock for {} - installing with cargo's own \
                 dependency resolution, not the one check validated",
                r.package
            ));
        }
        let mut cargo_args: Vec<String> = vec!["install".into()];
        if dev {
            cargo_args.push("--debug".into());
        }
        if locked {
            cargo_args.push("--locked".into());
        }
        cargo_args.push("--path".into());
        cargo_args.push(r.package_dir.display().to_string());
        output::run_msg(&format!("cargo {}", cargo_args.join(" ")));
        forward_cargo(&cargo_args, lock)?;
    }
    Ok(())
}

/// Spawn cargo with inherited stdio, mapping a non-zero exit to
/// `DevError::Subprocess`.
///
/// Goes through [`output::run_passthrough_timed`] rather than a bare
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
fn forward_cargo(
    args: &[String],
    lock: Option<&crate::lockfile::LockGuard>,
) -> Result<(), DevError> {
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = output::run_passthrough_timed("cargo", &arg_refs, lock)?;
    if out.code == 0 {
        return Ok(());
    }
    Err(DevError::Subprocess {
        program: format!("cargo {}", args.first().map_or("", String::as_str)),
        code: Some(out.code),
        stderr: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(args: &[&str]) -> Vec<String> {
        bare_run_sentinel(args.iter().map(|s| (*s).to_owned()).collect())
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
        let argv = ["brokkr", "run", "--release", "bench", "--", "--threads", "1"];
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
    fn named_run_after_features_is_untouched() {
        let argv = ["brokkr", "run", "--features", "a", "bench", "--", "-h"];
        assert_eq!(rewrite(&argv), argv.to_vec());
    }

    #[test]
    fn other_commands_are_untouched() {
        let argv = ["brokkr", "check", "--", "x"];
        assert_eq!(rewrite(&argv), argv.to_vec());
    }
}
