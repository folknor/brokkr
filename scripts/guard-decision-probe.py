#!/usr/bin/env python3
"""Build this checkout's guard and exercise its admission paths in a scratch HOME.

The guard resolves both lock files under `$HOME/.brokkr`, so pointing HOME at a
temporary directory inside this checkout's `.brokkr` lets us hold the exclusive
lock and publish authorization records by hand, without touching the real lock
or needing a live brokkr. The probe never uses the system temporary directory.

Covers every path except the admitted one: matching the published hash requires
computing xxh3_128, which is what the guard itself does, so asserting it here
would only restate the implementation. The admit path is covered end to end by
`brokkr check` succeeding while brokkr holds its own lock - if it did not, no
build on this machine would work.

Usage: python3 scripts/guard-decision-probe.py
"""

import fcntl
import json
import os
import pathlib
import subprocess
import sys
import tempfile

root = pathlib.Path(__file__).resolve().parents[1]


def build_guard():
    """Build and return the executable reported for this tree's guard target."""
    result = subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--bin",
            "brokkr-rustc-guard",
            "--message-format=json-render-diagnostics",
        ],
        cwd=root,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print("FAILED: could not build this tree's brokkr-rustc-guard")
        if result.stderr:
            print(result.stderr, file=sys.stderr, end="")
        return None

    executable = None
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            message.get("reason") == "compiler-artifact"
            and message.get("target", {}).get("name") == "brokkr-rustc-guard"
        ):
            executable = message.get("executable")

    if executable is None:
        print("FAILED: cargo did not report the brokkr-rustc-guard executable")
        return None

    guard = pathlib.Path(executable).resolve()
    if not os.access(guard, os.X_OK):
        print(f"FAILED: built guard is not executable: {guard}")
        return None
    return str(guard)


guard = build_guard()
if guard is None:
    sys.exit(1)
real = "/usr/bin/true"
project_state = root / ".brokkr"
system_tmp = pathlib.Path("/tmp").resolve()


def scratch_home():
    resolved_state = project_state.resolve()
    try:
        resolved_state.relative_to(system_tmp)
    except ValueError:
        pass
    else:
        raise SystemExit("project state resolves inside /tmp; refusing to write there")

    project_state.mkdir(parents=True, exist_ok=True)
    return tempfile.TemporaryDirectory(prefix="guard-decision-", dir=project_state)

failures = []


def run(label, *, auth, hold_lock, env_extra, expect_pass, rustc_args=()):
    with scratch_home() as home:
        bd = pathlib.Path(home) / ".brokkr"
        bd.mkdir()
        lock = bd / "brokkr.lock"
        if auth is None:
            lock.write_text("")
        else:
            lock.write_text(f"pid=1\nstarttime=1\nboot_id=b\nauth={auth}\ndraining=0\n")

        env = dict(os.environ)
        env["HOME"] = home
        env.pop("BROKKR_HOLD_NONCE", None)
        env.pop("BROKKR_CARGO", None)
        env.update(env_extra)

        held = None
        if hold_lock:
            held = open(lock, "a+b")
            fcntl.flock(held.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            r = subprocess.run(
                [guard, real, *rustc_args], env=env, capture_output=True, text=True, timeout=30
            )
        finally:
            if held:
                held.close()

        passed = r.returncode == 0
        ok = passed == expect_pass
        verdict = "ok" if ok else "MISMATCH"
        got = "passed" if passed else f"refused({r.returncode})"
        print(f"[{verdict}] {label}: expected {'pass' if expect_pass else 'refuse'}, got {got}")
        if not ok:
            failures.append(label)
            print(f"         stderr: {r.stderr.strip()}")


# An idle machine must compile. This is the case that, done wrong, bricks every
# build on the host.
run("idle, no lock held", auth=None, hold_lock=False, env_extra={}, expect_pass=True)
run("idle, stale record left behind", auth="deadbeef", hold_lock=False, env_extra={}, expect_pass=True)

# A hold that has not published a capability yet is draining, and admits nobody.
run("hold draining, empty auth", auth="", hold_lock=True, env_extra={}, expect_pass=False)

# A hold with a capability admits only processes carrying it.
run("hold active, no capability", auth="deadbeef", hold_lock=True, env_extra={}, expect_pass=False)
run(
    "hold active, capability for another hold",
    auth="deadbeef",
    hold_lock=True,
    env_extra={"BROKKR_HOLD_NONCE": "0" * 32},
    expect_pass=False,
)

# Compiler-information queries are admitted even against a hold that would
# refuse a compile: a refused probe is PERSISTED by cargo in
# target/.rustc_info.json and replayed against correctly-stamped cargos later
# (the 2026-09-10 piners incident). The same argv with a source file is a
# compile again.
run(
    "hold active, no capability, -vV query",
    auth="deadbeef",
    hold_lock=True,
    env_extra={},
    expect_pass=True,
    rustc_args=("-vV",),
)
run(
    "hold active, no capability, cargo target probe",
    auth="deadbeef",
    hold_lock=True,
    env_extra={},
    expect_pass=True,
    rustc_args=(
        "-", "--crate-name", "___", "--print=file-names", "--crate-type", "bin",
        "--print=sysroot", "--print=cfg", "-Wwarnings",
    ),
)
run(
    "hold active, no capability, compile-continuing print stays refused",
    auth="deadbeef",
    hold_lock=True,
    env_extra={},
    expect_pass=False,
    rustc_args=("lib.rs", "--print=link-args"),
)

# The human hatch bypasses the protocol, by design.
run(
    "hold active, BROKKR_CARGO override",
    auth="deadbeef",
    hold_lock=True,
    env_extra={"BROKKR_CARGO": "1"},
    expect_pass=True,
)

print()
if failures:
    print(f"FAILED: {len(failures)} case(s): {', '.join(failures)}")
    sys.exit(1)
print("all guard decision paths behaved as specified")
