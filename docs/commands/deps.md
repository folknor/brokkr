# brokkr deps

Audits `Cargo.lock` (and `cargo metadata`) for dependency smells. Works in any
Rust+git repo - no `brokkr.toml` required.

Status legend: **[v1]** = first cut, **[planned]** = designed, not built,
**[idea]** = on the wishlist, may never ship.

## Goal

One opinionated command that bundles the checks we keep running by hand
(`cargo tree -d`, `cargo audit`, `cargo outdated`, eyeballing the lockfile)
into a single report scoped to *our* projects. The target reader is "someone
touching `Cargo.toml` who wants to know whether they just made things worse."

Not a replacement for `cargo audit` / `cargo deny` - those have richer rule
languages and CI integrations. `brokkr deps` is the daily-driver triage view.

## Architecture

Mirrors `brokkr check`:

- Phase-based. Each smell is a phase that emits zero or more events.
- `DepsEvent` enum in `src/deps/mod.rs`, serde-tagged like `CheckEvent` in
  `src/cargo_json.rs`.
- Default output: prefixed text via `[deps]`, grouped by phase, capped by
  `--limit N` with a "+N hidden" trailer.
- `--json` emits NDJSON on stdout, one event per line. Every run ends
  with a `summary` event listing the phases that ran plus a findings
  count.
- Exit code: `1` if any findings, `0` otherwise. `--no-fail` always
  exits 0 (useful for report-only CI runs).

Adding a phase later: one new `DepsEvent` variant, one new module, wire
it into `run()`, add a render arm. No changes to callers.

## Data source

Shells out to `cargo metadata --format-version 1` once per run and
deserializes a minimal subset (packages, workspace members, resolve
graph). No extra crate dependencies. Network phases shell out to
existing tools (`ccu` today, `cargo audit` planned) - no native
network code in brokkr.

## Checks

### duplicate_version [v1]

Same crate resolved at >=2 versions. The point is **assigning blame**:
when foo 1.0 and foo 2.0 both ship, you want to know which of your direct
deps is anchoring the old one.

Algorithm: for each `(crate, version)` instance, BFS backwards through
the resolve graph until each branch hits a workspace member. The first
non-workspace hop on each path is the blame anchor; the workspace member
itself is the anchor if the path is just one hop (`(direct)` suffix).
Anchors are emitted as a sorted, deduped list per pin.

Each event carries one `VersionPin` per resolved version, with a sorted
`direct_blame` list and a `paths` list (each path is a chain of
`"name version"` labels from a workspace member to the target).

Text renderer prints the blame lines by default:

```
[deps] 1 crate with multiple versions:
[deps]   foo: 2 versions
[deps]     1.0.3  blamed on: old-lib 0.4
[deps]     2.1.0  blamed on: new-lib 2.0, our-crate (direct)
```

`--chains` adds example chains under each blame line, capped to 3 per
pin unless `--all` is also set. (In big trees the same chain repeats
across workspace members and floods the report.)

False-positive risk: low. The blame computation is deterministic.

### git_dependency [v1]

Any package with `source = "git+..."`. Emits `GitDependency` with the repo
URL, resolved commit SHA (from the source fragment), and the requested ref
(branch / tag / rev) if the manifest specified one.

### path_dependency [v1]

Any package with no source that isn't a workspace member. Workspace
path-linking is the whole point of a workspace, but a path dep *outside* the
workspace is usually a dev shortcut (forgot to publish a fork) or a hand-
patched dependency that wouldn't reproduce on a clean checkout. Emits
`PathDependency` with the resolved manifest path.

### outdated [v1]

Shells out to `ccu --json` (the user's check-updates tool, pinned at
`schema_version=1`). Emits one `Outdated` event per non-up-to-date
direct dep, carrying name + installed + latest + severity
(`patch`/`minor`/`major`) + source_file + line_number.

These events are informational - they don't count toward the failure-
counting findings tally. A patch bump shouldn't fail your build; you
look at the report and decide.

If `ccu` is missing or fails for any reason (offline, schema mismatch,
crash), the phase emits a single `ToolMissing` event with a reason
string and skips. Doesn't fail the run.

### advisory [planned]

Shells out to `cargo audit --json`, adapts findings into `Advisory`
events with id, severity, patched-in version, and kind
(`vulnerability` / `unmaintained`). Same `ToolMissing` skip pattern.

### yanked [idea]

`cargo audit` covers some yanks via advisories but not all. Could add
a direct sparse-index lookup phase later if we find it's missing real
problems.

### single_publisher [idea]

Crates whose only publisher is a single user account with <N total
crates, or <M monthly downloads. Supply-chain hygiene signal. Risky
to surface because it implicates real people - keep it `info`-only
if it ships.

## CLI

```
brokkr deps                       # all phases, terse text output
brokkr deps --chains              # add per-pin example chains under blame lines
brokkr deps --json                # NDJSON
brokkr deps --limit 50            # cap shown items per phase (default 20)
brokkr deps --all                 # no cap; implies --chains
brokkr deps --no-fail             # exit 0 even when findings exist
```

Exit code: `1` if any findings (duplicate / git / path) exist, `0`
otherwise. Outdated findings and tool-missing skips are informational
and don't drive the exit code. `--no-fail` always exits 0.

## Code layout

```
src/deps/
  mod.rs                 # DepsEvent enum, run(), text + JSON renderers
  duplicate_version.rs   # blame-aware duplicate detection
  git_dependency.rs      # git+ source scanning with ref parsing
  path_dependency.rs     # non-workspace path deps
  outdated.rs            # ccu --json shell-out
```

Future phases land as siblings (`advisory.rs`, etc.). The
cargo-metadata deserializer lives in `mod.rs` until it grows enough to
warrant its own file.

`src/cli.rs` carries the `Command::Deps { ... }` variant. Dispatch lands
in `src/main.rs` next to the other shared commands (not project-gated).

## v1 cut

Ship in this order, each as its own PR:

1. Scaffolding: `Command::Deps`, `DepsEvent` enum with `Summary` only,
   `--json` path, empty run that just emits a `Summary { findings: 0 }`.
   Proves the plumbing. **[shipped]**
2. `duplicate_version` phase. **[shipped]**
3. `git_dependency` + `path_dependency` phases. **[shipped]**
4. `outdated` phase via `ccu --json` shell-out. **[shipped]**

That's v1 done. Next: `advisory` via `cargo audit --json` - same
shell-out pattern, same graceful `ToolMissing` skip. No native network
code, no advisory-db cache management in brokkr itself.
