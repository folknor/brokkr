# brokkr deps - cover the cargo tree + cargo metadata corpus

Status: **observation + scoping**. Not yet a committed design - the
patterns here come from two corpora of past `cargo tree ...` and
`cargo metadata ...` invocations that turned up across Claude Code
sessions in `~/.claude/projects/*.jsonl` (extracted via
`scratch/extract_cargo_tree.py <prefix>`). Use this as the
source-of-truth for "what do people actually reach for that brokkr
should subsume".

## Goal

A single `brokkr deps` (with or without flags / positional) should
answer every question that prompted the corpus invocations, without
the user needing to remember `--target all`, chained `cd`,
`2>&1 | head -N`, or hand-rolled python one-liners against
`cargo metadata`'s JSON. The corpus is the acceptance test.

## Corpus categories (cargo tree)

### A. Reverse lookup ("who depends on X?")

Most common pattern in the corpus (~14 hits). Almost every invocation
needed at least one of `--target all`, `--all-features`, `-e features`,
`--edges no-dev`, plus `cd <some/long/path>` and `2>&1 | head -N`.

- `cargo tree -i hashbrown@0.15.5 --target all`
- `cargo tree -i hashbrown --target all` (all versions)
- `cargo tree -i hashbrown@0.15.5 --target all --edges no-dev`
- `cargo tree -i hashbrown@0.15.5 --target all --all-features`
- `cargo tree -i hashbrown@0.15.5 --target all -e all`
- `cargo tree -i wasmparser@0.244.0 --target all`
- `cargo tree -i quick-xml`
- `cargo tree -i flate2 -e features`
- `cargo tree -i dirs`
- `cargo tree -p tauri-plugin-sql -i libsqlite3-sys` (member-scoped reverse)

**Current coverage:** `brokkr deps <pkg>` / `brokkr deps name@version`.
Host-target filter + Normal-kind filter are already on by default;
fallback to unfiltered chain trace when the package isn't host-active.

**Gaps surfaced by the corpus:**
- Member-scoped reverse: `brokkr deps -p <member> <pkg>` to scope
  chains to one workspace member. Currently we always span the whole
  workspace.
- Feature-edge flavour: `-e features` and `-e all` show *why* the dep
  is included (which feature pulled it in). Today `brokkr deps <pkg>`
  shows the chain but not the feature edges. The data is in
  `cargo metadata`'s `node.deps[i].dep_kinds[i].kind`/`target` and the
  per-package `features` map - reachable, just not surfaced.

### B. Forward lookup ("what does *this* member pull in?")

~10 hits. Lots of `--depth 0/1/2` plus `-p <member>` plus occasional
`--no-default-features`.

- `cargo tree -p bifrost-smtp --depth 1`
- `cargo tree -p bifrost-imap --depth 1`
- `cargo tree -p bifrost-smtp --all-features --depth 1`
- `cargo tree -p seen --depth 1 --no-default-features`
- `cargo tree -p sluggrs --depth 1`
- `cargo tree -p hotpath --depth 2 --no-dedupe`
- `cargo tree -p hotpath --depth 1 -e features`
- `cargo tree -p ratatui --depth 1`
- `cargo tree -p ratatui -e features --depth 0`
- `cargo tree --depth 1`

**Current coverage:** none. `brokkr deps` has no forward-tree mode.

**Sketch:** `brokkr deps --tree <member> [--depth N]`. Default to
depth 1 (matches the corpus majority). Honour the same host+Normal
filters. When `<member>` is omitted in a single-crate project, target
the only package. Could share rendering with the chain-trace output
in focus mode.

### C. Workspace-wide overview

- `cargo tree --depth 0 | wc -l`
- `cargo tree --workspace --prefix none --no-dedupe | sort -u | wc -l`
- `cargo tree --depth 0`

Three hits. Mostly "how big is this dep graph?" curiosity, occasionally
"X of N direct deps" framing for documents / PR descriptions.

**Sketch:** `brokkr deps --summary` or a one-line preamble before the
existing sections: "workspace: M direct deps, N transitive". Cheap to
compute from the metadata we already load. Also unlocks the "X of N
direct deps have upgrades available" framing for the outdated section,
which we punted earlier.

### D. Feature inspection

- `cargo tree -e features -i <pkg>`
- `cargo tree -f '{p} features: {f}' --depth 0 -p <member>`
- `cargo tree --features hotpath -e features -i hotpath`

5 hits. Pattern: "is feature X enabled on Y, and how did it get
enabled?" Useful when a binary mysteriously links a heavy optional
dep.

**Sketch:** `brokkr deps --features <pkg>` printing the union of
enabled features per resolved version and (when ambiguous) the feature
that activated each. Cargo's resolver records the enabled-features set
in `node.features`; the cross-reference to *which workspace dep
enabled it* requires walking the dep graph with feature-spec tracking.
This is the most expensive item on the list.

### E. Grep-narrowed scans

A handful of hits are just `cargo tree ... | grep <substring>` -
people scanning for "does anything pull in libz/rustls/foo". These are
covered today by `brokkr deps <substring>` if it's an exact crate
name, but not by partial match.

**Sketch:** treat `brokkr deps <name>` as a glob/substring when the
literal name doesn't resolve. Low priority.

## Corpus categories (cargo metadata)

The metadata corpus is almost entirely hand-rolled python or jq
one-liners against `cargo metadata --format-version 1`. Several
patterns reproduce work brokkr already does in dedicated phases -
the gap is discovery, not coverage.

### F. Package lookup by name ("tell me about X")

Most common metadata pattern (~5 hits). Variants:

- `jmap-client`: get version + source + manifest_path
- `iced` (via jq): get version + source
- `mime` (substring filter on `p['name']`): list matching pkgs
- `get_if_addrs` (substring filter): check existence
- `jmap` (substring filter): list matching pkgs

**Current coverage:** `brokkr deps <pkg>` resolves a name to its
package(s) and prints chain traces, but does *not* surface the
metadata fields people kept extracting by hand (version, source,
manifest_path). The information is right there in the loaded
metadata - just not shown.

**Sketch:** add a 1-2 line metadata preamble to focus mode output:
`<name> <version> | source=<url|crates.io|workspace> |
manifest=<path>`. When the spec is a bare name with multiple
matches, list all matches before printing chains. Treat unknown
names as substring search before erroring (overlaps with category E
from the tree corpus).

### G. Git deps enumeration

3 hits, all reaching for the same pattern with progressive bug
fixes:

- `[p for p in packages if p.get('source','').startswith('git+')]`
  → `AttributeError: 'NoneType' object has no attribute 'startswith'`
  (workspace members have `source: null`).
- Retry with `if p.get('source') and p['source'].startswith('git+')`.
- Retry with `(p.get('source') or '').startswith('git+')`.

**Current coverage:** `git_dependency` phase already does this
correctly, with ref parsing (tag/branch/rev) on top. People aren't
discovering it.

**Action:** documentation / surfacing problem, not a code gap. The
README / `brokkr deps --help` should call out git/path/duplicate/
outdated/stale phases prominently so the next person reaches for
`brokkr deps` instead of writing a one-liner.

### H. Path deps outside workspace

1 hit: `p.get('source') is None and p['id'] not in ws_members`.

**Current coverage:** `path_dependency` phase covers it exactly.
Same discovery issue as G.

### I. Workspace / package counts and listings

2 hits: `[print(p['name']) for p in packages]`, `jq '.packages |
length'`.

**Current coverage:** none directly, but the `--summary` preamble
sketched in cargo tree category C would supply the count. The
flat list-of-names is uniquely a metadata-corpus pattern.

**Sketch:** `brokkr deps --list` for a one-package-per-line plain
listing (deduped or not; default deduped). Combine with
`brokkr deps --summary` for the count headline. Both are cheap
one-pass walks over the already-loaded metadata.

### J. Build metadata (target_directory, etc.)

1 hit: `print(json.load(sys.stdin)['target_directory'])`.

Niche; probably belongs in `brokkr env` if anywhere, not
`brokkr deps`. Skip unless the pattern recurs.

### K. Exploratory dump

1 hit: print first 3 workspace members + first 5 packages with
source/manifest. This is just "what does this JSON look like" -
not a use case to chase. Skip.

## Non-goals

- Reproducing `cargo tree --invert` semantics around platform-specific
  conditional compilation (`cfg(...)` evaluation beyond what
  `--filter-platform` gives us). Cargo's evaluator is the source of
  truth; we lean on `--filter-platform=<host>`.
- Mirroring `cargo tree -e build` / `-e dev` separation. We filter to
  Normal kind by design - the user can run `cargo tree` directly for
  the rare cases where dev-deps matter.

## Order of implementation

The order falls out of how much the corpora complain, weighted by how
small the patch is:

1. **Focus mode metadata preamble + substring/glob name resolution**
   (F + E). Tiny: extend `focus.rs` to print
   `<name> <version> | source=... | manifest=...` and to fall back to
   substring matching when the literal name doesn't resolve. Absorbs
   the largest pattern in the metadata corpus.
2. **Member-scoped reverse** (A gap). Tiny patch on top of `focus.rs`:
   start the BFS from a specific workspace member instead of all of
   them.
3. **Workspace summary line + `--list`** (C + I). One-pass walks
   over already-loaded metadata. The summary line also unlocks the
   "X of N direct deps have upgrades" framing for the outdated
   section that we punted earlier.
4. **Discovery / docs surfacing** (G + H). README / `--help` should
   prominently list all phases so the next person doesn't roll
   their own `cargo metadata | python3 -c` for git/path lookups
   that brokkr already does.
5. **Forward tree** (B). New mode, but reuses the metadata loader
   and chain renderer.
6. **Feature inspection** (D). Largest piece; defer until 1-5 land.

J (build metadata) and K (exploratory dump) are excluded; not
worth the cost.

## Provenance

Both corpora extracted with `scratch/extract_cargo_tree.py`:

```
python3 scratch/extract_cargo_tree.py 'cargo tree'      # ~40 hits
python3 scratch/extract_cargo_tree.py 'cargo metadata'  # ~15 hits
```

Re-run when the corpora are meaningfully larger (e.g. quarterly).
The priorities above may shift if e.g. forward-lookup becomes more
common than reverse, or if a new metadata pattern recurs enough to
warrant first-class support.
