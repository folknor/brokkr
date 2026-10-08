//! `brokkr test --from-run` is dispatched BEFORE any configuration is parsed.
//!
//! The recorded run is the whole selection and carries its own recipe, so a
//! `brokkr.toml` that no longer parses must not stand between a failed run and
//! the command it printed. This drives the real binary in a directory whose
//! `brokkr.toml` is garbage: if the `--from-run` dispatch in `run` moved below
//! project detection, the invocation would die on the parse error instead of
//! reaching the recorded-run lookup.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fresh directory holding a `brokkr.toml` that does not parse, inside the
/// target tree (cargo's per-test scratch area), never `/tmp`.
fn garbage_config_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("from_run_dispatch").join(name);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(dir.join("brokkr.toml"), "this is = [not valid toml").expect("config");
    dir
}

/// Run brokkr in `dir`; its status, and everything it printed (brokkr renders
/// its `[error]` lines on stdout).
fn brokkr(dir: &Path, args: &[&str]) -> (Output, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_brokkr"))
        .args(args)
        .current_dir(dir)
        // The invocation records itself in the global history; keep that out
        // of the user's.
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        .output()
        .expect("brokkr runs");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out, text)
}

#[test]
fn from_run_is_dispatched_before_the_configuration_is_parsed() {
    let dir = garbage_config_dir("dispatch");

    // Control: an ordinary `test` invocation in this directory reads the
    // configuration, which does not parse. Without this the assertion below
    // would pass for a config that was never a problem.
    let (ordinary, ordinary_text) = brokkr(&dir, &["test", "some_test"]);
    assert!(!ordinary.status.success(), "{ordinary_text}");
    assert!(!ordinary_text.contains("is gone"), "the control must fail on the configuration: {ordinary_text}");
    assert!(
        ordinary_text.contains("brokkr.toml") || ordinary_text.to_lowercase().contains("toml"),
        "the control must fail on the configuration: {ordinary_text}"
    );

    // `--from-run` reaches the recorded-run lookup: there is no such run, and
    // that - not the configuration - is the failure it reports. `--list`
    // takes no lock and builds nothing.
    let (recovery, recovery_text) = brokkr(&dir, &["test", "--from-run", "1700000000000-1", "--list"]);
    assert!(!recovery.status.success(), "{recovery_text}");
    assert!(
        recovery_text.contains("run 1700000000000-1 is gone"),
        "--from-run must be dispatched before configuration parsing: {recovery_text}"
    );
}
