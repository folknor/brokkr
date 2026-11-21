# deps internals

This page records how `brokkr deps` is built: the phase model, the metadata
graph, the ccu shell-out and the publish-cycle walk. How the command is used
is in `docs/commands/deps.md`.

## Code layout

`src/deps/` holds one file per phase beside the shared model:

- **`mod.rs`**: the `DepsEvent` enum, serialized as type-tagged events, and
  `run()`
- **`phase.rs`**: the closed `Phase` enum, its gating classification and
  `PhaseOutcome`
- **`render.rs`**: the `Render` trait the text renderer uses per event
- **`duplicate_version.rs`**: blame-aware duplicate detection
- **`focus.rs`**: the `brokkr deps PKG` chain trace
- **`git_dependency.rs`**: `git+` source scanning with ref parsing
- **`path_dependency.rs`**: non-workspace path dependencies
- **`publish_cycle.rs`**: publication cycles among members
- **`workspace_dep.rs`**: unused `[workspace.dependencies]` entries
- **`native_code.rs`**: `links` and native build-dependency detection
- **`ccu.rs`**: the `ccu --json` shell-out behind `outdated` and `stale`

`Command::Deps` is in `src/cli/schema.rs` and is dispatched in
`src/main_parts/dispatch.rs` with the other shared, ungated commands.

## Phase model

The audit mirrors `brokkr check`: each phase returns a `PhaseOutcome`, ran
or skipped with a reason, holding its events. `phases_run`, `phase_outcomes`
and the finding count are all derived from the outcomes, and
`Phase::is_gating` alone decides which phases drive the exit code.

To add a phase:

1. Add its `Phase` variant and its name, and classify it in `is_gating`.
2. Define its event type in the phase's module and wrap it in a
   `DepsEvent` variant.
3. Return a `PhaseOutcome` from the phase.
4. Implement `Render` for its event.

A phase backed by an external tool must skip with a reason when the tool is
missing or fails, never error the run, as the ccu phases do.

## Metadata graph

`src/cargo/metadata.rs` loads `cargo metadata` and indexes it into one
`Graph` per load. The graph owns `PackageId` lookups, source classification,
workspace membership and reverse Normal-kind adjacency. brokkr deserializes
only the subset deps needs (packages, membership, declared dependencies and
resolve edges), with no extra crate dependency.

Host-filtered views are projected from the same resolving snapshot using
its target selectors. A cfg selector the host evaluator can't decide is
kept, so filtering errs toward showing an edge. `src/cargo/rustc.rs` owns the
cached host, release and commit parse from the rustc selected in the project
directory.

`workspace_dep` reads manifests with the `toml_edit`-based `ManifestDeps`
walker in `src/manifest.rs`, the same one the `[manifest]` convention checks
use, because `cargo metadata` doesn't surface workspace inheritance.

## ccu shell-out

`src/deps/ccu.rs` runs `ccu --json` pinned at `schema_version=1`, killed
past `CCU_DEADLINE`. The `stale` thresholds are the `STALE_DAYS` and
`ABANDONED_DAYS` constants there; they are policy, not project config. One
response feeds both `outdated` and `stale`, and a failure skips both with
one `ToolMissing` event whose `phases` array lists them. deps has no network
code of its own: a network check shells out to an existing tool.

The `OutdatedComplete` marker and the flat `rev`, `branch` and `tag` fields
on a git-dependency event stay for NDJSON consumers that read them.

## Publish-cycle walk

The walk reports one cycle per back edge and prunes on a visited set, so a
back edge into an already-finished member is never examined and the list
can be partial. That is a deliberate trade. Exhaustive enumeration means
dropping the pruning, with an exponential worst case, or implementing
Johnson's algorithm. The limitation costs nothing when the output is a
gate, since any cycle fails; for scoping, the caveat line and re-running
after each fix make the loop sound.

`brokkr check`'s `publish_cycle` phase calls the same function and renderer,
so the two commands can't disagree about a tree. Keep it one function.

`scripts/publish-cycle-fixture.py` writes a throwaway workspace in both
shapes for manual end-to-end checks.
