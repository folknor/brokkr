#!/usr/bin/env python3
"""Smoke test: the parallel fan-out's direct execution of test binaries.

Generates a throwaway workspace under scratch/parallel-direct-smoke whose
tests assert cargo's runtime launch contract - package-root cwd, runtime
CARGO_PKG_NAME, OUT_DIR - and drives `brokkr check` over it twice:

1. sequentially (no `parallel`), where cargo itself launches the binaries:
   the reference run that defines what the contract IS on this toolchain;
2. with `parallel = {}` on a sweep carrying `test_exclude_packages`, where
   brokkr executes the prebuilt binaries directly and must reproduce the
   same contract - and where the excluded member's failing test proves the
   exclusion held.

Both runs must pass. A contract the reference run does not honour must not
be asserted here (that would gate the direct lane on more than cargo does).

Usage, from the brokkr repo root:

    python3 scripts/smoke-parallel-direct.py [path-to-brokkr]

Defaults to target/debug/brokkr. The generated directory is left behind for
inspection; it is disposable and regenerated on every run.
"""

import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SMOKE = ROOT / "scratch" / "parallel-direct-smoke"
BROKKR = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target" / "debug" / "brokkr"

FILES = {
    "Cargo.toml": """\
[package]
name = "psmoke"
version = "0.1.0"
edition = "2021"

[workspace]
members = ["excl"]
resolver = "2"
""",
    "build.rs": """\
use std::io::Write;

fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let mut f = std::fs::File::create(std::path::Path::new(&out).join("stamp.txt")).unwrap();
    f.write_all(b"stamped").unwrap();
    println!("cargo::rustc-env=SMOKE_GENERATED=yes");
}
""",
    "src/lib.rs": """\
/// Adds.
///
/// ```
/// assert_eq!(psmoke::add(2, 2), 4);
/// ```
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}

#[cfg(test)]
mod unit {
    #[test]
    fn unit_in_lib_harness() {
        assert_eq!(super::add(2, 2), 4);
    }
}
""",
    "tests/fixtures/input.txt": "fixture-content\n",
    "tests/launch_contract.rs": """\
// Each test asserts one piece of the environment cargo provides when IT
// launches a test binary. The sequential brokkr lane (cargo-mediated) is the
// reference; the parallel lane's direct execution must reproduce it.

#[test]
fn cwd_is_the_package_root() {
    // Passes under cargo, fails when launched from the workspace-external cwd.
    assert!(std::path::Path::new("tests/fixtures/input.txt").exists());
}

#[test]
fn cargo_pkg_name_is_a_runtime_var() {
    assert_eq!(std::env::var("CARGO_PKG_NAME").unwrap(), "psmoke");
}

#[test]
fn out_dir_is_a_runtime_var_with_the_build_script_output() {
    let out = std::env::var("OUT_DIR").unwrap();
    let stamp = std::path::Path::new(&out).join("stamp.txt");
    assert_eq!(std::fs::read_to_string(stamp).unwrap(), "stamped");
}

#[test]
fn rustc_env_is_a_runtime_var() {
    // Empirical probe: does cargo export build-script `rustc-env` values to
    // the test process at RUNTIME (not only via env!)? The sequential lane
    // answers for cargo; the parallel lane must give the same answer.
    assert_eq!(std::env::var("SMOKE_GENERATED").unwrap(), "yes");
}

#[test]
fn manifest_dir_matches_cwd() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(std::fs::canonicalize(manifest).unwrap(), std::fs::canonicalize(cwd).unwrap());
}
""",
    "excl/Cargo.toml": """\
[package]
name = "excl"
version = "0.1.0"
edition = "2021"
""",
    "excl/src/lib.rs": """\
#[cfg(test)]
mod tests {
    #[test]
    fn proves_exclusion_by_failing() {
        panic!("test_exclude_packages did not exclude this package");
    }
}
""",
    # Sequential first, parallel second: same selection, only the lane
    # differs. The doc-only twin is the adoption shape for a project whose
    # main sweep goes parallel - doctests cannot ride a fan-out, so the twin
    # carries them with the same selection. No `[test] doctests = true`
    # here, deliberately: with it, the sequential reference would run
    # doctests too, and the red-doctest scenario below could no longer prove
    # the DOC lane runs them (doc_only runs doctests regardless of the
    # flag - the key is the entry's own opt-in).
    "brokkr.toml": """\
project = "brokkr"

[[check]]
name = "sequential"
test_exclude_packages = ["excl"]

[[check]]
name = "fanout"
test_exclude_packages = ["excl"]
parallel = {}

[[check]]
name = "docs"
doc_only = true
test_exclude_packages = ["excl"]
""",
}

FAILING_DOCTEST = '''\
/// Adds.
///
/// ```
/// assert_eq!(psmoke::add(2, 2), 5); // deliberately wrong: proves doctests run
/// ```
pub fn add(a: u64, b: u64) -> u64 {
    a + b
}
'''


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

    proc = subprocess.run([str(BROKKR), "check"], cwd=SMOKE)
    if proc.returncode != 0:
        print("FAIL smoke-parallel-direct: brokkr check failed "
              "(sequential is the reference; fanout is the lane under test)")
        return 1

    # The doc twin must actually RUN the doctests, not merely exist: break
    # the doctest and the check must go red through the doc-only lane.
    lib = SMOKE / "src/lib.rs"
    green_lib = lib.read_text()
    lib.write_text(green_lib.replace(green_lib.split("pub fn add")[0],
                                     FAILING_DOCTEST.split("pub fn add")[0], 1))
    proc = subprocess.run([str(BROKKR), "check"], cwd=SMOKE)
    lib.write_text(green_lib)
    if proc.returncode != 1:
        print(f"FAIL smoke-parallel-direct: a broken doctest must fail the check "
              f"(want exit 1, got {proc.returncode})")
        return 1

    print("smoke-parallel-direct: all lanes passed (fanout, doc twin, red doctest)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
