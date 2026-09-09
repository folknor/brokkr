#!/usr/bin/env python3
"""Exercise brokkr-rustc-guard's admission paths against a scratch HOME.

The guard resolves both lock files under `$HOME/.brokkr`, so pointing HOME at a
temporary directory lets us hold the exclusive lock and publish authorization
records by hand, without touching the real lock or needing a live brokkr.

Covers every path except the admitted one: matching the published hash requires
computing xxh3_128, which is what the guard itself does, so asserting it here
would only restate the implementation. The admit path is covered end to end by
`brokkr check` succeeding while brokkr holds its own lock - if it did not, no
build on this machine would work.

Usage: python3 scripts/guard-decision-probe.py <path-to-brokkr-rustc-guard>
"""

import fcntl
import os
import pathlib
import subprocess
import sys
import tempfile

guard = sys.argv[1] if len(sys.argv) > 1 else str(
    pathlib.Path.home() / ".cargo" / "bin" / "brokkr-rustc-guard"
)
real = "/usr/bin/true"

failures = []


def run(label, *, auth, hold_lock, env_extra, expect_pass):
    with tempfile.TemporaryDirectory() as home:
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
            r = subprocess.run([guard, real], env=env, capture_output=True, text=True, timeout=30)
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
