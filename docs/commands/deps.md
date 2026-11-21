# brokkr deps

`brokkr deps` audits `Cargo.lock`, through `cargo metadata`, for dependency
smells. It works in any Rust and git repo, with or without a `brokkr.toml`.
This page covers what the command is for, its usage, its output and data
source, focus mode, and then the reference for each check.

The command bundles the checks you would otherwise run by hand (`cargo tree
-d`, `cargo audit`, `cargo outdated`, reading the lockfile) into one report.
It is written for someone touching `Cargo.toml` who wants to know whether
they just made things worse. It doesn't replace `cargo audit` or `cargo
deny`, which have richer rule languages and CI integrations: `brokkr deps`
is the daily triage view.

## Usage

These are the forms the command takes:

```sh
brokkr deps                     # all checks, terse text output
brokkr deps PKG                 # focus mode: metadata and chains for PKG
brokkr deps hashbrown@0.17.1    # focus mode pinned to one version
brokkr deps mime                # substring fallback if no exact match
brokkr deps --json              # NDJSON
brokkr deps --no-fail           # exit 0 even when findings exist
```

The exit code is `1` if any finding exists and `0` otherwise. Findings are a
duplicate version, a git or path dependency, a publish cycle, or an unused
workspace dependency. Outdated, stale, native-code and tool-missing events
are informational and don't drive the exit code. `--no-fail` always exits 0,
for report-only CI runs.

## Output

The audit runs like `brokkr check`: each smell is a phase that emits zero or
more events.

- **Text**: the default, `[deps]`-prefixed lines grouped by phase. Every
  finding prints, with no cap and no flag to set one.
- **JSON**: `--json` emits NDJSON on stdout, one type-tagged event per line.

A successful full audit ends with a `summary` event; focus mode emits chain
records instead. The summary has these fields:

- `phases_run`: the phases that completed
- `findings`: the count from the phases that drive failure
- `phase_outcomes`: every phase as `ran` or `skipped`, with a `reason` on a
  skipped phase, for example
  `{"phase":"stale","status":"skipped","reason":"..."}`

A shared-tool failure reports every phase that relies on `ccu` as skipped
(see outdated).

## Data source

The full audit loads unfiltered metadata and a host-filtered snapshot, using
the host triple from `rustc -vV`. Focus mode starts with the host-filtered
graph and loads unfiltered metadata only when it needs a fallback. brokkr
reads a minimal subset of each metadata load: packages, workspace
membership, declared dependencies and resolve edges. Network checks shell
out to existing tools (`ccu`); `deps` has no network code of its own.

brokkr runs `cargo metadata` with `--locked`, which is what makes the
command audit `Cargo.lock`. Without it, cargo re-resolves whenever the
lockfile is missing or behind a manifest, rewriting `Cargo.lock` and
querying the registry index, so the report would describe a graph that
exists only in that run. With it, a missing or out-of-date lockfile is an
error: bring it current with a build first, since `deps` never generates
one.

brokkr doesn't pass `--offline`. Metadata reads every dependency's own
manifest, so cargo still downloads a crate whose source isn't in the local
cache yet. Cargo doesn't consult the index once the lock is complete.

## Focus mode

Focus mode answers "who is pulling in this crate?" with the upward chain.
Give a crate name, optionally pinned to a version:

```sh
brokkr deps foo
brokkr deps hashbrown@0.17.1
brokkr deps mime              # substring fallback: mime, mime_guess, ...
```

Focus mode suppresses the other phases. For each matched version it emits a
one-line metadata header, then every distinct Normal-kind chain from a
workspace member down to it:

```text
[deps] hashbrown 0.17.1  source=crates.io
         manifest=~/.cargo/.../Cargo.toml  (6 chains)
[deps]   bifrost-graph 0.1.0 -> reqwest 0.13.3 -> h2 0.4.14
           -> indexmap 2.14.0 -> hashbrown 0.17.1
[deps]   ...
```

The header (wrapped here for width) has these fields:

- **`source`**: the normalised origin: `crates.io`, `git+URL#SHA`,
  `registry=URL` (an alternate registry, git-index `registry+` and
  sparse-index `sparse+` alike), `workspace` or `path`.
- **`manifest`**: the resolved `Cargo.toml`, with `$HOME` collapsed to `~`.

The JSON output has the same `source` and `manifest` on each chain record.
An optional `prefix_note` keeps the explanation when a record came from an
all-target or substring fallback.

Resolution falls back in three steps:

1. **Exact match in host-filtered metadata**, the common case.
2. **Exact match in unfiltered metadata**, when the spec resolves only for
   an inactive target. brokkr prints `SPEC: not in host-filtered graph;
   showing all-target chains`, so you know the chains come from another
   target.
3. **Substring search across all package names**, case-insensitive, when
   neither exact lookup matches. brokkr prints every match after a `no
   exact match for "SPEC"; showing N substring matches` note, so `brokkr
   deps mime` lists `mime`, `mime_guess` and the rest without your knowing
   the exact name. It errors only when nothing matches.

The false-positive risk is low: blame is deterministic, and the substring
fallback runs only when exact lookup fails.

## Checks

Each check below is one phase of the full audit. The ones that count as
findings drive the exit code; the informational ones don't.

### duplicate_version

`duplicate_version` reports a crate resolved at two or more versions, and
assigns blame. When foo 1.0 and foo 2.0 both ship, you want to know who
picked each and what in your `Cargo.toml` you can change to fix it. Each
version pin has two fields:

- **`picked_by`**: the pin's direct parents, one reverse step through the
  resolve graph. In JSON each picker has `name`, `direct` and an optional
  `version`; a workspace member has `direct: true` and no version. The text
  renderer keeps the compact `NAME (direct)` and `NAME VERSION` labels. This
  answers "who landed on this version?" without JSON consumers parsing
  labels.
- **`via_workspace`**: the workspace-direct dependencies that lead to the
  pin through chains whose immediate picker is transitive. It is empty when
  every picker is already a workspace member or a workspace-direct
  dependency, since `picked_by` then names what to bump. This answers "what
  in my `Cargo.toml` can I update?" without your running `cargo tree -i`.

Two filters keep the blame honest:

- **Host target.** brokkr loads this phase's metadata with
  `--filter-platform=HOST`, the host triple from `rustc -vV`. Transitive
  crates that exist only for inactive targets (`wasmparser -> hashbrown
  0.15.5` on a linux host) disappear from the graph, matching what `cargo
  tree -i CRATE --target all` shows. Other phases keep the unfiltered
  metadata.
- **Normal kind.** Only `dep_kinds[*].kind == null` edges count. brokkr
  drops dev and build dependencies, as `cargo tree -d` does by default.

The text renderer prints each crate's versions and pickers:

```text
[deps] 2 crates with multiple versions:
[deps]   hashbrown: 2 versions
[deps]     0.14.5  picked by dashmap 6.2.1
[deps]     0.17.1  picked by indexmap 2.14.0  [via calcard, reqwest]
[deps]   foo: 2 versions
[deps]     1.0.3  picked by old-lib 0.4
[deps]     2.1.0  picked by new-lib 2.0, our-crate (direct)
```

For the upward chain to one crate, use focus mode.

### git_dependency

`git_dependency` reports any package with `source = "git+..."`. The event
has the repo URL, the requested ref (branch, tag, rev, or the default
branch) and the resolved commit SHA from the source fragment. JSON keeps the
request in `requested_ref` and the lockfile pin in `resolved_rev`: a
manifest's `?rev=X#Y` is reported as `requested_ref:
{"kind":"rev","value":"X"}` and `resolved_rev: "Y"`. The event also keeps
flat `rev`, `branch` and `tag` fields for NDJSON consumers. `rev` holds the
resolved fragment, falling back to a requested `?rev=` when Cargo provides
no fragment.

### path_dependency

`path_dependency` reports any package with no source that isn't a workspace
member, with its resolved manifest path. Path-linking inside a workspace is
the point of a workspace. A path dependency outside it is usually a dev
shortcut, such as an unpublished fork, or a hand-patched dependency that
wouldn't reproduce on a clean checkout.

### publish_cycle

`publish_cycle` reports a dependency cycle among publishable workspace
members. `cargo build` resolves the whole workspace at once and accepts one.
`cargo publish` uploads one crate at a time and needs every dependency in
the published manifest to exist on the registry already, so a cycle has no
valid publication order. The failure appears at release time, long after the
change that introduced it merged, which is why it is worth a phase.

The phase reads the declared manifests (`packages[].dependencies`) rather
than the resolve graph, because publication depends on what the manifest
says, not on what the resolver picked. For the same reason it uses the
unfiltered metadata: a cycle closed by a target-specific dependency blocks
the release on every host.

Two exclusions follow from how publication works:

- **Version-less dev-dependencies don't count.** Cargo strips a dev-dep
  with no version requirement (the `{ path = "..." }`-only form) from the
  published manifest, so workspace test helpers may point back at their own
  dependents. Such an edge exists locally and vanishes on publish.
- **`publish = false` members are excluded.** They are never uploaded, so no
  edge through them can block a release.

A dev-dep that also names a version is published and does close the cycle.
Deleting its `version` key usually fixes the cycle without restructuring
anything, so the renderer names that edge, with a caveat: the fix is a fact
about `cargo publish`. A project that releases through its own planner may
order the graph by local `path =` edges whatever their kind
(nautilus_trader's `scripts/ci/publish-cargo-crates.sh` does, on purpose).
Such a planner still sees a path-only dev-dep, so the hint carries the
qualifier rather than promise a fix that leaves some gates red. Detection is
unaffected; only the advice depends on the consumer.

The phase emits a publish cycle event with two fields, and counts as a
finding:

- **`members`**: the loop, rotated to start at its lexicographically
  smallest member, so one cycle found from different roots dedups to one
  event.
- **`edges`**: one per member, closing back to `members[0]`, each tagged
  `normal`, `build` or `dev`.

The phase prints nothing when there is no cycle, like every other section,
rather than an all-clear line.

The list may be partial, and says so. The walk reports one cycle per back
edge and prunes on a visited set, so it never examines a back edge into an
already-finished member, and several loops through one member surface one
at a time. Every report with a cycle ends with a caveat line telling you to
re-run after each fix.

The limitation doesn't affect gating, since any cycle fails the run. It
matters when you use the output for scoping: choosing between "delete this
one edge" and "restructure the crate" depends on whether removing the named
edge leaves anything else entangled. Re-run after each fix until the report
is empty: that loop is sound even though one report isn't complete.

`brokkr check` runs the same check as its `publish_cycle` convention phase,
with the same renderer, so the two commands can't disagree about a tree.
Use `deps` to investigate a cycle; `check` stops one from merging. See
`brokkr man check publish_cycle`.

### workspace_dep

`workspace_dep` reports a `[workspace.dependencies]` entry in the root
manifest that no member inherits with `dep = { workspace = true }`. Such an
entry is dead weight, and counts as a finding.

`cargo metadata` doesn't surface workspace inheritance, so this phase reads
the manifests directly: the root for the declarations, and each member,
through its metadata `manifest_path`, for its `workspace = true` uses.
Metadata still supplies the workspace root and membership. The phase parses
manifests as the `[manifest]` checks do, preserving Cargo dependency-table
paths, package and target kinds, target cfgs, aliases and inherited package
names. It ignores similarly named dependency tables under metadata, which
Cargo doesn't interpret as dependency declarations.

The `[deps] workspace_dep_ignore` list exempts names that are legitimately
unreferenced, such as dev tools or top-level members. An entry ending in `*`
is a prefix match (`cargo-*`). Every ignore entry must match a declared
workspace dependency; brokkr refuses a stale entry.

A root or member manifest that can't be read or parsed fails the run rather
than being skipped. A skipped root would read as an all-clear, and a skipped
member would lose its inherited dependencies and report them as unused.

### native_code

`native_code` lists dependencies that pull non-Rust code into the build. The
`-sys` suffix can't identify native code reliably: `js-sys` and
`windows-sys` are pure-Rust FFI declarations, while some native bundlers
have no suffix. The event's `reason` records which signals apply:

- **links a native library**: the manifest's `links` key is set, the marker
  Cargo reserves for linking a system or bundled native library and what
  makes a real `-sys` crate. It is in the event's `links` field.
- **compiles non-Rust code**: the crate has a build-dependency on a known
  native-toolchain crate: `cc` (C and C++), `cmake`, `cxx-build` (C++) or
  `nasm-rs` (assembly). It is in the `toolchains` field. `cxx-build` pulls
  in `cc`, but a cxx user lists `cxx-build` as its own direct build-dep, so
  scanning direct build edges suffices.

brokkr computes `reason` from `links` and `toolchains` and keeps it in the
JSON event for consumers.

The phase runs on the host-filtered metadata, like `duplicate_version`, so
wasm-only native bundlers (`sqlite-wasm-rs`) don't appear on a native host.
It skips workspace members, since first-party native code is your own
choice, not a dependency smell.

The phase is informational, like `outdated` and `stale`. Native code is a
portability, cross-compile and supply-chain heads-up, not a defect, so it
doesn't drive the exit code:

```text
[deps] 1 dependency with native code:
[deps]   libsqlite3-sys 0.38.1  compiles (cc) + links sqlite3
```

A build script that runs a compiler directly, not through `cc`, `cmake` or
the others, is invisible unless it also sets `links`.

### outdated

`outdated` shells out to `ccu --json`, the check-updates tool, pinned at
`schema_version=1`. If `ccu` is missing, install it and put it on `PATH`.
The phase emits one outdated event per direct dependency that isn't up to
date, with its name, installed and latest versions, severity (`patch`,
`minor` or `major`), source file and line number.

Severity is a closed set: an unrecognized value from `ccu` makes the
response unparsable, and brokkr reports the `ccu` phases as skipped.

The events are informational and don't count as findings. A patch bump
shouldn't fail your build; you read the report and decide.

The section is an exhaustive answer about crates.io: a direct dependency
that isn't listed has no newer version. The text header reads `N upgrades
available on crates.io; no other candidates:` (`1 upgrade` for one). When
`ccu` found nothing, brokkr prints `All direct deps are at latest on
crates.io.` Someone who reads "duplicate hashbrown" and asks "should I bump
`reqwest`?" can answer it from this section alone, without `cargo search`.
The summary's `phase_outcomes` tells "0 upgrades, checked" from "didn't
check". brokkr also emits an outdated-complete marker after the `ccu`
events for NDJSON consumers.

brokkr runs `ccu` under a wall-clock ceiling and kills it past that, so a
stalled registry connection can't hang `deps`. If `ccu` is missing or fails
for any reason (offline, timed out, schema mismatch, crash), brokkr emits
one tool-missing event and reports the `ccu` phases as skipped. The event
keeps its `phase: "outdated"` field, and its `phases` array lists every
affected phase. The summary gives each skipped phase the same reason. A
skip doesn't fail the run or print the "all at latest" line.

### stale

`stale` uses the same `ccu --json` response as `outdated`. It emits a stale
event when a dependency's `latest_released_at` is older than a built-in age
threshold, with severity `"abandoned"` past a higher built-in threshold.
You can't configure the thresholds. The closed severity values are
serialized as strings.

The phase looks at the latest version's age, not the installed version's,
to answer "is this project maintained?". A long gap since the latest release
hints that nobody is pushing fixes upstream, whichever version you are on.

It is informational, like `outdated`. Date parsing handles crates.io's
verbatim ISO 8601 format (`"2024-11-12T18:34:21.123456+00:00"`).
