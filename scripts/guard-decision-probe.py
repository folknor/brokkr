#!/usr/bin/env python3
"""Build this checkout's guard and exercise its admission paths in a scratch HOME.

The guard resolves both lock files under `$HOME/.brokkr`, so pointing HOME at a
temporary directory inside this checkout's `.brokkr` lets us hold the exclusive
lock and publish authorization records by hand, without touching the real lock
or needing a live brokkr. The probe never uses the system temporary directory.
Its Cargo build uses a persistent subdirectory of Cargo's resolved target
directory, separate from the target tree used by check and test.

Covers every path except the admitted one: matching the published hash requires
computing xxh3_128, which is what the guard itself does, so asserting it here
would only restate the implementation. The admit path is covered end to end by
`brokkr check` succeeding while brokkr holds its own lock - if it did not, no
build on this machine would work.

Usage: python3 scripts/guard-decision-probe.py
"""

import fcntl
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tempfile

root = pathlib.Path(__file__).resolve().parents[1]
project_state = root / ".brokkr"
system_tmp = pathlib.Path("/tmp").resolve()


def ensure_project_state_safe():
    resolved_state = project_state.resolve()
    try:
        resolved_state.relative_to(system_tmp)
    except ValueError:
        return
    raise SystemExit("project state resolves inside /tmp; refusing to write there")


def cargo_records(stdout):
    """Read Cargo's JSON records, ignoring only plainly non-JSON output."""
    for line_number, line in enumerate(stdout.splitlines(), start=1):
        candidate = line.lstrip()
        if not candidate.startswith(("{", "[")):
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise ValueError(
                f"malformed cargo JSON on stdout line {line_number}: {error}"
            ) from error
        if not isinstance(record, dict):
            raise ValueError(
                f"malformed cargo JSON on stdout line {line_number}: record is not an object"
            )
        if not isinstance(record.get("reason"), str):
            raise ValueError(
                f"malformed cargo JSON on stdout line {line_number}: missing string reason"
            )
        yield line_number, record


def resolved_cargo_target_dir(cargo_env):
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version=1", "--no-deps"],
        cwd=root,
        env=cargo_env,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print("FAILED: could not resolve Cargo's target directory")
        if result.stderr:
            print(result.stderr, file=sys.stderr, end="")
        return None
    try:
        metadata = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        print(f"FAILED: malformed cargo metadata JSON: {error}")
        return None
    target_dir = metadata.get("target_directory") if isinstance(metadata, dict) else None
    if not isinstance(target_dir, str) or not target_dir:
        print("FAILED: cargo metadata did not report a target directory")
        return None
    return pathlib.Path(target_dir)


def build_guard():
    """Build and return the executable reported for this tree's guard target."""
    # Keep this probe's artifacts out of the check and test target trees. The
    # persistent path keeps the first build warm on later check runs.
    cargo_env = dict(os.environ)
    cargo_env["CARGO_CACHE_RUSTC_INFO"] = "0"
    target_root = resolved_cargo_target_dir(cargo_env)
    if target_root is None:
        return None
    checkout_key = hashlib.sha256(str(root.resolve()).encode()).hexdigest()[:16]
    target_dir = target_root / "brokkr-guard-decision-probe" / checkout_key
    target_dir.mkdir(parents=True, exist_ok=True)
    cargo_env["CARGO_TARGET_DIR"] = str(target_dir)
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
        env=cargo_env,
        capture_output=True,
        text=True,
    )

    executable = None
    try:
        for line_number, message in cargo_records(result.stdout):
            if message["reason"] != "compiler-artifact":
                if message.get("executable") is not None:
                    raise ValueError(
                        f"unexpected cargo JSON record {message['reason']!r} with an executable "
                        f"on stdout line {line_number}"
                    )
                continue
            target = message.get("target")
            if not isinstance(target, dict) or not isinstance(target.get("name"), str):
                raise ValueError(
                    f"malformed compiler-artifact on cargo stdout line {line_number}: "
                    "missing target name"
                )
            if "executable" not in message:
                raise ValueError(
                    f"malformed compiler-artifact on cargo stdout line {line_number}: "
                    "missing executable field"
                )
            reported = message["executable"]
            if reported is not None and (
                not isinstance(reported, str) or not reported
            ):
                raise ValueError(
                    f"malformed compiler-artifact on cargo stdout line {line_number}: "
                    "executable is not a non-empty string or null"
                )
            if target["name"] != "brokkr-rustc-guard" or reported is None:
                continue
            if executable is not None and executable != reported:
                raise ValueError(
                    "cargo reported multiple brokkr-rustc-guard executables"
                )
            executable = reported
    except ValueError as error:
        print(f"FAILED: {error}")
        if result.returncode != 0 and result.stderr:
            print(result.stderr, file=sys.stderr, end="")
        return None

    if result.returncode != 0:
        print("FAILED: could not build this tree's brokkr-rustc-guard")
        if result.stderr:
            print(result.stderr, file=sys.stderr, end="")
        return None

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


def scratch_home():
    ensure_project_state_safe()
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
