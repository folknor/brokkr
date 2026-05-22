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
graph). No extra crate dependencies. Planned `--online` phases shell out
to existing tools (`cargo audit`, `ccu`) - no native network code in
brokkr.

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

False-positive risk: medium. The list is opinionated. Keep it short - if a
pair generates noise across projects, remove it.

### advisory [planned, --online]

Shells out to `cargo audit --json`, adapts its findings into `Advisory`
events with the advisory id, severity, patched-in version, and the
RustSec-classified kind (`vulnerability` or `unmaintained`).

If `cargo audit` isn't installed: emit a single `ToolMissing` event with
the install hint (`cargo install cargo-audit`) and skip the phase. We do
not auto-install - this is a network-touching tool the user should
consent to.

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

## CLI

```
brokkr deps                       # offline phases, terse text output
brokkr deps --chains              # add per-pin example chains under blame lines
brokkr deps --json                # NDJSON
brokkr deps --limit 50            # cap shown items per phase (default 20)
brokkr deps --all                 # no cap; implies --chains
brokkr deps --no-fail             # exit 0 even when findings exist
```

Exit code: `1` if any findings exist, `0` otherwise. `--no-fail` always
exits 0. (Planned `--online` and `--only` flags will come with the
network phases.)

## Code layout

```
src/deps/
  mod.rs                 # DepsEvent enum, run(), text + JSON renderers
  duplicate_version.rs   # blame-aware duplicate detection
  git_dependency.rs      # git+ source scanning with ref parsing
  path_dependency.rs     # non-workspace path deps
```

Future phases land as siblings (`duplicate_purpose.rs`, `advisory.rs`,
etc.). The cargo-metadata deserializer lives in `mod.rs` until it grows
enough to warrant its own file.

`src/cli.rs` carries the `Command::Deps { ... }` variant. Dispatch lands
in `src/main.rs` next to the other shared commands (not project-gated).

## v1 cut

Ship in this order, each as its own PR:

1. Scaffolding: `Command::Deps`, `DepsEvent` enum with `Summary` only,
   `--json` path, empty run that just emits a `Summary { findings: 0 }`.
   Proves the plumbing. **[shipped]**
2. `duplicate_version` phase. **[shipped]**
3. `git_dependency` + `path_dependency` phases. **[shipped]**
4. `duplicate_purpose` phase with the initial curated table.

After v1 ships and we've used it for a week or two, add the `--online`
phases (`advisory` via `cargo audit`, `outdated` via `ccu`). Both are
shell-outs to existing tools - no native network code, no advisory-db
cache management, no sparse-index parsing in brokkr itself.
