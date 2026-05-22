# brokkr deps

Audits `Cargo.lock` (and `cargo metadata`) for dependency smells. Works in any
of brokkr's six projects - no `brokkr.toml` required, but if present its
`[deps]` block tunes allowlists.

Status legend: **[v1]** = first cut, **[planned]** = designed, not built,
**[idea]** = on the wishlist, may never ship.

## Goal

One opinionated command that bundles the checks we keep running by hand
(`cargo tree -d`, `cargo audit`, `cargo outdated`, eyeballing the lockfile)
into a single report scoped to *our* projects, with project-aware allowlists.
The target reader is "someone touching `Cargo.toml` who wants to know whether
they just made things worse."

Not a replacement for `cargo audit` / `cargo deny` - those have richer rule
languages and CI integrations. `brokkr deps` is the daily-driver triage view.

## Architecture

Mirrors `brokkr check`:

- Phase-based. Each smell is a phase that emits zero or more events.
- Structured event model in `src/deps/events.rs` (one `DepsEvent` enum,
  serde-tagged like `CheckEvent` in `src/cargo_json.rs`).
- Default output: prefixed text via `[deps]`, grouped by phase, capped by
  `--limit N` with a "+N hidden" trailer (reuse `src/scope.rs::format_trailer`).
- `--json` emits NDJSON: one event per line on stdout, no prefixed output.
  Every phase always emits a `*_summary` event at the end, even on zero
  findings, so consumers can tell "ran and clean" from "didn't run".
- Exit code: `0` if no severity>=warn findings, `1` otherwise. `--no-fail`
  to always exit 0 (useful for the report-only invocation in CI).

Adding a check later means: one new `DepsEvent` variant, one new function
that pushes those events, one new render arm. No changes to callers.

## Data sources

- **`Cargo.lock`** parsed via `cargo_lock` crate - gives every resolved
  package, source (`registry+...`, `git+...`, path), and the dependency graph.
- **`cargo metadata --format-version=1`** parsed via `cargo_metadata` crate -
  gives workspace members, direct vs transitive distinction, features.
- **Network (planned phases only)**: crates.io sparse index for yanked +
  newest version; RustSec advisory db (git clone, cached under
  `$XDG_CACHE_HOME/brokkr/advisory-db`).

Phases that don't need network run unconditionally; network phases are gated
behind `--online` (default off) so the offline path stays fast and
deterministic.

## Checks

### duplicate_version [v1]

Same crate resolved at >=2 versions. The point of this check is **assigning
blame**: when foo 1.0 and foo 2.0 both ship, you want to know which of your
direct deps is anchoring the old one so you can file an issue, upgrade, or
add an allowlist entry.

For each duplicated crate, emit one `DuplicateVersion` event with one
`VersionPin` per resolved version:

```jsonc
{
  "kind": "duplicate_version",
  "crate": "foo",
  "pins": [
    {
      "version": "1.0.3",
      "direct_blame": ["old-lib"],          // workspace direct deps anchoring this version
      "paths": [                            // every distinct chain from a workspace member
        ["our-crate", "old-lib 0.4", "foo 1.0.3"]
      ]
    },
    {
      "version": "2.1.0",
      "direct_blame": ["new-lib", "foo"],   // including foo itself if we depend directly
      "paths": [
        ["our-crate", "new-lib 2.0", "foo 2.1.0"],
        ["our-crate", "foo 2.1.0"]
      ]
    }
  ]
}
```

Computed by walking the `cargo_metadata` resolve graph: for each `(crate,
version)` node, find every parent chain back to a workspace member. Dedupe
chains by the first non-workspace hop ("direct dep") - that's the blame
anchor. Then collapse chains under each direct-blame entry.

Text renderer prints it like:

```
[deps] foo: 2 versions
  1.0.3  blamed on: old-lib 0.4
                    our-crate -> old-lib 0.4 -> foo 1.0.3
  2.1.0  blamed on: new-lib 2.0, our-crate (direct)
                    our-crate -> new-lib 2.0 -> foo 2.1.0
                    our-crate -> foo 2.1.0
```

The blame line is the headline. The paths underneath are for when the
blame line isn't enough (multi-hop transitive cases).

Allowlist: `deps.allow_duplicate = ["hashbrown", "syn"]` - crate names where
the duplication is known and intentional (proc-macro stacks, hashbrown
straddling editions, etc.). Allowlisted dupes still emit events at `info`
severity so the report stays honest.

False-positive risk: low. The blame computation is deterministic.

### git_dependency [v1]

Any package with `source = "git+..."`. Emits `GitDependency` with the repo
URL and pinned rev/branch/tag.

Allowlist: `deps.allow_git = ["litehtml-rs", "sluggrs"]` - our six sister
crates and any deliberate forks. Anything not on the list is `warn`.

### path_dependency [v1]

Any package with no source (local path dep). Emits `PathDependency` with the
resolved manifest path.

Allowlist: `deps.allow_path` - implicit allow for workspace members; warn for
anything else (a path dep outside the workspace is usually a dev accident).

### duplicate_purpose [v1]

Two crates with overlapping purpose in the resolved graph. Small curated
heuristic table in `src/deps/purpose.rs`:

```
async runtime:   tokio, async-std, smol
random:          rand, fastrand, nanorand, oorandom
http client:     reqwest, ureq, hyper-client, isahc
json:            serde_json, simd-json, json
regex:           regex, fancy-regex, onig, regress
hashmap:         (multiple hashbrown versions handled by duplicate_version)
tls:             rustls, native-tls, openssl
```

Emits one `DuplicatePurpose` event per overlapping pair, with the dep paths
that pull each.

Allowlist: `deps.allow_duplicate_purpose = [["serde_json", "simd-json"]]` -
pairs that are intentional.

False-positive risk: medium. The list is opinionated. Keep it short - if a
pair generates noise across projects, remove it.

### pre_release [v1]

Direct deps (workspace member -> X) where X resolves to 0.x.y. Informational
only (`info` severity, doesn't fail the run). Lots of the Rust ecosystem is
0.x and that's fine - the value is just *seeing the count* per project.

Allowlist: none; this never fails the run.

### advisory [planned, --online]

Shells out to `cargo audit --json`, adapts its findings into `Advisory`
events with the advisory id, severity, patched-in version, and the
RustSec-classified kind (`vulnerability` or `unmaintained`).

If `cargo audit` isn't installed: emit a single `ToolMissing` event with
the install hint (`cargo install cargo-audit`) and skip the phase. We do
not auto-install - this is a network-touching tool the user should
consent to.

Allowlist by advisory id: `deps.allow_advisory = ["RUSTSEC-2024-0001"]`.
Allowlisted findings still emit at `info`.

### outdated [planned, --online]

Shells out to `ccu --json` (from `~/Programs/check-updates`, user's own
tool - `--json` flag to be added there). Adapts each finding into an
`Outdated` event with current/latest/severity (patch/minor/major).

Defaults to `warn` on major gaps, `info` on minor/patch (otherwise the
report becomes noise).

If `ccu` isn't installed: same `ToolMissing` skip pattern as `advisory`.

### yanked [idea, --online]

`cargo audit` covers some yanks via advisories but not all. Could add a
direct sparse-index lookup phase later if we find it's missing real
problems. Not in v1.

### single_publisher [idea, --online]

Crates whose only publisher is a single user account with <N total crates,
or <M monthly downloads. Supply-chain hygiene signal. Risky to surface
because it implicates real people - keep it gated and `info`-only if it
ships.

### license [idea]

Compare each crate's license against a project-configured allowlist
(`deps.allow_license = ["MIT", "Apache-2.0", "BSD-*"]`). Off by default.

## `brokkr.toml` schema

All optional. Absent block = defaults.

```toml
[deps]
allow_duplicate         = ["hashbrown", "syn"]
allow_git               = ["litehtml-rs", "sluggrs"]
allow_path              = []
allow_duplicate_purpose = [["serde_json", "simd-json"]]
allow_advisory          = ["RUSTSEC-2024-0001"]
allow_license           = ["MIT", "Apache-2.0"]
```

Per-project: workspace members are always allowed as path deps without
needing to list them.

## CLI

```
brokkr deps                       # all offline phases, text output
brokkr deps --json                # NDJSON
brokkr deps --online              # add yanked/advisory/outdated (network)
brokkr deps --only duplicate_version,git_dependency
brokkr deps --limit 50            # cap shown items per phase (default 20)
brokkr deps --all                 # no cap
brokkr deps --no-fail             # exit 0 even with warnings
```

`--only` accepts comma-separated phase names matching the event variant in
snake_case. Unknown names error out with the valid set listed.

## Code layout

```
src/deps/
  mod.rs            # run(), phase orchestration, severity tallying
  events.rs         # DepsEvent enum + per-variant structs (serde)
  lockfile.rs       # cargo_lock parsing wrapper
  metadata.rs       # cargo_metadata wrapper, direct-vs-transitive
  phases/
    duplicate_version.rs
    git_dependency.rs
    path_dependency.rs
    duplicate_purpose.rs
    pre_release.rs
    yanked.rs        # planned
    advisory.rs      # planned
    outdated.rs      # planned
  purpose.rs        # curated heuristic table
  render.rs         # text renderer (the JSON path is just serde)
```

`src/cli.rs` gets one new `Command::Deps { ... }` variant. Dispatch lands
in `src/main.rs` next to the other shared commands (not project-gated).

## v1 cut

Ship in this order, each as its own PR:

1. Scaffolding: `Command::Deps`, `DepsEvent` enum with `Summary` only,
   `--json` path, empty run that just emits a `Summary { findings: 0 }`.
   Proves the plumbing.
2. `duplicate_version` phase.
3. `git_dependency` + `path_dependency` phases (share a Cargo.lock walk).
4. `duplicate_purpose` phase with the initial curated table.
5. `pre_release` phase.
6. `brokkr.toml` `[deps]` block + allowlist plumbing.

After v1 ships and we've used it for a week or two, add the `--online`
phases (`advisory` via `cargo audit`, `outdated` via `ccu`). Both are
shell-outs to existing tools - no native network code, no advisory-db
cache management, no sparse-index parsing in brokkr itself.
