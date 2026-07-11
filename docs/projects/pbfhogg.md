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

**Output model.** Every verify check runs "quiet on pass, loud on fail" via
`verify::run_check`: by default a check's detail (`verify_msg` output - section
headers, inspect dumps, element diffs) is captured into a buffer and only
replayed if the check fails; a passing check prints just a one-line
`<name>: PASS (<ms>ms)` summary. `-v`/`--verbose` skips the buffer so detail
streams live even on pass. `verify all` applies this per check, so its default
output is one line per check plus a final tally, and only failing checks spew.
The buffer lives in `output.rs` (`verify_buffer_begin/flush/discard`, fed by
`verify_msg`); one-line results use `verify_summary`, which bypasses it. On
failure `run_check` returns `DevError::ExitCode(1)` so `main` exits non-zero
without re-printing an error it already reported.

Every verify subcommand that takes `--dataset` also accepts `--input <PATH>`
to skip dataset resolution and use a handcrafted fixture, and `--snapshot
<key>` to cross-validate a registered snapshot (e.g. an adversarial encoding
from `degrade`/`repack --as-snapshot`) instead of the primary data. `--input`
and `--snapshot` are mutually exclusive. PBF resolution is centralised in
`resolve_verify_input` (`src/pbfhogg/cmd.rs`), which routes through
`resolve_snapshot_pbf_path` for both the base and named cases.

OSC resolution for the change-consuming verifies (`merge` / `derive-changes`
/ `diff`) is snapshot-aware via `resolve_verify_osc`: a named snapshot that
carries its own `osc` table (a point-in-time snapshot) is diffed against
*that* chain; an encoding-only snapshot (degrade/repack has no OSC table)
falls back to the dataset's primary chain - the logically-correct diff stream,
since such snapshots are same-sequence re-encodings of the base PBF. Which
chain resolved is narrated to stderr (`[verify] osc: snapshot-scoped (<key>)`
vs `[verify] osc: base fallback (<key> has no osc table)`), but only when
`--snapshot` was actually passed. Even a deliberately misaligned OSC is still
a valid tool-vs-tool check - both pbfhogg and osmium apply the same changes to
the same input - so the fallback never invalidates the cross-check.

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

pbfhogg-specific flags that adjust cargo features and binary args. Note on
result rows: post-v13 the `mode` column (formerly `variant`) carries only the
measurement mode (`bench`/`hotpath`/`alloc`). These axis flags are **not**
folded into a variant suffix anymore - they live in the `cli_args` /
`brokkr_args` columns and are what the `brokkr results` table's `args` column
renders and what `brokkr results --compare` keys its pairs on. So a flag-on run
and its flag-off baseline at the same commit are distinct rows / distinct
compare pairs automatically, no suffix required.

- `--direct-io` - enable O_DIRECT I/O. Adds `linux-direct-io` cargo feature,
  `--direct-io` binary flag.
- `--io-uring` - enable io_uring I/O. Adds `linux-io-uring` cargo feature,
  `--io-uring` binary flag. Runs io_uring preflight checks before building.
  Only supported by `apply-changes`, `sort`, `cat-dedupe`, `diff-osc`,
  `repack`, and `degrade`; brokkr rejects it for other commands before
  building.
- `--compression <spec>` - output compression passed through to the binary.
  Values: `zlib:N` (1-9), `zstd:N`, `none`. No cargo features required.
- `--inject-prepass` - (`add-locations-to-ways` only) emit the injected-prepass
  wire extensions (BlobHeader field 5 way-member bitmaps, Way field 20
  shared-node pins; declared via the `pbfhogg.WayMembers-v1` /
  `pbfhogg.SharedNodePins-v1` header feature strings). Forwarded verbatim to
  the pbfhogg child (`src/pbfhogg/commands.rs`, `AddLocationsToWays` arm), no
  cargo features. Composes with `--index-type sparse|external|auto`,
  `--bench`/`--hotpath`/`--alloc`, `--commit`, `--compression`, `--snapshot`,
  and the I/O flags. pbfhogg hard-errors on invalid combinations (e.g. sparse
  without indexdata), so brokkr does no validation of its own beyond
  forwarding. The producer's four counters (`altw_member_ways`,
  `altw_pinned_refs`, `altw_field5_bytes`, `altw_field20_ways_emitted`) ride
  the existing sidecar FIFO counter channel and show up in
  `brokkr sidecar --counters` unchanged. **`brokkr verify
  add-locations-to-ways --inject-prepass` is refused** (nonzero exit, no diff
  run): enriched output is osmium-incompatible by design (field-5 headers run
  ~1-8 KB; libosmium 2.23 rejects any BlobHeader over 127 bytes, their issue
  405), so there is no reference tool to cross-validate against. Run flag-off
  verify for the element semantics; enriched correctness is covered by
  pbfhogg's own oracle-roundtrip + backend-parity suite.
- `--locations-on-ways` - (`apply-changes` only) passes through to the child
  pbfhogg invocation.

### Durable enriched output (re-enrichment workflow)

`add-locations-to-ways` writes to scratch and cleans up after every
run/bench iteration - the output does not survive by design. To produce an
enriched file that survives (e.g. to register as a dataset's `locations`
variant for elivagar's `tilegen --variant locations`), run the producer
through the raw passthrough, which does no cleanup:

```
brokkr passthrough add-locations-to-ways <input.osm.pbf> \
  -o <durable/out.osm.pbf> --index-type external --inject-prepass \
  --compression zlib:6
```

then register the file manually in `brokkr.toml` under
`pbf.locations` with a `brokkr env` xxhash. (There is no `--as-snapshot`
promotion for `add-locations-to-ways`: that machinery routes into the
`snapshot` graph under `pbf.indexed`, which is the wrong target for a
top-level `locations` variant.)

## download command

`download <region> [--osc-seq N]` - download PBF + OSC from Geofabrik.
Accepts short aliases (`denmark`, `europe`) or full Geofabrik paths
(`europe/france`, `asia/japan/kanto`). Dataset key is the last path component.
Checks configured filenames in `brokkr.toml` before downloading. `--osc-seq N`
downloads all missing diffs from `last_configured_seq + 1` through N. After
downloading, computes xxh128 hashes and appends new entries to `brokkr.toml`.
Filenames follow project convention: `{key}-{YYYYMMDD}-seq{N}.osc.gz`,
`{key}-{YYYYMMDD}.osm.pbf`.
