#!/usr/bin/env python3
"""Smoke test: the `harness = "nextest"` check lane.

Generates a throwaway workspace under scratch/nextest-lane-smoke with a
`.config/nextest.toml` and drives `brokkr check` through three scenarios:

1. green: a bounded nextest profile (terminating slow-timeout), a passing
   suite, the sweep's `skip` filter honoured -> exit 0;
2. red: a failing test -> exit 1;
3. unbounded: the terminating timeout removed -> refusal naming
   slow-timeout, before any test runs.

Usage, from the brokkr repo root:

    python3 scripts/smoke-nextest-lane.py [path-to-brokkr]

Defaults to target/debug/brokkr. The generated directory is left behind for
inspection; it is disposable and regenerated on every run.
"""

import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SMOKE = ROOT / "scratch" / "nextest-lane-smoke"
BROKKR = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target" / "debug" / "brokkr"

BOUNDED = """\
[profile.default]
slow-timeout = { period = "30s", terminate-after = 2 }
"""

UNBOUNDED = """\
[profile.default]
slow-timeout = "30s"
"""

FILES = {
    "Cargo.toml": """\
[package]
name = "nsmoke"
version = "0.1.0"
edition = "2021"
""",
    ".config/nextest.toml": BOUNDED,
    "src/lib.rs": """\
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add(2, 2), 4);
    }

    // Selected out by the sweep's `skip = ["skipme"]`; would fail if run.
    #[test]
    fn skipme_broken() {
        panic!("the sweep skip was not honoured by the nextest lane");
    }
}
""",
    "brokkr.toml": """\
project = "brokkr"

[[check]]
name = "nx"
harness = "nextest"
skip = ["skipme"]
""",
}

FAILING_TEST = """\

#[cfg(test)]
mod red {
    #[test]
    fn goes_red() {
        assert_eq!(1, 2, "deliberate failure for the smoke");
    }
}
"""


def run_check() -> int:
    return subprocess.run([str(BROKKR), "check"], cwd=SMOKE).returncode


def main() -> int:
    if not BROKKR.is_file():
        print(f"smoke: brokkr binary not found at {BROKKR}", file=sys.stderr)
        return 2
    shutil.rmtree(SMOKE, ignore_errors=True)
    for rel, content in FILES.items():
        path = SMOKE / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
    subprocess.run(["git", "init", "-q"], cwd=SMOKE, check=True)
    subprocess.run(["git", "add", "-A"], cwd=SMOKE, check=True)

    fail = 0

    print("=== green: bounded profile, passing suite, skip honoured ===")
    rc = run_check()
    if rc != 0:
        print(f"FAIL green scenario: want exit 0, got {rc}")
        fail = 1

    print("=== red: a failing test fails the check ===")
    lib = SMOKE / "src/lib.rs"
    green_lib = lib.read_text()
    lib.write_text(green_lib + FAILING_TEST)
    rc = run_check()
    if rc != 1:
        print(f"FAIL red scenario: want exit 1, got {rc}")
        fail = 1
    lib.write_text(green_lib)

    print("=== unbounded: no terminating timeout is a refusal ===")
    (SMOKE / ".config/nextest.toml").write_text(UNBOUNDED)
    rc = run_check()
    if rc != 1:
        print(f"FAIL unbounded scenario: want exit 1, got {rc}")
        fail = 1
    (SMOKE / ".config/nextest.toml").write_text(BOUNDED)

    if fail == 0:
        print("smoke-nextest-lane: all scenarios passed")
    return fail


if __name__ == "__main__":
    sys.exit(main())
