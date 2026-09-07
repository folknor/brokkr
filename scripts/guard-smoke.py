#!/usr/bin/env python3
"""Smoke test for brokkr-rustc-guard (src/bin/rustc_guard.rs).

Exercises the guard's three decision paths directly, without cargo:

1. lock free            -> guard execs the wrapped command (pass-through)
2. lock held (flock EX) -> guard refuses with exit 1 and names brokkr
3. lock held + BROKKR_CARGO=1 -> the escape hatch execs anyway

The brokkr-ancestor path can't be exercised from here (the parent would be
python); it is covered end-to-end by every `brokkr check` run with the
guard enrolled.

Usage: python3 scripts/guard-smoke.py [path-to-guard]
Default guard path: target/debug/brokkr-rustc-guard (falls back to release,
then ~/.cargo/bin). Exits 0 when all three paths behave, 1 otherwise.

The test holds the REAL brokkr lock (~/.brokkr/brokkr.lock) for the held
cases - momentarily, released before exit. Don't run it while a brokkr
command is mid-flight; the flock would queue behind it.
"""

import fcntl
import os
import subprocess
import sys


def find_guard() -> str:
    if len(sys.argv) > 1:
        return sys.argv[1]
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    candidates = [
        os.path.join(here, "target", "debug", "brokkr-rustc-guard"),
        os.path.join(here, "target", "release", "brokkr-rustc-guard"),
        os.path.expanduser("~/.cargo/bin/brokkr-rustc-guard"),
    ]
    for c in candidates:
        if os.path.exists(c):
            return c
    sys.exit(f"no guard binary found; tried {candidates}")


def run_guard(guard: str, extra_env: dict[str, str] | None = None):
    env = dict(os.environ)
    env.pop("BROKKR_CARGO", None)
    if extra_env:
        env.update(extra_env)
    return subprocess.run(
        [guard, "/bin/echo", "guard-passthrough"],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )


def main() -> int:
    guard = find_guard()
    print(f"guard under test: {guard}")
    failures = 0

    # 1. Lock free: expect pass-through.
    r = run_guard(guard)
    if r.returncode == 0 and "guard-passthrough" in r.stdout:
        print("PASS lock-free pass-through")
    else:
        print(f"FAIL lock-free: rc={r.returncode} stdout={r.stdout!r} stderr={r.stderr!r}")
        failures += 1

    # 2 + 3. Hold the real brokkr lock exclusively, as brokkr does.
    lock_dir = os.path.expanduser("~/.brokkr")
    os.makedirs(lock_dir, exist_ok=True)
    lock_path = os.path.join(lock_dir, "brokkr.lock")
    with open(lock_path, "a") as lf:
        fcntl.flock(lf, fcntl.LOCK_EX | fcntl.LOCK_NB)

        r = run_guard(guard)
        if r.returncode == 1 and "brokkr" in r.stderr and "guard-passthrough" not in r.stdout:
            print("PASS lock-held refusal")
            print(f"     stderr: {r.stderr.strip()}")
        else:
            print(f"FAIL lock-held: rc={r.returncode} stdout={r.stdout!r} stderr={r.stderr!r}")
            failures += 1

        r = run_guard(guard, {"BROKKR_CARGO": "1"})
        if r.returncode == 0 and "guard-passthrough" in r.stdout:
            print("PASS BROKKR_CARGO escape hatch")
        else:
            print(f"FAIL escape hatch: rc={r.returncode} stdout={r.stdout!r} stderr={r.stderr!r}")
            failures += 1

        fcntl.flock(lf, fcntl.LOCK_UN)

    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
