#!/usr/bin/env python3
"""Smoke test: a serial lane verifies its own `cargo test` build at the gate.

Generates a throwaway workspace under scratch/serial-gate-smoke - a library
with unit tests, an integration test that runs the package's bin through
CARGO_BIN_EXE, a `build_packages` support package, doctests on - and drives
`brokkr check` over it:

1. the partial default profile: green, and the serial lane enters cargo for a
   build once in the whole run (its preparation) - the test phase runs
   `cargo test` with no build of its own before it;
2. `--gate` (a complete profile): green, every expected execution passed;
3. the same workspace with a build script that reruns on every build and
   exports a fresh `rustc-env` value each time - harness bytes unchanged, the
   launch envelope not: the partial run passes with the lane superseded in its
   journal, and the gate refuses the lane's harnesses before any test runs.

Usage, from the brokkr repo root:

    python3 scripts/smoke-serial-gate.py [path-to-brokkr]

Defaults to target/release/brokkr. The generated directory is left behind for
inspection; it is disposable and regenerated on every run.
"""

import json
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SMOKE = ROOT / "scratch" / "serial-gate-smoke"
BROKKR = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target" / "release" / "brokkr"

FILES = {
    "Cargo.toml": """\
[package]
name = "gsmoke"
version = "0.1.0"
edition = "2021"

[workspace]
members = ["helper"]
default-members = [".", "helper"]
resolver = "2"
""",
    "src/lib.rs": """\
/// Adds.
///
/// ```
/// assert_eq!(gsmoke::add(2, 2), 4);
/// ```
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}

#[cfg(test)]
mod unit {
    #[test]
    fn unit_adds() {
        assert_eq!(super::add(2, 2), 4);
    }
}
""",
    "src/main.rs": """\
fn main() {
    println!("{}", gsmoke::add(1, 2));
}
""",
    "tests/runs_bin.rs": """\
#[test]
fn the_bin_prints_three() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gsmoke")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "3");
}

#[test]
fn the_support_bin_exists() {
    let dir = std::env::var("BROKKR_TEST_BIN_DIR").unwrap();
    assert!(std::path::Path::new(&dir).join("helper").exists());
}
""",
    "helper/Cargo.toml": """\
[package]
name = "helper"
version = "0.1.0"
edition = "2021"
""",
    "helper/src/main.rs": """\
fn main() {}

#[cfg(test)]
mod tests {
    #[test]
    fn helper_runs() {
        assert_eq!(1 + 1, 2);
    }
}
""",
    "brokkr.toml": """\
project = "brokkr"

[[check]]
name = "default"
build_packages = ["helper"]

[test]
doctests = true
default_profile = "edit"
gate_profile = "gate"

[test.profiles.edit]
certifies = "partial"
skip_phases = ["clippy"]
sweeps = ["default"]

[test.profiles.gate]
certifies = "complete"
sweeps = ["default"]
""",
}

# Reruns on every build (it watches a file that never exists) and exports a
# fresh value each time: the build cargo test makes is not the planned one,
# though no test reads the value and no harness byte changes.
RERUN_BUILD_SCRIPT = """\
fn main() {
    println!("cargo::rerun-if-changed=never-exists");
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    println!("cargo::rustc-env=GSMOKE_STAMP={nanos}");
}
"""

failures = []


def write(files):
    for rel, body in files.items():
        path = SMOKE / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)


def run(*args):
    proc = subprocess.run([str(BROKKR), *args], cwd=SMOKE, capture_output=True, text=True, check=False)
    print(f"$ brokkr {' '.join(args)} -> exit {proc.returncode}")
    return proc


def summary(proc):
    lines = [line for line in proc.stdout.splitlines() if line.startswith("{")]
    try:
        return json.loads(lines[-1]) if lines else {}
    except json.JSONDecodeError:
        return {}


def newest_journal():
    runs = sorted((SMOKE / ".brokkr" / "accounting").iterdir(), key=lambda p: p.stat().st_mtime)
    return (runs[-1] / "journal.jsonl").read_text() if runs else ""


def expect(desc, ok, proc=None):
    print(("ok   " if ok else "FAIL ") + desc)
    if not ok:
        failures.append(desc)
        if proc is not None:
            print(proc.stdout[-4000:])
            print(proc.stderr[-4000:])


def main():
    if SMOKE.exists():
        shutil.rmtree(SMOKE)
    SMOKE.mkdir(parents=True)
    write(FILES)
    subprocess.run(["git", "init", "-q"], cwd=SMOKE, check=True)
    subprocess.run(["git", "add", "-A"], cwd=SMOKE, check=True)

    proc = run("check", "--commands", "--json")
    expect("partial check passes (exit 10)", proc.returncode == 10, proc)
    output = proc.stdout + proc.stderr
    builds = sum(1 for line in output.splitlines() if "cargo test --no-run" in line)
    expect(f"the serial lane is built once, in preparation (saw {builds})", builds == 1, proc)
    expect("the run's own build is verified clean", "lane_superseded" not in newest_journal())

    proc = run("check", "--gate", "--json")
    s = summary(proc)
    acct = s.get("execution_accounting") or {}
    expect("gate passes (exit 0)", proc.returncode == 0, proc)
    expect(
        "gate certified every expected execution",
        acct.get("status") == "complete"
        and acct.get("expected_executions", 0) > 0
        and acct.get("passed") == acct.get("expected_executions")
        and acct.get("anomalies") == 0,
        proc,
    )

    write({"build.rs": RERUN_BUILD_SCRIPT})
    subprocess.run(["git", "add", "-A"], cwd=SMOKE, check=True)

    proc = run("check", "--json")
    expect("partial check still passes on a drifting build (exit 10)", proc.returncode == 10, proc)
    expect("the drifting lane is superseded in its journal", "lane_superseded" in newest_journal(), proc)

    proc = run("check", "--gate", "--json")
    journal = newest_journal()
    expect("gate fails on a drifting build (exit 1)", proc.returncode == 1, proc)
    expect("its harnesses were refused as an unverified build", "unverified_build" in journal, proc)
    expect("no test of the lane ran", '"event":"finished"' not in journal, proc)

    if failures:
        print(f"smoke: {len(failures)} scenario(s) failed")
        return 1
    print("smoke: all scenarios passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
