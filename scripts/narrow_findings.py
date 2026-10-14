#!/usr/bin/env python3
"""Rewrite findings entries to a stated residue.

Usage: narrow_findings.py STATUS_FILE NOTES_FILE [NOTES_FILE ...]
STATUS_FILE holds lines `ID OPEN - <what remains>` (other lines are ignored).
For every such ID found in a notes file, the section keeps its `## ` heading
and its `Reported by:` line, and its body becomes `Residue: <what remains>`.
Prints which IDs were rewritten and which were not found in any file.
"""
import re
import sys

LINE = re.compile(r"^([A-Z]+-\d+) OPEN - (.+)$")


def load_status(path):
    out = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            m = LINE.match(line.strip())
            if m:
                out[m.group(1)] = m.group(2).strip()
    return out


def rewrite(path, residue, done):
    with open(path, encoding="utf-8") as f:
        lines = f.read().rstrip("\n").split("\n")
    out, i = [], 0
    while i < len(lines):
        line = lines[i]
        ident = line[3:].split(" ", 1)[0] if line.startswith("## ") else None
        if ident in residue:
            j = i + 1
            while j < len(lines) and not lines[j].startswith("## "):
                j += 1
            body = lines[i + 1:j]
            reported = next((b for b in body if b.startswith("Reported by:")), None)
            out.append(line)
            out.append("")
            if reported:
                out.append(reported)
                out.append("")
            out.append(f"Residue: {residue[ident]}")
            out.append("")
            done.add(ident)
            i = j
        else:
            out.append(line)
            i += 1
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(out).rstrip("\n") + "\n")


def main():
    residue = load_status(sys.argv[1])
    done = set()
    for path in sys.argv[2:]:
        rewrite(path, residue, done)
    print(f"rewrote {len(done)}: {sorted(done)}")
    missing = sorted(set(residue) - done)
    if missing:
        print(f"not found: {missing}")


if __name__ == "__main__":
    main()
