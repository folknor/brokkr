#!/usr/bin/env python3
"""Refuse brokkr source paths, Rust paths and type names in docs/.

docs/ says how brokkr is used; source paths belong in reference/ or a code
comment (reference/how-to-write-docs.md). docs/ also quotes consumer
projects' own src/ paths in config examples, so a bare `src/` cannot be
refused. The module list is read from brokkr's own src/ at run time, so a
new module is covered the moment it exists:

- `crate::` and `DevError::` are always refused.
- `src/NAME...` is refused when NAME is a top-level brokkr module (a file
  stem or directory under src/) that is not in GENERIC, the names a consumer
  repo shares.
- A deeper path under a GENERIC module (`src/output/console.rs`) is refused
  when that exact path exists in brokkr's tree; `src/main.rs` or
  `src/lib.rs` alone are not.
- A backticked compound CamelCase word (`MechanicalDb`, `LaneExec::Parallel`)
  is refused when brokkr's src/ defines a struct, enum, trait or type of
  that name. Compound only: a single capitalised word (`Running`) is as
  likely cargo's output or plain English. Unquoted prose is not scanned; a
  config key or consumer name never takes this shape unless brokkr defines
  the same type.

Prints each hit as path:line and ends with a summary line, which is what the
script check matches. Exit status is the number of hits, capped.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "src"
DOCS = ROOT / "docs"

# Module names a consumer project's tree plausibly has too. A path into one
# of these is refused only when it names a deeper file brokkr really has.
GENERIC = {
    "bin", "build", "cargo", "cli", "config", "context", "db", "diagnostic",
    "env", "error", "git", "globs", "header", "health", "history", "host",
    "lex", "lib", "main", "man", "output", "process", "profile", "project",
    "request", "scope", "wc",
}

RUST_PATH = re.compile(r"\b(?:crate|DevError)::")
SRC_PATH = re.compile(r"\bsrc/([A-Za-z0-9_]+)((?:/[A-Za-z0-9_.-]+)*)")
TYPE_DEF = re.compile(r"\b(?:struct|enum|trait|type)\s+([A-Z][A-Za-z0-9]*[a-z0-9][A-Z][A-Za-z0-9]*)")
TICKED_TYPE = re.compile(r"`([A-Z][A-Za-z0-9]*)(?:::\w+)*`")


def types():
    names = set()
    for p in SRC.rglob("*.rs"):
        names.update(TYPE_DEF.findall(p.read_text(encoding="utf-8")))
    return names


def modules():
    names = set()
    for p in SRC.iterdir():
        if p.is_dir():
            names.add(p.name)
        elif p.suffix == ".rs":
            names.add(p.stem)
    return names - GENERIC


def refused(top, rest, distinctive):
    if top in distinctive:
        return True
    if top in GENERIC and rest:
        return (SRC / (top + rest)).exists()
    return False


def main():
    distinctive = modules()
    internal = types()
    hits = 0
    for path in sorted(DOCS.rglob("*.md")):
        text = path.read_text(encoding="utf-8")
        for lineno, line in enumerate(text.splitlines(), 1):
            found = [m.group(0) for m in RUST_PATH.finditer(line)]
            for m in SRC_PATH.finditer(line):
                rest = m.group(2).rstrip(".")
                if refused(m.group(1), rest, distinctive):
                    found.append(m.group(0).rstrip("."))
            found += [m.group(0) for m in TICKED_TYPE.finditer(line) if m.group(1) in internal]
            for what in found:
                hits += 1
                print(f"{path.relative_to(ROOT)}:{lineno}: {what}")
    if hits:
        print("docs/ says how brokkr is used; move source paths to reference/ or a code comment")
    print(f"{hits} source paths in docs/")
    sys.exit(min(hits, 100))


if __name__ == "__main__":
    main()
