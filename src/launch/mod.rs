//! How brokkr launches a program it built: cargo computes the environment,
//! brokkr validates and takes the last hop.
//!
//! A launch cargo performs (plain `run`, `run --commit`) is a `cargo run` of
//! the build's recipe with `brokkr-launch` passed as the exact-target runner.
//! Cargo builds the program's whole launch environment from what it actually
//! receives (loader path, `[env]`, `CARGO_*`, build-script `rustc-env`, the
//! rustup proxy's variables, a live jobserver) and execs the helper in its
//! place. The helper changes to the plan's directory, adds the plan's
//! `BROKKR_*` keys, writes a receipt and execs the user's runner, if any, then
//! the program, under the same PID. brokkr never reconstructs or replays an
//! environment. Every reconstruction tried got a concrete case wrong.
//!
//! - [`helper`]: the embedded static helper and its copy on disk.
//! - [`runner`]: the preflight that proves the runner override wins.
//! - [`Prepared`]: one launch's plan file, receipt and `--config` argument.
//!
//! `docs/commands/run.md` ("How a program is launched") is the contract.

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::error::DevError;

pub(crate) mod helper;
pub(crate) mod runner;

/// What one launch through the helper does after cargo hands over.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// The directory the program runs in; `None` keeps cargo's.
    pub(crate) cwd: Option<PathBuf>,
    /// `BROKKR_*` keys added over cargo's environment, never a key cargo
    /// itself sets: adding a key cargo leaves alone needs no reconstruction.
    pub(crate) sets: Vec<(OsString, OsString)>,
    /// The user's runner argv, program first; empty when none applies.
    pub(crate) runner: Vec<OsString>,
}

/// One prepared launch: the plan on disk, where its receipt will appear, and
/// the `--config` argument that makes cargo run the helper. Both files are
/// removed when it drops.
#[derive(Debug)]
pub(crate) struct Prepared {
    plan: PathBuf,
    receipt: PathBuf,
    config_arg: String,
}

/// What the receipt says about the handover. The helper creates it before it
/// reads anything, so it exists exactly when cargo ran the helper.
#[derive(Debug)]
pub(crate) enum Receipt {
    /// The helper ran and reached its exec of the runner or program with no
    /// failure recorded. With a runner, the runner decides what runs next.
    Started,
    /// The helper ran and failed, and said why on stderr.
    Failed,
    /// No receipt: cargo never ran the helper. It stopped before launching
    /// (a rebuild it failed), or ran something else.
    Missing,
    /// A receipt that exists but cannot be read: nothing can be concluded.
    Unreadable(std::io::Error),
}

impl Prepared {
    /// Write `plan` for a cargo invocation whose runner key is
    /// `target.TRIPLE.runner`; `string` writes the override as a string, to
    /// replace a user's string-valued runner without cargo's merge error.
    pub(crate) fn new(plan: &Plan, triple: &str, string: bool) -> Result<Self, DevError> {
        let helper = helper::path()?;
        let base = crate::user_dirs::launch_dir()
            .ok_or_else(|| DevError::Config("cannot write a launch plan: $HOME is not set".into()))?
            .join("plans");
        std::fs::create_dir_all(&base)?;
        let stem = format!("{}-{}", std::process::id(), unique());
        let plan_path = base.join(format!("{stem}.plan"));
        // The helper's own rule: the receipt sits beside the plan.
        let receipt = base.join(format!("{stem}.plan.receipt"));
        let config_arg = runner_config(triple, &helper, &plan_path, string)?;
        std::fs::write(&plan_path, encode(plan)?)?;
        Ok(Self {
            plan: plan_path,
            receipt,
            config_arg,
        })
    }

    /// `--config` and its value, for the cargo invocation that launches.
    pub(crate) fn config_args(&self) -> [String; 2] {
        ["--config".into(), self.config_arg.clone()]
    }

    /// Read the receipt once cargo has returned.
    pub(crate) fn receipt(&self) -> Receipt {
        match std::fs::read(&self.receipt) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Receipt::Missing,
            Err(e) => Receipt::Unreadable(e),
            Ok(text) if text == b"started\n" => Receipt::Started,
            Ok(_) => Receipt::Failed,
        }
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        for path in [&self.plan, &self.receipt] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => crate::output::warn(&format!(
                    "could not remove launch file {}: {e}",
                    crate::output::path(path)
                )),
            }
        }
    }
}

/// A value distinct across the launches of one process.
fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{nanos:09}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The plan in the helper's format: the magic, then `TAG VALUE` pairs, every
/// token NUL-terminated (`helper_main.rs`). Values are OS bytes, so a
/// non-UTF-8 path or value survives; a NUL inside one cannot be expressed and
/// refuses.
fn encode(plan: &Plan) -> Result<Vec<u8>, DevError> {
    for (i, (key, _)) in plan.sets.iter().enumerate() {
        if plan.sets[..i].iter().any(|(prev, _)| prev == key) {
            return Err(DevError::Internal(format!("launch variable {key:?} is set twice")));
        }
    }
    let mut out = b"brokkr-launch-plan-v1\0".to_vec();
    let mut push = |tag: &str, value: &OsStr| -> Result<(), DevError> {
        let bytes = value.as_bytes();
        if bytes.contains(&0) {
            return Err(DevError::Refused(format!(
                "a launch {tag} contains a NUL byte, which no process argument can carry"
            )));
        }
        out.extend_from_slice(tag.as_bytes());
        out.push(0);
        out.extend_from_slice(bytes);
        out.push(0);
        Ok(())
    };
    if let Some(cwd) = &plan.cwd {
        push("cwd", cwd.as_os_str())?;
    }
    for (key, value) in &plan.sets {
        if key.is_empty() || key.as_bytes().contains(&b'=') {
            return Err(DevError::Internal(format!(
                "launch variable name {key:?} is empty or contains `=`"
            )));
        }
        let mut entry = key.clone();
        entry.push("=");
        entry.push(value);
        push("set", &entry)?;
    }
    for arg in &plan.runner {
        push("runner", arg)?;
    }
    Ok(out)
}

/// `target.TRIPLE.runner=[HELPER, "--plan", PLAN]` as one `--config` value,
/// or with `string` the same words as one space-separated string, which cargo
/// splits on whitespace. TOML carries only UTF-8, so a helper or plan path
/// that is not refuses, as does whitespace in one under the string form;
/// both live under the per-user launch directory.
fn runner_config(triple: &str, helper: &Path, plan: &Path, string: bool) -> Result<String, DevError> {
    let text = |p: &Path| -> Result<String, DevError> {
        let s = p.to_str().ok_or_else(|| {
            DevError::Config(format!(
                "{} is not UTF-8, so cargo's --config cannot name it",
                crate::output::path(p)
            ))
        })?;
        if string && s.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(DevError::Config(format!(
                "{} contains whitespace, which a string-valued cargo runner cannot carry",
                crate::output::path(p)
            )));
        }
        Ok(s.to_owned())
    };
    let (helper, plan) = (text(helper)?, text(plan)?);
    let value = if string {
        toml::Value::String(format!("{helper} --plan {plan}"))
    } else {
        toml::Value::Array(vec![
            toml::Value::String(helper),
            toml::Value::String("--plan".into()),
            toml::Value::String(plan),
        ])
    };
    Ok(format!("target.{triple}.runner={value}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_plan_encodes_as_nul_terminated_pairs() {
        let plan = Plan {
            cwd: Some("/live tree".into()),
            sets: vec![("BROKKR_WITH_EXE".into(), "/a=b".into())],
            runner: vec!["/usr/bin/valgrind".into(), "-q".into()],
        };
        let bytes = encode(&plan).unwrap();
        assert_eq!(
            bytes,
            b"brokkr-launch-plan-v1\0cwd\0/live tree\0set\0BROKKR_WITH_EXE=/a=b\0\
              runner\0/usr/bin/valgrind\0runner\0-q\0"
                .to_vec()
        );
        let nul = Plan {
            cwd: Some(OsString::from_vec(b"a\0b".to_vec()).into()),
            ..Plan::default()
        };
        assert!(encode(&nul).is_err());
        let twice = Plan {
            sets: vec![("BROKKR_X".into(), "1".into()), ("BROKKR_X".into(), "2".into())],
            ..Plan::default()
        };
        assert!(encode(&twice).is_err());
    }

    /// Cargo parses the part after `=` as a TOML value; the quoting style is
    /// the serializer's to choose, so compare what it parses to.
    fn parsed(arg: &str) -> toml::Value {
        let (key, value) = arg.split_once('=').unwrap();
        assert_eq!(key, "target.x86_64-unknown-linux-gnu.runner");
        let table: toml::Table = format!("v = {value}").parse().unwrap();
        table["v"].clone()
    }

    #[test]
    fn the_runner_override_takes_the_form_it_replaces() {
        let triple = "x86_64-unknown-linux-gnu";
        let list = runner_config(triple, Path::new("/h/brokkr-launch"), Path::new("/p/\"x\".plan"), false)
            .unwrap();
        let argv: Vec<String> = parsed(&list)
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(argv, ["/h/brokkr-launch", "--plan", "/p/\"x\".plan"]);

        let string = runner_config(triple, Path::new("/h/brokkr-launch"), Path::new("/p/x.plan"), true).unwrap();
        assert_eq!(parsed(&string).as_str(), Some("/h/brokkr-launch --plan /p/x.plan"));
        assert!(runner_config(triple, Path::new("/h/a b"), Path::new("/p/x.plan"), true).is_err());
    }

    use std::os::unix::ffi::OsStringExt;
}
