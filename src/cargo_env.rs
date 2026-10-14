//! `[cargo_env]`: environment variables brokkr computes, fresh, for every
//! cargo it launches.
//!
//! ```toml
//! [cargo_env]
//! BA_SOURCE_IDENTITY = "python3 scripts/source_identity.py"
//! ```
//!
//! Before each cargo invocation brokkr launches directly - every spawn whose
//! caller names the cargo [`SpawnProfile`] (the captured, sidecar and
//! passthrough runners in `output/child.rs`, the process-group spawn in
//! `test_runner/procgroup.rs`) - every entry's
//! command runs with `sh -c` in the directory cargo is about to run in, and its
//! trimmed stdout is set on that cargo as the variable. The use it exists for:
//! a build script that must follow a content-exact input cargo cannot watch
//! through mtimes, declaring `cargo::rerun-if-env-changed=NAME` instead of
//! rerunning on every build - a rerun build script dirties its package, so the
//! difference is every consumer recompiling on every cargo invocation.
//!
//! What the contract is, precisely:
//!
//! - **Per invocation.** Nothing is cached: a source edit between two cargo
//!   invocations of one run changes the value the second one gets. Each
//!   evaluation is a point-in-time reading; an edit during the build itself is
//!   the command's to detect, not brokkr's.
//! - **cargo launched directly by brokkr.** A cargo a script check, a test or a
//!   build script runs is outside it, as is the `cargo --version` probe in
//!   `env.rs`. Invocations that run no build script skip the evaluation
//!   ([`needs_evaluation`]): `metadata`, `clean`, `tree`, ..., and cargo's own
//!   `--version`/`--help`.
//! - **Where it runs.** cwd is cargo's launch directory - the worktree for a
//!   `--commit` build, so a relative command runs that tree's own script
//!   against that tree. The environment is the one that cargo gets (inherited
//!   plus the caller's pairs, the hold capability included), less every
//!   configured name, so no entry reads a stale value of itself or another.
//! - **Failure is fatal.** A non-zero exit, a kill on the deadline, output that
//!   is empty, too long, not UTF-8, multi-line or carries a control character,
//!   or output that did not close cleanly, fails that cargo spawn - and every
//!   later one: the first failure is recorded process-wide ([`seal`]), so a
//!   caller that tolerates a failed build and retries cannot run past it, and
//!   the command exits non-zero whatever path it took.
//! - **Build inputs, not test environment.** The value is set on cargo, so its
//!   build scripts and rustc see it, and a test `cargo test` runs inherits it
//!   incidentally. The direct-execution lanes do not set it: tests must not
//!   read it at runtime.
//! - **Not overridable by cargo.** A cargo config `[env]` entry for the name
//!   (whose `force = true` would replace what brokkr hands cargo, and whose
//!   plain form would mean two sources for one input) is refused, checked
//!   against each invocation's own config chain and `--config` arguments.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub use crate::check_cmd::Phase;
use crate::error::DevError;
use crate::output;

/// Wall limit on one entry's evaluation.
const EVAL_DEADLINE: Duration = Duration::from_secs(30);
/// Raw stdout an evaluation may produce; more fails it, read or not.
const STDOUT_LIMIT: usize = 4096;
/// Stderr kept for the failure message; the rest is dropped and said to be.
const STDERR_LIMIT: usize = 16 * 1024;

/// The spawn caller chooses whether this child is Cargo. This deliberately
/// avoids classifying by executable basename, which misses wrappers and
/// rustup's `+toolchain` proxy form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnProfile {
    Plain,
    Cargo { phase: Option<Phase> },
}

/// The configured entries, by name (sorted - entries are independent, so the
/// order is only what makes evaluation deterministic).
static REGISTRY: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// The first evaluation failure of this process, with the `check` phase it
/// happened in (when there was one).
static FAILED: Mutex<Option<FailureRecord>> = Mutex::new(None);

#[derive(Debug, Clone)]
struct FailureRecord {
    message: String,
    phase: Option<Phase>,
}

/// Install the project's `[cargo_env]` entries. Called by project detection;
/// detection of one checkout installs the same entries however often it runs.
pub fn install(entries: &BTreeMap<String, String>) {
    let mut reg = REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *reg = entries
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
}

fn snapshot() -> Vec<(String, String)> {
    REGISTRY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The phase a recorded failure happened in, if one is recorded.
pub fn failure() -> Option<(String, Option<Phase>)> {
    FAILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|record| (record.message.clone(), record.phase))
}

/// The command's result with any recorded failure folded in: a run that
/// swallowed an evaluation failure somewhere still ends in error. An
/// interrupt or a propagated exit code keeps its own meaning.
pub fn seal(result: Result<(), DevError>) -> Result<(), DevError> {
    match (result, failure()) {
        (Ok(()), Some(_)) => Err(DevError::CargoEnv(REPORTED.into())),
        (r, _) => r,
    }
}

/// Set every entry's value on a cargo selected by the spawn caller, with
/// `args` in `cwd` (the process's own when `None`) over `env`. A no-op for the
/// plain spawn profile, invocations that run no build script, and no entries.
pub fn apply(
    cmd: &mut Command,
    profile: SpawnProfile,
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
) -> Result<(), DevError> {
    let SpawnProfile::Cargo { phase } = profile else {
        return Ok(());
    };
    let entries = snapshot();
    if entries.is_empty() || !needs_evaluation(args) {
        return Ok(());
    }
    if failure().is_some() {
        // Refused for the rest of the run; the failure was printed once,
        // when it was recorded.
        return Err(DevError::CargoEnv(REPORTED.into()));
    }
    let dir: PathBuf = match cwd {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    if let Some(conflict) = config_conflict(&names, &dir, args, env)? {
        return Err(record(conflict, phase));
    }
    for (name, command) in &entries {
        if crate::shutdown::is_shutdown_requested() {
            return Err(DevError::Interrupted);
        }
        let started = Instant::now();
        match evaluate(command, &dir, env, &names) {
            Ok(value) => {
                output::detail(&format!(
                    "cargo_env {name}={value} ({} in {}, for cargo {})",
                    fmt_ms(started.elapsed()),
                    dir.display(),
                    subcommand(args).unwrap_or("?")
                ));
                cmd.env(name, value);
            }
            Err(Failure::Interrupted) => return Err(DevError::Interrupted),
            Err(Failure::Failed(why)) => {
                return Err(record(
                    format!("{name}: `{command}` (in {}) {why}", dir.display()),
                    phase,
                ));
            }
        }
    }
    if crate::shutdown::is_shutdown_requested() {
        return Err(DevError::Interrupted);
    }
    Ok(())
}

/// The label every `[cargo_env]` error carries: the detail is printed once,
/// when the first failure is recorded, and never again.
const REPORTED: &str = "cargo_env failed";

/// Record the first failure - printed once, here, so a caller that swallows
/// the error cannot lose it and one that reports it cannot repeat it - and
/// return the error for this spawn.
fn record(message: String, phase: Option<Phase>) -> DevError {
    let mut failed = FAILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if failed.is_none() {
        output::error(&format!("cargo_env: {message}"));
        *failed = Some(FailureRecord { message, phase });
    }
    DevError::CargoEnv(REPORTED.into())
}

fn fmt_ms(d: Duration) -> String {
    format!("{}ms", d.as_millis())
}

/// Subcommands that run no build script, and so get no evaluation.
const NO_BUILD: &[&str] = &[
    "metadata",
    "clean",
    "tree",
    "locate-project",
    "pkgid",
    "version",
    "help",
    "fmt",
    "search",
    "owner",
    "login",
    "logout",
    "read-manifest",
    "verify-project",
];

/// Cargo's global options that take a value as the next argument.
const GLOBAL_WITH_VALUE: &[&str] = &["--config", "-Z", "-C", "--color", "--explain"];

/// The subcommand: the first argument past `+toolchain` and cargo's global
/// options. `None` when there is none before a `--` or the end.
fn subcommand<'a>(args: &[&'a str]) -> Option<&'a str> {
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        if a == "--" {
            return None;
        }
        if a.starts_with('+') {
            i += 1;
        } else if GLOBAL_WITH_VALUE.contains(&a) {
            i += 2;
        } else if a.starts_with('-') {
            i += 1;
        } else {
            return Some(a);
        }
    }
    None
}

/// Whether this cargo invocation may run a build script. Conservative: an
/// argv brokkr cannot classify evaluates. Cargo's own `--version`/`--help`
/// count only before the subcommand and before `--` (`cargo test -- --help`
/// builds, then hands `--help` to the harness).
fn needs_evaluation(args: &[&str]) -> bool {
    for &a in args {
        if a == "--" {
            break;
        }
        if !a.starts_with('-') && !a.starts_with('+') {
            break;
        }
        if matches!(a, "-V" | "--version" | "-h" | "--help" | "--list") {
            return false;
        }
    }
    !subcommand(args).is_some_and(|s| NO_BUILD.contains(&s))
}

/// A cargo config `[env]` entry naming a configured variable, in the chain
/// this invocation reads or a `--config` it was given.
fn config_conflict(
    names: &[&str],
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<Option<String>, DevError> {
    let names_in = |doc: &toml::Table| -> Option<String> {
        let table = doc.get("env")?.as_table()?;
        names
            .iter()
            .find(|n| table.contains_key(**n))
            .map(|n| (*n).to_owned())
    };
    let launch = crate::rustflags::CargoLaunchConfig::discover(
        cwd,
        crate::rustflags::EffectiveEnv::new(env),
    )?;
    for entry in launch.env {
        if names.contains(&entry.key.as_str()) {
            return Ok(Some(conflict_message(
                &entry.key,
                &entry.source.display().to_string(),
            )));
        }
    }
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        if a == "--" {
            break;
        }
        let value = if a == "--config" {
            i += 1;
            args.get(i).copied()
        } else {
            a.strip_prefix("--config=")
        };
        if let Some(v) = value {
            let file = cwd.join(v);
            if file.is_file() {
                for (config_path, doc) in crate::rustflags::read_config_file(&file)? {
                    if let Some(name) = names_in(&doc) {
                        return Ok(Some(conflict_message(
                            &name,
                            &config_path.display().to_string(),
                        )));
                    }
                }
            } else if let Some(name) = names
                .iter()
                .find(|n| v.trim_start().starts_with(&format!("env.{n}")))
            {
                return Ok(Some(conflict_message(name, &format!("--config {v}"))));
            }
        }
        i += 1;
    }
    Ok(None)
}

fn conflict_message(name: &str, source: &str) -> String {
    format!(
        "{name}: cargo config sets it too ([env] in {source}); brokkr computes it per invocation, \
         so a second source is refused - remove that entry"
    )
}

enum Failure {
    Interrupted,
    Failed(String),
}

/// Run one entry's command and return its value.
fn evaluate(
    command: &str,
    cwd: &Path,
    env: &[(&str, &str)],
    names: &[&str],
) -> Result<String, Failure> {
    // `output` owns the shared deadline, process-group, drain and interrupt
    // behavior. Scrub configured variables in the wrapper shell before the
    // command is evaluated; the names come from config validation and are
    // valid shell identifiers.
    let script = format!("unset {}; exec sh -c \"$1\" </dev/null", names.join(" "));
    let args = ["-c", script.as_str(), "brokkr-cargo-env", command];
    let signal_guard = crate::shutdown::SigtermGuard::install();
    let pgid = std::cell::Cell::new(None::<u32>);
    let observe_spawn = |pid: u32| pgid.set(Some(pid));
    // Capture one byte past each limit: enough to tell "too long" from "at
    // the limit", and no more held in memory however much the command prints.
    let capture_result = crate::output::child::run_captured_bounded(
        "sh",
        &args,
        cwd,
        env,
        EVAL_DEADLINE,
        Some(&observe_spawn),
        crate::shutdown::ChildIsolation::ProcessGroup(&signal_guard),
        crate::output::child::CaptureCap {
            stdout: STDOUT_LIMIT + 1,
            stderr: STDERR_LIMIT + 1,
        },
    );
    // The evaluator's shell must not leave grandchildren behind, even when it
    // exits successfully and those grandchildren closed the captured pipes.
    if let Some(pgid) = pgid.get() {
        crate::process::signal_process_group(pgid, libc::SIGKILL).ok();
    }
    let capture = capture_result.map_err(|error| match error {
        DevError::Interrupted => Failure::Interrupted,
        other => Failure::Failed(format!("could not be started or waited on: {other}")),
    })?;
    if capture.captured.ending.is_deadline() {
        return Err(Failure::Failed("was killed after 30s".into()));
    }
    let mut stderr = capture.captured.stderr;
    let stderr_was_cut = stderr.len() > STDERR_LIMIT;
    stderr.truncate(STDERR_LIMIT);
    let mut stderr_text = printable(&stderr);
    if stderr_was_cut {
        stderr_text.push_str(&format!(" [stderr cut at {STDERR_LIMIT} bytes]"));
    }
    let with_stderr = |why: String| {
        if stderr_text.trim().is_empty() {
            why
        } else {
            format!("{why}; stderr:\n{}", stderr_text.trim_end())
        }
    };
    if capture.output_cut {
        return Err(Failure::Failed(with_stderr(
            "output did not close cleanly after the command exited".into(),
        )));
    }
    if capture.captured.stdout.len() > STDOUT_LIMIT {
        return Err(Failure::Failed(with_stderr(format!(
            "printed more than {STDOUT_LIMIT} bytes"
        ))));
    }
    if !matches!(
        &capture.captured.ending,
        crate::output::child::Ending::Exited(0)
    ) {
        let ending = match &capture.captured.ending {
            crate::output::child::Ending::Exited(code) => format!("exit status: {code}"),
            crate::output::child::Ending::Signalled(signal) => format!("signal: {signal}"),
            crate::output::child::Ending::StopMarker(marker) => {
                format!("unexpected stop marker: {marker}")
            }
            crate::output::child::Ending::Deadline(_) => "deadline exceeded".into(),
            crate::output::child::Ending::Interrupted => "interrupted".into(),
            crate::output::child::Ending::WaitFailed(message) => {
                format!("wait failed: {message}")
            }
        };
        return Err(Failure::Failed(with_stderr(format!("failed ({ending})"))));
    }
    let Ok(text) = String::from_utf8(capture.captured.stdout) else {
        return Err(Failure::Failed(with_stderr(
            "printed something that is not UTF-8".into(),
        )));
    };
    let value = text.trim();
    if value.is_empty() {
        return Err(Failure::Failed(with_stderr("printed nothing".into())));
    }
    if value.chars().any(char::is_control) {
        return Err(Failure::Failed(with_stderr(
            "printed more than one line, or a control character - the value must be one line"
                .into(),
        )));
    }
    Ok(value.to_owned())
}

/// Stderr for a message: lossy, with control characters other than newline
/// and tab made visible rather than replayed onto the terminal.
fn printable(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '?'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_build_capable_invocations_evaluate() {
        assert!(needs_evaluation(&["test", "--no-run", "--tests"]));
        assert!(needs_evaluation(&["build", "-p", "x"]));
        assert!(needs_evaluation(&["+nightly", "clippy", "--all-targets"]));
        assert!(needs_evaluation(&["--config", "x=1", "test"]));
        assert!(!needs_evaluation(&[
            "metadata",
            "--no-deps",
            "--format-version",
            "1"
        ]));
        assert!(!needs_evaluation(&["--config", "x=1", "metadata"]));
        assert!(!needs_evaluation(&["clean", "-p", "x"]));
        assert!(!needs_evaluation(&["--version"]));
        assert!(!needs_evaluation(&["-V"]));
        // Past the subcommand or past `--`, a help flag is not cargo's: these
        // build first.
        assert!(needs_evaluation(&["test", "--", "--help"]));
        assert!(needs_evaluation(&["run", "--", "--version"]));
        // Nothing classifiable evaluates.
        assert!(needs_evaluation(&[]));
    }

    #[test]
    fn a_value_is_one_trimmed_line() {
        let dir = crate::test_scratch::scratch("cargo_env", "value");
        assert_eq!(
            evaluate("printf '  abc123 \\n'", &dir, &[], &[])
                .ok()
                .unwrap(),
            "abc123"
        );
        let fails = |cmd: &str| match evaluate(cmd, &dir, &[], &[]) {
            Err(Failure::Failed(why)) => why,
            _ => panic!("{cmd} should fail"),
        };
        assert!(fails("true").contains("printed nothing"));
        assert!(fails("printf 'a\\nb'").contains("one line"));
        assert!(fails("echo oops >&2; exit 3").contains("oops"));
        assert!(fails("head -c 5000 /dev/zero | tr '\\0' a").contains("more than"));
    }

    #[test]
    fn the_evaluator_never_sees_a_configured_name() {
        let dir = crate::test_scratch::scratch("cargo_env", "names");
        let got = evaluate(
            "printf '%s' \"${FX_ID:-unset}\"",
            &dir,
            &[("FX_ID", "stale")],
            &["FX_ID"],
        );
        assert_eq!(got.ok().unwrap(), "unset");
    }

    #[test]
    fn a_cargo_config_env_entry_conflicts() {
        let dir = crate::test_scratch::scratch("cargo_env", "conflict");
        assert!(
            config_conflict(
                &["FX_ID"],
                &dir,
                &["--config", "env.FX_ID=\"x\"", "build"],
                &[]
            )
            .unwrap()
            .is_some()
        );
        assert!(
            config_conflict(
                &["FX_ID"],
                &dir,
                &["--config", "env.OTHER=\"x\"", "build"],
                &[]
            )
            .unwrap()
            .is_none()
        );
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(dir.join(".cargo/config.toml"), "[env]\nFX_ID = \"x\"\n").unwrap();
        assert!(
            config_conflict(&["FX_ID"], &dir, &["build"], &[])
                .unwrap()
                .is_some()
        );
    }
}
