# pbfhogg project notes

`project = "pbfhogg"` in `brokkr.toml`.

## Module layout

- `src/pbfhogg/commands.rs` - `PbfhoggCommand` enum with `build_args()`,
  `build_hotpath_args()`, `result_command()`, `result_variant()`,
  `metadata()` - single source of truth for all pbfhogg command argument
  construction.
- `src/pbfhogg/dispatch.rs` - dispatch via `run_command_with_params()`. Routes
  through run/bench/hotpath/alloc based on command enum + mode. Uses
  `BenchContext` for build+harness.
- `src/pbfhogg/...` - benchmarks (read, write, merge, commands, extract,
  allocator, blob-filter, planetiler, all), verify (11 commands + all),
  download (Geofabrik region/OSC fetcher with auto-registration in
  `brokkr.toml`).

## Verify subcommands

Every verify subcommand that takes `--dataset` also accepts `--input <PATH>`
to skip dataset resolution and use a handcrafted fixture, and `--snapshot
<key>` to cross-validate a registered snapshot (e.g. an adversarial encoding
from `degrade`/`repack --as-snapshot`) instead of the primary data. `--input`
and `--snapshot` are mutually exclusive. `--snapshot` overrides only the PBF
input; the OSC (for `merge` / `derive-changes` / `diff` verifies) still
resolves from the dataset's primary chain, since degrade/repack snapshots
share base's sequence and carry no OSC table of their own. Resolution is
centralised in `resolve_verify_input` (`src/pbfhogg/cmd.rs`), which routes
through `resolve_snapshot_pbf_path` for both the base and named cases.

`verify_merge` parses the input OSC's delete set via `osc::parse_osc_file` and
runs a strict `pbfhogg diff --format osc` between pbfhogg's and osmium's
outputs - osmium-only IDs that appear in the input OSC's delete set are
exempt (osmium does version-based deletes; pbfhogg/osmosis/osmconvert delete
unconditionally), everything else fails.

## OSC parser (`src/osc.rs`)

Minimal `.osc` / `.osc.gz` reader. Returns `OscDiff` with sorted ID sets per
(`<create>` / `<modify>` / `<delete>`) section per element kind. Used by
`verify_merge` for the delete-set carve-out.

Hand-rolled tag-start scanner; tolerant of XML comments, processing
instructions, self-closing elements, and single-quoted attributes. Element
bodies (tags / refs / members / coords / metadata) are deliberately skipped -
only IDs are needed.

## Snapshots and variant selection

pbfhogg accepts these flags on every measurable command. For the
schema-universal flags (`--variant`, `--osc-seq`, `--tiles`) see
`docs/brokkr.toml.md`. The pbfhogg-only flags are:

- `--snapshot <key>` - selects which snapshot's `pbf`/`osc` tables the
  resolver reads from. `base` (or omitting the flag) reads from the legacy
  top-level data. Any other key reads from
  `[..datasets.<dataset>.snapshot.<key>]`. Accepted by every measurable
  pbfhogg command - the resolver in `build_pbfhogg_context`
  (`src/pbfhogg/dispatch.rs`) calls `resolve_snapshot_pbf_path` regardless of
  which command is dispatching, so wiring a new pbfhogg command into the
  snapshot graph is just a matter of adding the field to its CLI variant in
  `src/cli.rs` and propagating it to `CommandParams.snapshot` in
  `src/pbfhogg/cli_adapter.rs`. The `read` throughput benchmark and the
  `verify` cross-validation suite accept `--snapshot` too (they resolve
  outside `build_pbfhogg_context` but share `resolve_snapshot_pbf_path` via
  `SnapshotRef::from_opt`); the synthetic `write`/`merge-bench`/etc. benches
  do not.
- `--as-snapshot <key>` / `--replace-snapshot` - (`repack` and `degrade` only)
  promote the final iteration's scratch artifact into the dataset graph as a
  new snapshot. `--replace-snapshot` allows overwriting an existing key;
  without it, an existing key errors out. Both flags are validated up-front
  via `download::preflight_snapshot_collision` (called from the top of
  `run_command_with_params`), so a forgotten `--replace-snapshot` errors
  before the cargo build kicks off, not after the run.

## I/O and compression flags

pbfhogg-specific flags that adjust cargo features and binary args:

- `--direct-io` - enable O_DIRECT I/O. Adds `linux-direct-io` cargo feature,
  `--direct-io` binary flag, `+direct-io` variant suffix.
- `--io-uring` - enable io_uring I/O. Adds `linux-io-uring` cargo feature,
  `--io-uring` binary flag, `+uring` variant suffix. Runs io_uring preflight
  checks before building. Only supported by `apply-changes`, `sort`,
  `cat-dedupe`, `diff-osc`, `repack`, and `degrade`; brokkr rejects it for
  other commands before building.
- `--compression <spec>` - output compression passed through to the binary.
  Values: `zlib:N` (1-9), `zstd:N`, `none`. Adds
  `+zstd1`/`+zlib6`/`+nocompress` variant suffix. No cargo features required.
- `--locations-on-ways` - (`apply-changes` only) passes through to the child
  pbfhogg invocation.

## download command

`download <region> [--osc-seq N]` - download PBF + OSC from Geofabrik.
Accepts short aliases (`denmark`, `europe`) or full Geofabrik paths
(`europe/france`, `asia/japan/kanto`). Dataset key is the last path component.
Checks configured filenames in `brokkr.toml` before downloading. `--osc-seq N`
downloads all missing diffs from `last_configured_seq + 1` through N. After
downloading, computes xxh128 hashes and appends new entries to `brokkr.toml`.
Filenames follow project convention: `{key}-{YYYYMMDD}-seq{N}.osc.gz`,
`{key}-{YYYYMMDD}.osm.pbf`.
