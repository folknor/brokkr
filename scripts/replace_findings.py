#!/usr/bin/env python3
"""Replace whole `## ID - ...` sections in a findings document.

Usage: replace_findings.py FILE REPLACEMENTS
REPLACEMENTS is a markdown file of `## ID - title` sections; each replaces the
section with the same ID in FILE (heading included). Fails if any replacement
ID is missing from FILE, so a typo never passes silently.
"""
import sys


def sections(lines):
    """Yield (id or None, [lines]) chunks split at `## ` headings."""
    current_id, chunk = None, []
    for line in lines:
        if line.startswith("## "):
            yield current_id, chunk
            current_id, chunk = line[3:].split(" ", 1)[0], []
        chunk.append(line)
    yield current_id, chunk


def main() -> int:
    target, repl_path = sys.argv[1], sys.argv[2]
    with open(repl_path, encoding="utf-8") as f:
        repl = {i: c for i, c in sections(f.read().rstrip("\n").split("\n")) if i}
    with open(target, encoding="utf-8") as f:
        lines = f.read().rstrip("\n").split("\n")
    out, done = [], set()
    for ident, chunk in sections(lines):
        if ident in repl:
            out.extend(repl[ident] + [""])
            done.add(ident)
        else:
            out.extend(chunk)
    missing = set(repl) - done
    if missing:
        print(f"not found: {sorted(missing)}", file=sys.stderr)
        return 1
    text = "\n".join(out).rstrip("\n") + "\n"
    while "\n\n\n" in text:
        text = text.replace("\n\n\n", "\n\n")
    with open(target, "w", encoding="utf-8") as f:
        f.write(text)
    print(f"replaced {sorted(done)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
