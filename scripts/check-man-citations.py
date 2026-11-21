#!/usr/bin/env python3
"""Resolve every `brokkr man TOPIC SECTION` citation in the tree.

Scans docs/, reference/, src/, README.md and AGENTS.md for the literal
phrase, runs each distinct citation through the installed brokkr, and
prints the ones that fail. Exit status is the number of failures, capped.
Topics are project-filtered, so a citation of another project's topic fails
here as unavailable; the failure reason says which kind of failure it is.
"""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
PATTERN = re.compile(r"brokkr man ([a-z][a-z-]*) ([a-z_][a-z0-9_-]*)")
SCAN = ["docs", "reference", "src", "README.md", "AGENTS.md"]


def files():
    for entry in SCAN:
        path = ROOT / entry
        if path.is_file():
            yield path
        elif path.is_dir():
            for p in sorted(path.rglob("*")):
                if p.suffix in (".md", ".rs") and p.is_file():
                    yield p


def main():
    seen = {}
    for path in files():
        text = path.read_text(encoding="utf-8")
        for lineno, line in enumerate(text.splitlines(), 1):
            for m in PATTERN.finditer(line):
                key = (m.group(1), m.group(2))
                seen.setdefault(key, []).append(f"{path.relative_to(ROOT)}:{lineno}")

    failures = 0
    for (topic, section), sites in sorted(seen.items()):
        proc = subprocess.run(
            ["brokkr", "man", topic, section],
            capture_output=True,
            text=True,
            cwd=ROOT,
        )
        if proc.returncode != 0:
            failures += 1
            reason = (proc.stderr or proc.stdout).strip().splitlines()
            print(f"FAIL {topic} {section}: {reason[-1] if reason else 'no output'}")
            for site in sites:
                print(f"    {site}")
    print(f"{len(seen)} citations, {failures} failing")
    sys.exit(min(failures, 100))


if __name__ == "__main__":
    main()
