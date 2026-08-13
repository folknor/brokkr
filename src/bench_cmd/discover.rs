//! Bench-target discovery via `cargo metadata --no-deps`.
//!
//! The owning package is discovered alongside the target name, and every
//! invocation names both. That is not a convenience: building several bench
//! targets in one cargo invocation can link crates with `panic = "abort"`
//! against a harness compiled to unwind, which fails at link time. Deriving
//! `-p` from the target's own package makes the one-crate-at-a-time rule
//! structural - there is no way to phrase an invocation that violates it.
//!
//! ## What this cannot tell you
//!
//! Nothing here distinguishes a criterion bench from an iai (or plain libtest)
//! one. Both are `kind: ["bench"]` in cargo metadata, and both set
//! `harness = false` in the manifest, so neither source separates them. The
//! only reliable discriminator is behavioural - criterion implements `--list`
//! and iai does not - which costs a build to ask. So the index lists every
//! bench target, and a baseline verb aimed at a non-criterion one fails when
//! the child rejects the flag. See `docs/commands/bench.md`.

use std::path::Path;

use crate::error::DevError;
use crate::output;

/// One `kind: ["bench"]` target and the package that owns it.
pub struct BenchTarget {
    /// Target name, as passed to `cargo bench --bench <name>`.
    pub name: String,
    /// Owning package, as passed to `cargo bench -p <package>`.
    pub package: String,
}

/// Discover every bench target in the workspace.
pub fn discover(build_root: &Path) -> Result<Vec<BenchTarget>, DevError> {
    let captured = output::run_captured(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps"],
        build_root,
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
        let Some(pkg_name) = pkg.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(targets) = pkg.get("targets").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for target in targets {
            let Some(kinds) = target.get("kind").and_then(serde_json::Value::as_array) else {
                continue;
            };
            if !kinds.iter().any(|v| v.as_str() == Some("bench")) {
                continue;
            }
            let Some(name) = target.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            out.push(BenchTarget {
                name: name.to_owned(),
                package: pkg_name.to_owned(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Resolve a requested target name against the discovered set.
///
/// A name matching several targets is an error naming all of them rather than
/// a silent pick: which package a bench lands in decides what gets linked with
/// it, so guessing here is exactly the wrong move.
pub fn resolve<'a>(
    targets: &'a [BenchTarget],
    wanted: &str,
) -> Result<&'a BenchTarget, DevError> {
    let matches: Vec<&BenchTarget> = targets.iter().filter(|t| t.name == wanted).collect();
    match matches.as_slice() {
        [] => Err(DevError::Build(format!(
            "no bench target named '{wanted}'; bare `brokkr bench` lists what exists"
        ))),
        [one] => Ok(*one),
        many => {
            let mut msg = format!("'{wanted}' names {} bench targets:\n", many.len());
            for t in many {
                msg.push_str(&format!("  {} ({})\n", t.name, t.package));
            }
            msg.push_str("disambiguate the target names in Cargo.toml");
            Err(DevError::Build(msg))
        }
    }
}

/// Render the bare-is-an-index listing.
pub fn index(targets: &[BenchTarget]) -> String {
    let mut msg = format!("{} bench targets:\n", targets.len());
    for t in targets {
        if t.name == t.package {
            msg.push_str(&format!("  {}\n", t.name));
        } else {
            msg.push_str(&format!("  {} ({})\n", t.name, t.package));
        }
    }
    msg.push_str("measure one with `brokkr bench <name>`");
    msg
}
