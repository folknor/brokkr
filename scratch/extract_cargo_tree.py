#!/usr/bin/env python3
"""Extract every Bash invocation matching a prefix (default `cargo tree`)
from Claude Code transcripts under ~/.claude/projects.

For each Bash tool_use whose command contains the prefix, the command
is split on shell separators (`&&`, `;`, newlines) and each piece is
checked independently. Only the piece carrying the prefix is emitted,
so a long multi-line bash script with the prefix buried inside no
longer dumps the whole script as one collapsed line. Inline pipes
(`|`) and redirections (`2>&1`) inside a piece are preserved.

The prefix has to appear at a word boundary (start of the piece, or
preceded by whitespace) - drops false positives where the literal
text appears inside a quoted string passed to another tool.

Usage:
    python3 scratch/extract_cargo_tree.py              # default 'cargo tree'
    python3 scratch/extract_cargo_tree.py 'cargo metadata'
"""
from __future__ import annotations

import json
import pathlib
import sys

ROOT = pathlib.Path.home() / ".claude" / "projects"


def iter_commands():
    for path in sorted(ROOT.rglob("*.jsonl")):
        try:
            fh = path.open()
        except OSError as e:
            print(f"# skip {path}: {e}", file=sys.stderr)
            continue
        with fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except json.JSONDecodeError:
                    continue
                msg = rec.get("message")
                if not isinstance(msg, dict):
                    continue
                content = msg.get("content")
                if not isinstance(content, list):
                    continue
                for block in content:
                    if not isinstance(block, dict):
                        continue
                    if block.get("type") != "tool_use" or block.get("name") != "Bash":
                        continue
                    inp = block.get("input")
                    if not isinstance(inp, dict):
                        continue
                    cmd = inp.get("command")
                    if isinstance(cmd, str):
                        yield cmd


def split_pieces(cmd: str):
    """Split a shell command on top-level `&&`, `;`, or newline -
    respecting `"` / `'` quoting and backslash escapes so separators
    inside quoted strings (e.g. a multi-line `python3 -c "..."` body)
    don't chop the command mid-heredoc."""
    pieces: list[str] = []
    cur: list[str] = []
    in_quote: str | None = None
    i = 0
    n = len(cmd)
    while i < n:
        c = cmd[i]
        if in_quote:
            cur.append(c)
            if c == "\\" and i + 1 < n:
                cur.append(cmd[i + 1])
                i += 2
                continue
            if c == in_quote:
                in_quote = None
            i += 1
            continue
        if c in ('"', "'"):
            in_quote = c
            cur.append(c)
        elif c == "\\" and i + 1 < n:
            cur.append(c)
            cur.append(cmd[i + 1])
            i += 2
            continue
        elif c == ";" or c == "\n":
            pieces.append("".join(cur))
            cur = []
        elif c == "&" and i + 1 < n and cmd[i + 1] == "&":
            pieces.append("".join(cur))
            cur = []
            i += 2
            continue
        else:
            cur.append(c)
        i += 1
    pieces.append("".join(cur))
    return pieces


def extract_pieces(cmd: str, prefix: str):
    """Yield each shell-piece of `cmd` that starts with `prefix`. Multi-
    line content inside the piece (e.g. a python heredoc) is preserved
    verbatim. Drops occurrences where the prefix appears mid-piece
    without a word boundary - those are almost always inside another
    program's quoted argument list."""
    for raw in split_pieces(cmd):
        piece = raw.strip()
        if not piece:
            continue
        idx = piece.find(prefix)
        if idx < 0:
            continue
        if idx > 0 and not piece[idx - 1].isspace():
            continue
        yield piece[idx:]


def main(argv: list[str]) -> int:
    prefix = argv[1] if len(argv) > 1 else "cargo tree"
    if not ROOT.is_dir():
        print(f"not found: {ROOT}", file=sys.stderr)
        return 1
    first = True
    for cmd in iter_commands():
        for piece in extract_pieces(cmd, prefix):
            if not first:
                # Blank line between hits so multi-line pieces (heredocs,
                # python -c bodies) stay visually separated.
                print()
            print(piece)
            first = False
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
