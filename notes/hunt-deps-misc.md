# Design hunt: deps + standalone modules

Scope: `src/deps/`, `src/dependency_rules.rs`, `src/manifest.rs`, `src/lex.rs`,
`src/wc.rs`, `src/invalidate_cmd.rs`, `src/env.rs`, `src/git.rs`, plus
unclaimed top-level files I followed into (`src/pmtiles.rs`, `src/bench_cmd`'s
dirty check, `src/runnables.rs` only as a metadata consumer).

The overall verdict: the `deps` phases themselves are small and well reasoned,
but the subsystem has no model of Cargo. Every consumer of `cargo metadata`
and of `Cargo.toml` brings its own partial schema, its own dependency-kind
vocabulary, its own idea of which tables are dependency tables, and its own
host triple. And `git.rs` is one of three to five independent answers to "is
this tree dirty" and "what is this commit called".

---

## 1. Axes that should be types

### 1.1 Dependency kind is a string in four vocabularies

- `deps::PackageDep.kind` / `deps::DepKind.kind`: `Option<String>`, compared
  against `Some("dev")` / `Some("build")` / `is_none()` at each use
  (`duplicate_version.rs`, `focus.rs`, `native_code.rs`, `publish_cycle.rs`).
- `publish_cycle.rs`: a second, phase-local vocabulary of `&'static str`
  constants (`KIND_NORMAL`, `KIND_BUILD`, `KIND_DEV_VERSIONED = "dev"`) plus a
  `rank()` over those strings, then serialised into `CycleEdge.kind`.
- `dependency_rules.rs`: a real `DependencyKind` enum (Normal/Dev/Build/Unknown)
  with `From<Option<&str>>`, a separate `parse_config_kind` for config tokens,
  and its own private `CargoMetadata`.
- `[[dependency_rule]] kinds` arrives from config as `Vec<String>` and is only
  validated when the check phase runs (a typo is a runtime `DevError::Config`
  from `check`, not a config-load error).

`DependencyKind` already exists. It belongs in a shared cargo-model module,
deserialised directly by serde (`#[serde(rename_all="lowercase")]`, `null` ->
Normal), used by every phase, and parsed at config load for `kinds`. The
publish-cycle "dev, but versioned" refinement should be a separate type
(`PublishedEdge { Normal, Build, DevVersioned }`) built from
`(DependencyKind, req)`, not a string that happens to be `"dev"`.

### 1.2 Severity labels are strings ranked by string match

- `OutdatedEvent.severity: String` (ccu's `patch`/`minor`/`major`), sorted by
  `severity_rank` with a `_ => 3` catch-all - an unknown ccu value sorts last
  silently rather than being a schema failure.
- `StaleEvent.severity: &'static str`, minted by `ccu::classify_age` as
  `"stale"`/`"abandoned"` and ranked in `mod.rs::stale_rank` by matching the same
  literals again. Two sites own one enum's spelling; a typo in either sorts
  wrongly with no error.
- `NativeDependencyEvent.reason: &'static str` (`"links"`/`"compiles"`/`"both"`)
  is derivable from `links.is_some()` and `!toolchains.is_empty()` - it is a
  redundant field that can disagree with its siblings.

All three should be enums with `Serialize` + `Ord`; `reason` should be computed,
not stored.

### 1.3 Phase identity is a string, and the summary lies

`run()` hard-codes `phases_run` as a `vec![...]` of eight names, separately
from the calls that actually run the phases. The list is declared, not
observed: `outdated` and `stale` are listed even when ccu was missing and
emitted `ToolMissing`. `ToolMissingEvent.phase` is `PHASES[0]` = `"outdated"`
even though the event covers both phases. The `findings` count is likewise a
hand-maintained sum over five of the event vectors; the classification of a
phase as gating vs informational lives only in that addition expression and
the comment above it.

Shape it as a `Phase` enum with `fn gating(self) -> bool`, each phase returning
`PhaseOutcome { phase, events, ran: Ran | Skipped(reason) }`. `phases_run`,
`findings`, the exit code, `OutdatedComplete` (a marker event that exists only
because "ran with zero results" cannot be expressed), and `ToolMissing` all
become derived. A machine caller of `--json` could then branch on "skipped"
per phase instead of string-matching a `reason`.

### 1.4 Package identity: `id` / `name` / `"name version"` strings

Phases key maps by `&str` id, `&str` name, and a formatted `"{name} {version}"`
label, in parallel `HashMap`s rebuilt per phase (`id_to_label`, `id_to_name`,
`workspace_set` appear in `duplicate_version`, `focus`, `native_code`;
`workspace_set`/`member_ids` in every phase). `VersionPin.picked_by` mixes two
encodings in one `Vec<String>`: `"<name> (direct)"` for members and
`"name version"` for others - a JSON consumer must parse prose to tell them
apart. Introduce `PackageId` newtype and an indexed `Graph` (built once per
metadata load) exposing `is_member(id)`, `name(id)`, `label(id)`,
`normal_parents(id)`. `picked_by` should be `Vec<Picker { name, version:
Option, direct: bool }>`.

### 1.5 git dependency ref is three Options that are really one enum

`GitDependencyEvent { rev, branch, tag }` - `rev` is the resolved commit (from
the fragment) while branch/tag are the requested ref; the renderer then picks
"tag > branch > rev" with a match on the tuple. Requested ref is
`enum GitRef { Branch, Tag, Rev, DefaultBranch }`, and the resolved commit is a
separate field. Today a `?rev=X` request with a resolved fragment `Y` reports
`rev=Y` and loses `X` entirely (`fragment.or(query_rev)`) - minor, but the
type would have made it visible.

### 1.6 Commit identity is `String` everywhere

`git::CommitId { full: String, short: String }` has public string fields, and
`short_of` returns a non-hash input unchanged, so anything can be a "short
hash". `GitInfo.commit` is a short hash in a field named `commit`. Results rows
store a `String`. Sites that abbreviate independently of `SHORT_HASH_LEN`:

- `env.rs::read_git_rev` uses `git rev-parse --short HEAD` - exactly the
  growing-width abbreviation `git.rs`'s header says brokkr abolished. It also
  runs in the process cwd, not `project_root`.
- `invalidate_cmd.rs::preview` displays `chars().take(8)`.
- `ratatoskr_sync/bench_gate.rs` displays `take(8)`; `db/gate.rs` slices
  `&uuid[..8]` (a uuid, but the same "display width" decision).

A `Commit` newtype (full hash, validated hex length) with `short()` as the one
abbreviation, and `ShortCommit` as a distinct type for stored keys, removes the
choice. Similarly `RunUuid` for invalidate/results/sidecar: `invalidate` passes
full uuids into `delete_by_uuid_prefix`, a prefix API used as an exact delete.

### 1.7 Smaller primitives

- `[[manifest.version_align]] granularity` is a `String` validated inside
  `manifest::scan` (`"" | "major" | "minor"`), then defaulted again in
  `check_version_align` (`"" -> "minor"`), then re-compared in `version_key`
  (`== "major"`, else minor). Parse to `enum Granularity` at config load.
- `ManifestViolation.rule: &'static str` and `textlint`-style rule ids: an enum
  of manifest rules would give `--json` a closed set.
- `lex::Region::parse` is a config parser living in the lexer; the config layer
  should deserialize `Region` directly so a bad `region` is a load error.
- `DependencyRule { forbid: Option<Vec>, allow: Option<Vec> }` with "the parser
  enforces exactly one" - the check then matches `(None, None) => false`, a
  state that is supposedly unconstructible. Make it `polarity: Forbid(set) |
  Allow(set)`; `from = ["*"]` likewise should be `From::All | From::Named`.
- `invalidate`'s `(uuid_prefix, commit_prefix)` pair with a "clap guarantees
  one" fallback arm: an enum `Selector { Uuid(p), Commit(p) }` from the CLI.
- `env::DatasetStatus::HashError(String)` is prose; fine for display, but
  the hash comparison itself (below) wants a `Xxh128` type.

---

## 2. Decisions made in more than one place

### 2.1 "Is this tree dirty?" - at least five independent answers

| Site | Exempts |
|---|---|
| `git::check_clean` (measured runs) | `.brokkr/`, `*.md`, `brokkr.toml` + includes, `approved.png`, toolchain sidecar |
| `git::has_uncommitted` (run records) | `.brokkr/`, toolchain sidecar (with HEAD byte compare) |
| `scope::dirt` (check prose-only shortcut) | classifies markdown as prose |
| `bench_cmd::is_dirty` | nothing - raw `git status --porcelain` |
| `worktree::is_dirty` / `is_pristine` | worktree-specific |

They already disagree, and one is a bug: `bench_cmd::is_dirty` runs under the
lock, where `disable_toolchain` has moved `rust-toolchain.toml` aside. Its raw
`git status` sees that as a deletion plus an untracked file, so on every
`disable_toolchain` project an unnamed `brokkr bench` refuses on a spotless
checkout - exactly the failure `git::toolchain_exclusions` was written to
prevent for the harness. It also counts `.brokkr/bench/` itself unless
gitignored.

Single owner: `git.rs` should expose one `TreeState` computation (porcelain
entries, classified once - brokkr-owned, toolchain-moved, config, prose,
approved baseline, code) and named policies over it (`MeasuredRunPolicy`,
`RunRecordPolicy`, `ProseOnlyPolicy`, `BaselinePolicy`). The exemption list is
then a table, not five `git status` invocations with five pathspec sets.

### 2.2 How to load `cargo metadata` - nine-ish loaders, three schemas

Loaders: `deps::run_metadata` (+ `_locked`, + host-filtered), 
`dependency_rules::check`, `build.rs` (two calls, `serde_json::Value` walk),
`runnables.rs`, `bench_cmd/discover.rs`, `check_cmd/features.rs` and
`prepare.rs` (Value walks over `workspace_members`), `check_cmd/direct_runtime.rs`,
`check_cmd/nextest_lane.rs` (with `--all-features --filter-platform`).

Each decides independently: `--locked` or not (only `deps` passes it - a
`check` run's `dependency_rules`/`publication_cycles` does not, harmless under
`--no-deps` but nobody wrote that down at the call sites), `--no-deps` or not,
platform filtering, how to report failure (`DevError::Build` with stderr in
some, other variants elsewhere), and what "workspace member" means (id set
from `workspace_members` vs. `build.rs`'s id->name map vs. the
`workspace_default_members` comparison). `dependency_rules` and `deps` even
deserialize the same `packages[].dependencies` into two different private
structs (`CargoDependency` with `rename/target/optional`; `PackageDep` with
`req`).

Single owner: a `cargo_model` module (or adopt the `cargo_metadata` crate):
one typed schema, one `load(root, Query { deps: bool, platform: Option<Triple>,
locked: bool })`, one memoised snapshot per invocation keyed by query (check
already has a "one lazy `cargo metadata --no-deps` snapshot" in `features.rs`;
`dependency_rules` and `publication_cycles` each spawn their own on the same
run). `deps` additionally loads the resolving metadata twice (unfiltered +
host-filtered); the host-filtered graph is a subgraph and could be computed
from the full one plus the triple, but that is optimisation, not shape.

### 2.3 Host triple - three implementations

`deps::host_triple` (fails hard, runs `rustc -vV` in `.`), `rustflags::host_triple`
(memoised, `Option`), `bench_cmd/stamp.rs` (parses `host:` from its own
`rustc -vV`). Same question, three answers; `deps` should use the memoised one,
and there should be one `rustc -vV` parse (`RustcInfo { host, release,
commit_hash }`) that the stamp also reads. Note all of them run the rustc on
`PATH` from an arbitrary cwd, not the one rustup would pick for the project
(the toolchain file can differ) - so on a pinned project the triple and the
stamp's version can describe a different compiler than the build uses.

### 2.4 Which TOML tables are dependency tables

- `manifest::is_dependency_table_name`: hyphenated three only, recursing into
  any child table (including `[package.metadata.*]`, where a third-party tool
  table named `dependencies` would be linted as a real one), and
  `declared_deps` skips `workspace`.
- `deps/workspace_dep::collect_from`: hyphen *and* underscore spellings,
  recursing only into `target`.

They disagree today: `dev_dependencies` (still accepted by cargo, with a
deprecation warning) is recognised by `workspace_dep` but invisible to
`manifest`'s `sort-dependencies`, `cargo-machete-ignored` and `adapter-group`
checks - so an adapter-group crate can be depended on via an underscore table
without a violation. Also note `adapter-group` matches on the TOML key, while
`dependency_rules` deliberately judges by the real package name so a rename
cannot dodge it; `adapter-group` can be dodged with `alias = { package = ... }`.
One `ManifestDeps` walker (yields `(table_path, kind, target_cfg, key,
real_name, spec)`) should serve both, and `manifest.rs`'s check of
`[workspace.dependencies]` shape should use it too.

### 2.5 "Normal edge" for blame

`duplicate_version` and `focus` each build the same reverse Normal-kind
adjacency; `focus.rs`'s comment says it "matches the duplicate_version blame
filter so the chains line up". Two copies held in step by a comment. The
`Graph` from 1.4 owns `normal_parents`.

### 2.6 "Is this a workspace member" in `deps`

Every phase rebuilds `workspace_members` into a set and tests `pkg.id`; 
`path_dependency` additionally decides "path dep" as `source.is_none() &&
!member`. `git_dependency` decides "git" by `source.starts_with("git+")`. Source
kind is one classification (`Registry | Git(..) | Path | Member`) that should be
computed once on the package.

### 2.7 Hash comparison

`env::check_hash_status` (`eq_ignore_ascii_case`), `preflight::verify_file_hash`
(same), `tools.rs:495` (same) - agree today. An `Xxh128` newtype with
normalised lowercase storage and `PartialEq` is the owner; config parsing
should normalise at load.

### 2.8 Tracked-file walk

`gremlins::tracked_files` is the shared walk (header, textlint, manifest use it,
and `gremlins::read_in_scope` defines "unreadable is an error, deleted is a
skip"). `wc.rs` runs its own `git ls-files` with different semantics: it
silently skips unreadable files and non-UTF-8 paths. Harmless for a
convenience command, but it is the same question ("which files are the
project's") answered twice. Move the walk out of `gremlins` (see 3.4) and have
`wc` use it.

### 2.9 Disk free floor

`env::free_share` calls `disk_gate::is_low` / `MIN_FREE_PERCENT` - correctly a
single owner. Noted only as the good pattern; but `disk_gate` depends on
`env::probe_path` and `env::format_bytes`, while `env` depends on `disk_gate`
for the policy - a cycle between a policy module and a report module (3.2).

---

## 3. Structure

### 3.1 There is no cargo-model layer

The root structural problem behind 1.1, 1.4, 2.2, 2.4, 2.5. `deps/mod.rs` owns
`CargoMetadata`/`CargoPackage`/`ResolveNode` as `pub(crate)` but nothing outside
`deps` uses them; `dependency_rules`, `build.rs`, `runnables`, `bench_cmd`, and
four `check_cmd` modules each re-model the same JSON. Proposal: `src/cargo/`
with `metadata.rs` (typed schema + loader + snapshot cache), `graph.rs`
(indexed ids, membership, source kind, kind-filtered adjacency, host
filtering), `manifest_walk.rs` (TOML dependency-table walker), `rustc.rs`
(`rustc -vV` facts). Then:

- `dependency_rules.rs` is a `check` phase over the graph and should live in
  `check_cmd/` (or beside `deps` as a "policy" phase), not at top level.
- `deps::publication_cycles` + `PUBLISH_CYCLE_CAVEAT` + `publish_cycle_lines`
  are re-exported for `check`. That is a check phase reaching into a command's
  private module. With a cargo layer, `publish_cycle` is a pure analysis
  (`fn cycles(&Graph) -> Vec<Cycle>`) both commands call; the rendering of a
  `Cycle` belongs with the type.
- `manifest.rs` (a `check` phase) stays a `toml_edit` reader but shares the walker.

### 3.2 `env.rs` is five modules

It is a host-facts collector (kernel, governor, CPU, memory, io_uring,
memlock), a storage prober (mount table parsing, statvfs, block-device
model/rotational - used by `disk_gate`), a tool-version prober, a dataset
verifier, a user-config reporter, and the printer for all of them. `history_cmd`
calls `env::read_memory`; `disk_gate` calls `env::probe_path` and
`env::format_bytes`; `env` calls back into `disk_gate`. Split:

- `host/` facts (`memory`, `cpu`, `storage`) as libraries with typed results
  (`StorageInfo` already is one) - `disk_gate`, `history_cmd`, `preflight`, the
  sidecar and `bench_cmd::stamp` (which collects its own CPU facts) consume them.
- `datasets` status belongs with dataset resolution (`resolve`/`preflight`),
  since `env` is just one renderer of it.
- `env.rs` becomes the report: collect + print.
- `format_bytes` belongs in `output`.

`collect_tools` is a per-project `match` on `Project` listing which tool to
probe - a project fact stored outside the project. It should be a `Project`
method or a field of the project's descriptor, next to the visibility table.

### 3.3 `deps/mod.rs` mixes schema, orchestration and rendering

741 lines: event types, the cargo schema, the loaders, the phase list, the text
renderer with per-section noun pairs, and sort ranks. With 3.1 the schema and
loaders leave; with 1.3 orchestration becomes a phase table; the renderer
should be `render.rs` with a `Render` impl per event type rather than the
match-into-ten-vectors-then-render-each-section shape (which is the reason
`OutdatedComplete` exists). `UnusedWorkspaceDepEvent` is the one event type
defined in its phase module rather than `mod.rs` - pick one convention (events
next to their phase is better).

### 3.4 Shared file walk owned by `gremlins`

`gremlins::tracked_files` and `read_in_scope` are the project's file universe
and read policy for every text phase. They belong in their own module
(`scope.rs` already holds the git-status side of "which files"), with
`gremlins` as one consumer.

### 3.5 `git.rs` owns policy that is not git

`check_clean`'s exemption list (markdown, `brokkr.toml`, approved.png for
sluggrs/litehtml) is measurement policy and project knowledge;
`include_exclusions` re-composes the whole config (`config::compose`) from
inside a git helper, and `has_uncommitted` / `toolchain_exclusions` know
`toolchain`'s file-naming convention. `git.rs` should be mechanism (run git,
parse porcelain, resolve commits); the policies of 2.1 belong with their
consumers or in one `tree_state` module, and the toolchain module should
answer "which paths have I moved aside" itself. `git.rs` also has no module
header, unlike its neighbours.

### 3.6 `pmtiles.rs` is project code at top level, and a second parser

`src/pmtiles.rs` is a hand-written PMTiles v3 parser (`pmtiles-stats` for
elivagar/nidhogg, `sample_tile_coords` for nidhogg's tile bench). Elivagar
already links the elivagar crate for archive reading (`elivagar/eliv.rs`,
used by the corpus gate and regress). Two readers of one format: a header or
directory-decoding fix in one will not reach the other. It belongs under a
shared map-data module (or behind `eliv.rs`, if nidhogg can take the
dependency), not at top level.

### 3.7 `invalidate_cmd.rs` belongs to `db`

It is a query-plus-delete over `results.db` and `sidecar.db`, and it reaches
into both with three different selection APIs (`query_by_uuid`,
`uuids_matching_prefix`, `QueryFilter{commit}` with `limit: 100_000`,
`uuids_matching_commit_prefix`). Selection-by-prefix across both stores is a
`db` concern (`db::select_runs(Selector) -> Vec<RunUuid>`), and the command
becomes a thin preview/confirm shell. The `limit: 100_000` comment says "so we
don't silently truncate" - it still silently truncates at 100k; a delete
selection should be unlimited or refuse when the cap is hit.

### 3.8 `lex.rs`

Fine as a module; its only structural issue is `Region::parse` (1.7) and
that the `use`-statement reconstruction later in the file serves a different
consumer than region masking - two jobs, but both small. Leave it.

---

## Lateral findings (bugs, smells)

1. **Bug**: `bench_cmd::is_dirty` ignores the toolchain move-aside (2.1); an
   unnamed `brokkr bench` refuses on a clean `disable_toolchain` project.
2. **Bug-ish**: `env::read_git_rev` uses `--short` (variable width, contradicts
   `SHORT_HASH_LEN`) and runs in cwd rather than the project root.
3. `deps` summary reports `outdated`/`stale` as run when ccu was missing;
   `ToolMissing.phase` names only `outdated` (1.3).
4. `manifest` dependency-table detection misses underscore spellings and
   descends into `[package.metadata.*]` (2.4); `adapter-group` is judged by
   key, so a rename dodges it.
5. `invalidate` silently caps commit selections at 100k rows (3.7), and deletes
   full uuids through a prefix-delete API.
6. Host triple / rustc version are read from the `PATH` rustc in an arbitrary
   cwd, not the project's toolchain (2.3).
7. `dependency_rules` `kinds` typos surface only when `check` runs, not at
   config load (1.1).
8. `OutdatedEvent` sorts unknown ccu severities last silently instead of
   treating them as a schema violation, though the module pins
   `schema_version` for exactly that reason (1.2).
9. `git_dependency` drops the requested `?rev=` when a fragment is present (1.5).
