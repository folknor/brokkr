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
when foo 1.0 and foo 2.0 both ship, you want to know who picked each
and what's in your Cargo.toml that you can change to fix it.

Each `VersionPin` carries two fields:

- `picked_by` - direct parents of the pin (one reverse step through
  the resolve graph). Workspace members appear as `"<name> (direct)"`;
  other parents as `"name version"`. This answers "who landed on this
  version?".
- `via_workspace` - workspace-direct dep names that lead to the pin
  via chains whose immediate picker is transitive. Empty when every
  picker is already a workspace member or a workspace-direct dep -
  in that case `picked_by` already names what to bump. This answers
  "what's in my Cargo.toml that I can update?" without making the
  reader run `cargo tree -i`.

Two filters keep the blame honest:

- **Host-target.** Metadata for this phase is loaded with
  `--filter-platform=<host>` (host triple parsed from `rustc -vV`).
  Transitive crates that only exist for inactive targets (e.g.
  `wasmparser -> hashbrown 0.15.5` on a linux host) disappear from the
  graph entirely, matching what `cargo tree -i <crate> --target all`
  shows. Other phases keep the unfiltered metadata.
- **Normal kind.** Only `dep_kinds[*].kind == null` edges count. Dev
  and build deps are dropped, mirroring `cargo tree -d`'s default.

Text renderer:

```
[deps] 2 crates with multiple versions:
[deps]   hashbrown: 2 versions
[deps]     0.14.5  picked by dashmap 6.2.1
[deps]     0.17.1  picked by indexmap 2.14.0  [via calcard, mail-parser, reqwest]
[deps]   foo: 2 versions
[deps]     1.0.3  picked by old-lib 0.4
[deps]     2.1.0  picked by new-lib 2.0, our-crate (direct)
```

For the upward chain - "who is pulling in this crate?" - use the
positional focus form:

```
brokkr deps foo
brokkr deps hashbrown@0.17.1
brokkr deps mime              # substring fallback - matches mime, mime_guess, ...
```

Focus mode suppresses the other phases and emits, for each matched
version, a one-line metadata header followed by every distinct
Normal-kind chain from a workspace member down to it:

```
[deps] hashbrown 0.17.1  source=crates.io  manifest=~/.cargo/.../Cargo.toml  (6 chains)
[deps]   bifrost-graph 0.1.0 -> reqwest 0.13.3 -> h2 0.4.14 -> indexmap 2.14.0 -> hashbrown 0.17.1
[deps]   ...
```

`source` is the normalised origin: `crates.io`, `git+<url>#<sha>`,
`registry=<url>`, `workspace`, or `path`. `manifest` is the resolved
Cargo.toml with `$HOME` collapsed to `~`. The JSON output carries
the same two fields on each `ChainTrace`.

Resolution falls back in three steps:

1. **Exact match in host-filtered metadata.** The common case.
2. **Exact match in unfiltered metadata.** If the spec resolves only
   for an inactive target. Prints
   `<spec>: not in host-filtered graph; showing all-target chains`
   so the chain output isn't silently from a different target.
3. **Substring search across all package names** (case-insensitive).
   If neither exact lookup hits, brokkr greps for the needle and
   prints every match with a leading
   `no exact match for "<spec>"; showing N substring matches` note.
   Lets `brokkr deps mime` enumerate `mime` + `mime_guess` + ...
   without the user having to know the exact crate name. Only
   errors when there are zero substring matches.

False-positive risk: low. Blame is deterministic; substring fallback
is gated behind exact lookup failing.

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

### native_code [v1]

Lists dependencies that pull non-Rust code into the build. **Not**
name-based - the `-sys` suffix is advisory and both over- and under-
includes (`js-sys` / `windows-sys` are pure-Rust FFI declarations;
plenty of native bundlers carry no suffix). Two orthogonal signals,
each reported via a `reason` of `links` / `compiles` / `both`:

- **links a native library** - the manifest's `links` key is set. The
  canonical marker Cargo reserves for linking a system/bundled native
  library; what makes a real `-sys` crate. Carried in the `links`
  field of the event.
- **compiles non-Rust code** - the crate has a *build-dependency* on a
  known native-toolchain crate: `cc` (C/C++), `cmake`, `cxx-build`
  (C++), `nasm-rs` (assembly). Carried in the `toolchains` field.
  `cxx-build` pulls in `cc`, but a cxx user lists `cxx-build` as its
  own direct build-dep, so scanning direct build edges suffices.

Runs on the **host-filtered** metadata (like `duplicate_version`), so
wasm-only native bundlers (`sqlite-wasm-rs`) don't appear on a native
host. Workspace members are skipped - first-party native code is the
user's own choice, not a dependency smell.

Informational, like `outdated`/`stale`: native code is a portability /
cross-compile / supply-chain heads-up, not a defect, so it doesn't
drive the exit code.

```
[deps] 1 dependency with native code:
[deps]   libsqlite3-sys 0.38.1  compiles (cc) + links sqlite3
```

Limitation: a build script that shells out to a compiler directly
(not via `cc`/`cmake`/...) is invisible unless it also sets `links`.

### outdated [v1]

Shells out to `ccu --json` (the user's check-updates tool, pinned at
`schema_version=1`). Emits one `Outdated` event per non-up-to-date
direct dep, carrying name + installed + latest + severity
(`patch`/`minor`/`major`) + source_file + line_number.

These events are informational - they don't count toward the failure-
counting findings tally. A patch bump shouldn't fail your build; you
look at the report and decide.

The section is presented as an **exhaustive** answer about crates.io:
if a direct dep isn't here, it has no newer version available. The
text renderer phrases the header as
`N upgrade(s) available on crates.io; no other candidates:` and, when
ccu found nothing, prints `All direct deps are at latest on crates.io.`
- a colleague reading "duplicate hashbrown" then asking "should I bump
`reqwest`?" can resolve it from this section alone without running
`cargo search`. The completeness signal relies on a marker event
(`OutdatedComplete`) ccu emits after a successful parse so the
renderer can distinguish "0 upgrades, checked" from "didn't check".

If `ccu` is missing or fails for any reason (offline, schema mismatch,
crash), the phase emits a single `ToolMissing` event with a reason
string and skips. Doesn't fail the run, and doesn't print the
"all at latest" line - the `ToolMissing` event covers it.

### stale [v1]

Same `ccu --json` invocation as `outdated` (one network call, two
signals). Reads each check's `latest_released_at` and emits a `Stale`
event when the newest available version was published more than
~8 months ago. Crosses to severity `"abandoned"` past ~2 years.

The point of looking at the latest version's age (not the installed
version's) is to answer "is this project maintained?". A long gap on
the latest release is a hint that nobody's pushing fixes upstream
anymore - regardless of which version you happen to be on.

Informational, same as `outdated`. ISO-8601 date parsing handles
crates.io's verbatim format (`"2024-11-12T18:34:21.123456+00:00"`).

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
brokkr deps <pkg>                 # focus mode: metadata + chains for one package
brokkr deps hashbrown@0.17.1      # focus mode pinned to a specific version
brokkr deps mime                  # substring fallback if no exact match
brokkr deps --json                # NDJSON
brokkr deps --limit 50            # cap shown items per phase (default 20)
brokkr deps --all                 # no per-phase cap
brokkr deps --no-fail             # exit 0 even when findings exist
```

Exit code: `1` if any findings (duplicate / git / path) exist, `0`
otherwise. Outdated, stale, and tool-missing events are informational
and don't drive the exit code. `--no-fail` always exits 0.

## Code layout

```
src/deps/
  mod.rs                 # DepsEvent enum, run(), text + JSON renderers
  duplicate_version.rs   # blame-aware duplicate detection
  focus.rs               # `brokkr deps <pkg>` chain trace
  git_dependency.rs      # git+ source scanning with ref parsing
  path_dependency.rs     # non-workspace path deps
  native_code.rs         # links= + cc/cmake/... build-dep detection
  ccu.rs                 # ccu --json shell-out (outdated + stale)
```

Future phases land as siblings (`advisory.rs`, etc.). The
cargo-metadata deserializer lives in `mod.rs` until it grows enough to
warrant its own file.

`src/cli/schema.rs` carries the `Command::Deps { ... }` variant. Dispatch
lands in `src/main.rs` next to the other shared commands (not project-gated).

## v1 cut

Ship in this order, each as its own PR:

1. Scaffolding: `Command::Deps`, `DepsEvent` enum with `Summary` only,
   `--json` path, empty run that just emits a `Summary { findings: 0 }`.
   Proves the plumbing. **[shipped]**
2. `duplicate_version` phase. **[shipped]**
3. `git_dependency` + `path_dependency` phases. **[shipped]**
4. `outdated` + `stale` phases via `ccu --json` shell-out. **[shipped]**

That's v1 done. Next: `advisory` via `cargo audit --json` - same
shell-out pattern, same graceful `ToolMissing` skip. No native network
code, no advisory-db cache management in brokkr itself.
