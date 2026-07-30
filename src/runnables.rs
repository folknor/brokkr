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
pub fn cmd_run(
    project_root: &Path,
    cfg: Option<&BinConfig>,
    name: Option<&str>,
    debug: bool,
    release: bool,
    args: &[String],
) -> Result<(), DevError> {
    let runnables = discover(project_root)?;
    if runnables.is_empty() {
        return Err(DevError::Build(
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
    if target.example {
        cargo_args.push("--example".into());
        cargo_args.push(target.name.clone());
    }
    if !args.is_empty() {
        cargo_args.push("--".into());
        cargo_args.extend(args.iter().cloned());
    }
    forward_cargo(&cargo_args, project_root)
}

/// `brokkr install`: install `[bin] install`'s packages, or the sole
/// bin-carrying package, via `cargo install [--debug] --path <pkg dir>`.
pub fn cmd_install(
    project_root: &Path,
    cfg: Option<&BinConfig>,
    debug: bool,
    release: bool,
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
        let mut cargo_args: Vec<String> = vec!["install".into()];
        if dev {
            cargo_args.push("--debug".into());
        }
        cargo_args.push("--path".into());
        cargo_args.push(r.package_dir.display().to_string());
        output::run_msg(&format!("cargo {}", cargo_args.join(" ")));
        forward_cargo(&cargo_args, project_root)?;
    }
    Ok(())
}

/// Spawn cargo with inherited stdio in `project_root`, mapping a non-zero
/// exit (or signal death) to `DevError::Subprocess`.
fn forward_cargo(args: &[String], project_root: &Path) -> Result<(), DevError> {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command as ProcCommand;

    let mut cmd = ProcCommand::new("cargo");
    cmd.args(args);
    cmd.current_dir(project_root);
    let status = cmd.status().map_err(|e| DevError::Subprocess {
        program: "cargo".into(),
        code: None,
        stderr: e.to_string(),
    })?;
    if status.success() {
        return Ok(());
    }
    let program = format!("cargo {}", args.first().map_or("", String::as_str));
    match status.code() {
        Some(code) => Err(DevError::Subprocess {
            program,
            code: Some(code),
            stderr: String::new(),
        }),
        None => Err(DevError::Subprocess {
            program,
            code: None,
            stderr: format!("killed by signal {}", status.signal().unwrap_or(0)),
        }),
    }
}
