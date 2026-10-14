#!/usr/bin/env python3
"""Remove whole `## ID - ...` sections from a findings document.

Usage: drop_findings.py FILE ID [ID ...]
A section runs from its `## ` heading to the next `## ` heading (or EOF).
Fails if any requested ID is not found, so a typo never passes silently.
"""
import sys


def main() -> int:
    path, ids = sys.argv[1], set(sys.argv[2:])
    with open(path, encoding="utf-8") as f:
        lines = f.read().split("\n")
    out, skipping, seen = [], False, set()
    for line in lines:
        if line.startswith("## "):
            ident = line[3:].split(" ", 1)[0]
            skipping = ident in ids
            if skipping:
                seen.add(ident)
        if not skipping:
            out.append(line)
    missing = ids - seen
    if missing:
        print(f"not found: {sorted(missing)}", file=sys.stderr)
        return 1
    text = "\n".join(out).rstrip("\n") + "\n"
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)
    print(f"dropped {sorted(seen)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
