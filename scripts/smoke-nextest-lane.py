#!/usr/bin/env python3
"""Smoke test: the `harness = "nextest"` check lane (brokkr-owned config).

Generates a throwaway workspace under scratch/nextest-lane-smoke and drives
`brokkr check` through three scenarios:

1. green: a passing suite, the sweep's `skip` filter honoured -> exit 0;
2. red: a failing test -> exit 1;
3. foreign config gets no vote: a `.config/nextest.toml` whose
   default-filter would exclude the failing test (and whose retries would
   mask it) is written into the checkout - brokkr must ignore the file
   entirely, so the run stays red. This is the load-bearing scenario: the
   engine runs under brokkr's synthesized config, and everything the lane
   runs comes from brokkr.toml.

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

FILES = {
    # `crate-type = ["rlib", ...]` on purpose: cargo then reports the lib
    # target's kind as `rlib`, the shape that made the audit's binary-id
    # construction drift from the engine's (pkg::rlib/target vs plain pkg)
    # and orphan every lib pair only the engine lane covered. With this in
    # the fixture, that exact regression turns the gate scenario red.
    "Cargo.toml": """\
[package]
name = "nsmoke"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["rlib", "staticlib", "cdylib"]
""",
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

    #[test]
    fn third_probe() {
        assert_eq!(super::add(3, 3), 6);
    }

    // Selected out by the sweep's `skip = ["skipme"]`; would fail if run.
    #[test]
    fn skipme_broken() {
        panic!("the sweep skip was not honoured by the nextest lane");
    }
}
""",
    # The gate composes a MIXED shape: `nx` (nextest) and `lt` (libtest)
    # share one compile shape, so the audit keys the whole shape by binary
    # id. `lt` runs only third_probe, which makes `adds` a pair the gate can
    # only cover through the ENGINE's claim - if the audit's binary-id
    # construction ever disagreed with the engine's, `adds` would orphan and
    # the gate would go red.
    "brokkr.toml": """\
project = "brokkr"

[[check]]
name = "nx"
harness = "nextest"
skip = ["skipme"]

[[check]]
name = "lt"
only = ["third"]

[test]
gate_profile = "gate"

[test.profiles.gate]
certifies = "complete"
sweeps = ["nx", "lt"]

[[quarantine]]
category = "doctests"
issue = "B2"
reason = "smoke workspace gates no doctests"

[[quarantine]]
pattern = "skipme"
issue = "B1"
reason = "deliberately broken smoke test, excluded by the sweep skip"
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

# A foreign config that, if consulted, would hide the failure twice over:
# the default-filter deselects the failing test and retries would need it to
# fail three times. Brokkr must never open this file.
FOREIGN_CONFIG = """\
[profile.default]
default-filter = "not test(~goes_red)"
retries = 2
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

    print("=== green: passing suite, skip honoured ===")
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

    print("=== foreign .config/nextest.toml gets no vote: still red ===")
    foreign = SMOKE / ".config/nextest.toml"
    foreign.parent.mkdir(parents=True, exist_ok=True)
    foreign.write_text(FOREIGN_CONFIG)
    rc = run_check()
    if rc != 1:
        print(
            f"FAIL foreign-config scenario: want exit 1, got {rc} - the checkout's "
            "nextest config influenced a brokkr run"
        )
        fail = 1
    shutil.rmtree(foreign.parent)
    lib.write_text(green_lib)

    print("=== gate: complete claim over a mixed nextest+libtest shape ===")
    rc = subprocess.run([str(BROKKR), "check", "--gate"], cwd=SMOKE).returncode
    if rc != 0:
        print(f"FAIL gate scenario: want exit 0, got {rc}")
        fail = 1

    print("=== gate without the quarantine: the skipped pair orphans ===")
    cfg = SMOKE / "brokkr.toml"
    full_cfg = cfg.read_text()
    head, _, _ = full_cfg.partition("[[quarantine]]\npattern")
    cfg.write_text(head)
    proc = subprocess.run(
        [str(BROKKR), "check", "--gate"], cwd=SMOKE, capture_output=True, text=True
    )
    out = proc.stdout + proc.stderr
    print(out, end="")
    if proc.returncode != 1 or "orphaned" not in out:
        print(f"FAIL orphan scenario: want exit 1 with an orphan report, got {proc.returncode}")
        fail = 1
    cfg.write_text(full_cfg)

    if fail == 0:
        print("smoke-nextest-lane: all scenarios passed")
    return fail


if __name__ == "__main__":
    sys.exit(main())
