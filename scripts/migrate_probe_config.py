"""One-off migration of a piners pins.toml from per-probe execution facts
plus [roots] to prefix-scoped [probe_config] declarations.

Usage: python3 scripts/migrate_probe_config.py IN_PINS_TOML OUT_PINS_TOML

Old semantics: a probe runs with its pin's feed, else the feed of the
longest [roots] prefix covering its directory; bar_budget, ohlcv_start_ms
and tv_trades_csv_tz come from the pin alone.

New semantics (brokkr src/piners/registry.rs, `resolve`): each field from
the longest [probe_config] prefix covering the probe directory that sets it.

A field's value is hoisted onto a root prefix only when every pinned probe
under that root resolves some value for it (no clearing exists, so a root
value would otherwise reach a probe that had none); the value hoisted is the
most common one, and each probe that differs gets an exact-directory
declaration. A comment block directly above a moved key moves with it.

The script refuses to write unless (a) every probe resolves all four fields
identically before and after, and (b) the result satisfies the declaration
rules brokkr's loader enforces: canonical prefixes, no empty entry, every
declared field governs at least one probe, no field restates its inherited
value, no CSV timezone resolved onto a record probe.
"""

import re
import sys
import tomllib
from collections import Counter
from pathlib import PurePosixPath

FIELDS = ["feed", "bar_budget", "ohlcv_start_ms", "tv_trades_csv_tz"]
KEY_RE = re.compile(r"^(feed|bar_budget|ohlcv_start_ms|tv_trades_csv_tz)\s*=")
HEADER_RE = re.compile(r"^\[(.+)\]\s*(#.*)?$")


def parts(path):
    return PurePosixPath(path).parts


def covers(prefix, directory):
    p = parts(prefix)
    return parts(directory)[: len(p)] == p


def chain(config, directory):
    hits = [(k, v) for k, v in config.items() if covers(k, directory)]
    hits.sort(key=lambda kv: -len(parts(kv[0])))
    return hits


def resolve(config, directory):
    out = {}
    for f in FIELDS:
        out[f] = next((v[f] for _, v in chain(config, directory) if f in v), None)
    return out


def old_resolve(pin, roots, directory):
    out = {f: pin.get(f) for f in FIELDS}
    if out["feed"] is None:
        best = [(k, v) for k, v in roots.items() if covers(k, directory)]
        best.sort(key=lambda kv: -len(parts(kv[0])))
        out["feed"] = best[0][1]["feed"] if best else None
    return out


def check_rules(config, probes):
    problems = []
    for prefix, entry in config.items():
        canon = "/".join(p for p in prefix.split("/"))
        if not prefix or canon != "/".join(parts(prefix)) or any(
            c in (".", "..") for c in prefix.split("/")
        ) or prefix.startswith("/"):
            problems.append(f"{prefix}: not a canonical prefix")
        if not entry:
            problems.append(f"{prefix}: declares no field")
        above = {k: v for k, v in config.items() if k != prefix}
        inherited = resolve(above, prefix)
        for f, v in entry.items():
            if inherited[f] == v:
                problems.append(f"{prefix}: {f} restates its inherited value")
    winners = set()
    for pid, pin in probes.items():
        d = str(PurePosixPath(pin["pine"]["path"]).parent)
        c = chain(config, d)
        for f in FIELDS:
            w = next((k for k, v in c if f in v), None)
            if w is not None:
                winners.add((w, f))
        if "record" in pin and resolve(config, d)["tv_trades_csv_tz"] is not None:
            problems.append(f"{pid}: CSV timezone resolved onto a record probe")
    for prefix, entry in config.items():
        for f in entry:
            if (prefix, f) not in winners:
                problems.append(f"{prefix}: {f} governs no probe")
    return problems


def toml_value(v):
    if isinstance(v, str):
        return '"' + v.replace("\\", "\\\\").replace('"', '\\"') + '"'
    return str(v)


def main():
    src, dst = sys.argv[1], sys.argv[2]
    text = open(src, encoding="utf-8").read()
    data = tomllib.loads(text)
    roots = data.get("roots", {})
    probes = data.get("probes", {})
    dirs = {pid: str(PurePosixPath(p["pine"]["path"]).parent) for pid, p in probes.items()}
    before = {pid: old_resolve(p, roots, dirs[pid]) for pid, p in probes.items()}

    # Declarations: hoist per root and field, then exact dirs for the rest.
    config = {}
    for root in sorted(roots):
        under = [pid for pid in probes if covers(root, dirs[pid])]
        entry = {}
        for f in FIELDS:
            values = [before[pid][f] for pid in under]
            if under and all(v is not None for v in values):
                entry[f] = Counter(values).most_common(1)[0][0]
        if entry:
            config[root] = entry
    for pid in sorted(probes, key=lambda p: dirs[p]):
        have = resolve(config, dirs[pid])
        own = {f: before[pid][f] for f in FIELDS if before[pid][f] != have[f]}
        if any(v is None for v in own.values()):
            sys.exit(f"{pid}: would need to clear an inherited field: {own}")
        if own:
            config[dirs[pid]] = own

    after = {pid: resolve(config, dirs[pid]) for pid in probes}
    diffs = [pid for pid in probes if before[pid] != after[pid]]
    if diffs:
        sys.exit(f"resolution changed for {len(diffs)} probe(s): {diffs[:10]}")
    problems = check_rules(config, probes)
    if problems:
        sys.exit("rule violations:\n  " + "\n  ".join(problems))

    # Text rewrite: drop the four keys from probe blocks (keeping any comment
    # block directly above a dropped key, to move it), drop [roots].
    lines = text.split("\n")
    out = []
    moved_comments = {}  # probe dir -> comment lines
    section = None
    pending_comments = []
    in_roots = False
    roots_tail = []  # comments after a blank line inside [roots]
    roots_blank = False  # the previous [roots] line was blank
    for line in lines:
        m = HEADER_RE.match(line.strip())
        if m and not line.startswith(" "):
            section = m.group(1)
            if in_roots:
                # A comment block after the last [roots] blank line belongs to
                # the header that follows it.
                out.extend(roots_tail)
                roots_tail = []
            in_roots = section == "roots"
            roots_blank = False
            if in_roots:
                # Comments directly above the [roots] header are its own.
                for c in pending_comments:
                    if c.strip():
                        print(f"note: dropping [roots] comment: {c.strip()}")
                pending_comments = []
                continue
            out.extend(pending_comments)
            pending_comments = []
            out.append(line)
            continue
        if in_roots:
            # The [roots] body runs to the next header. Its entries and its own
            # comments are dropped; a comment block separated from the next
            # header only by being last is kept for that header.
            s = line.strip()
            if s == "":
                if roots_tail:
                    roots_tail.append(line)
                roots_blank = True
                continue
            if s.startswith("#"):
                if roots_tail:
                    roots_tail.append(line)
                elif roots_blank:
                    # Only a comment block set off by a blank line can belong
                    # to the next section; one hugging an entry is the entry's.
                    roots_tail = ["", line]
                else:
                    print(f"note: dropping [roots] comment: {s}")
                roots_blank = False
                continue
            for c in roots_tail:
                if c.strip():
                    print(f"note: dropping [roots] comment: {c.strip()}")
            roots_tail = []
            roots_blank = False
            continue
        if section and section.startswith("probes.") and line.strip().startswith("#"):
            pending_comments.append(line)
            continue
        if section and section.startswith("probes.") and KEY_RE.match(line.strip()):
            pid = section[len("probes."):].strip('"')
            if "#" in line.split("=", 1)[1] and '"' not in line.split("#", 1)[1]:
                print(f"note: {pid}: trailing comment on `{line.strip()}` moves with it")
            if pending_comments:
                moved_comments.setdefault(dirs[pid], []).extend(pending_comments)
                pending_comments = []
            continue
        out.extend(pending_comments)
        pending_comments = []
        out.append(line)
    out.extend(pending_comments)
    # [roots] was the last section: a trailing comment block has no next
    # section to move to.
    for c in roots_tail:
        if c.strip():
            print(f"note: dropping [roots] comment: {c.strip()}")

    block = [
        "# Execution facts, declared by directory prefix: each probe takes each",
        "# field from the longest prefix covering its directory that sets it.",
        "# brokkr's writers never touch this table, so a probe entry lost and",
        "# re-added comes back running exactly as before.",
    ]
    order = sorted(config, key=lambda k: (parts(k)[:1], len(parts(k)) > 1, k))
    for prefix in order:
        block.append("")
        for c in moved_comments.get(prefix, []):
            block.append(c)
        block.append(f'[probe_config."{prefix}"]')
        for f in FIELDS:
            if f in config[prefix]:
                block.append(f"{f} = {toml_value(config[prefix][f])}")

    # Insert before the first [probes.*] header, after the feeds and whatever
    # prose preceded [roots].
    first_probe = next(
        i for i, l in enumerate(out) if l.startswith("[probes.")
    )
    # Keep a comment block that belongs to the first probe with it.
    insert_at = first_probe
    while insert_at > 0 and out[insert_at - 1].startswith("#"):
        insert_at -= 1
    new_lines = out[:insert_at] + block + [""] + out[insert_at:]
    new_text = re.sub(r"\n{3,}", "\n\n", "\n".join(new_lines))

    check = tomllib.loads(new_text)
    stray = set(check) - {"feeds", "probe_config", "probes"}
    if stray:
        sys.exit(f"internal: unexpected top-level keys after the rewrite: {sorted(stray)}")
    if check.get("probe_config", {}) != config:
        sys.exit("internal: written [probe_config] differs from the computed one")
    for pid, pin in check["probes"].items():
        if any(f in pin for f in FIELDS):
            sys.exit(f"internal: {pid} still carries an execution fact")
    open(dst, "w", encoding="utf-8").write(new_text)
    print(f"{len(probes)} probes, {len(config)} declarations -> {dst}")
    for prefix in order:
        print(f"  {prefix}: {config[prefix]}")


main()
