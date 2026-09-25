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
  allocator, blob-filter, planetiler, all), verify (one check per
  cross-validated command, plus `all`),
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

**Verdicts.** A check fails by returning `Err` (a tool crashed, output could
not be parsed) or by returning `Findings` that carry a failed comparison;
`run_check` folds the latter into a FAIL exactly like the former, detail
replay included. The comparison helpers on `VerifyHarness` (`diff_pbfs`,
`check_sorted`, `compare_sort_feature`) return a `#[must_use]` `Verdict`
rather than a `bool`, so a comparison whose result is dropped is a compile
warning instead of a silent PASS - the only way to consume one is
`findings.record(...)`. `diff_pbfs` reads `pbfhogg diff`'s exit status: 0 with
no listing is identical, 1 is different, anything else (a signal, another
code) is an error - never "identical". Two comparisons are deliberately not
verdicts: `extract`'s element diff against osmium (known to differ; reported
on the summary channel as informational), and the `dense` index variant of
`add-locations-to-ways`, whose non-zero exit is taken as an allocation
failure on hosts without `vm.overcommit_memory=1` and skipped.

**Fresh outputs.** `VerifyHarness::subdir` empties `target/verify/<check>/`
before handing it back, so every file a check finds there was written by this
run. Checks rely on that: `verify_merge` treats the diff OSC's existence as
proof the diff ran and prints the optional osmosis/osmconvert outputs only if
present, and a failed optional tool's partial output is removed.

**`verify all` inputs.** The OSC- and bbox-consuming checks skip only when
the input is not configured at all (no `--osc-seq` and no OSC registered for
the dataset or named snapshot; no `--bbox` and no dataset `bbox`). A
configured input that fails to resolve - hash mismatch, missing file, several
OSCs and no `--osc-seq`, a malformed bbox - fails the command before the suite
runs rather than turning its checks into skips. osmosis setup failure is
non-fatal but narrated.

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
only IDs are needed. The document itself is checked: the first element must
be the `<osmChange>` root and the root must be closed, with nothing after it.
An empty file, a non-OSC document or a truncated diff is an error, not an
empty `OscDiff` - `verify_merge` would otherwise report it as
"element-identical PASS".

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
- `--force-altw` - (`add-locations-to-ways` only) appends `--force` to the
  pbfhogg child (`src/pbfhogg/commands.rs`, `AddLocationsToWays` arm), skipping
  its indexdata requirement so raw / non-indexed input reaches the decode-all
  fallback path. Named to disambiguate from brokkr's own per-subcommand
  `--force` dirty-tree override, mirroring `--force-repack`. Composes with
  `--inject-prepass`, `--index-type`, `--compression`, and the measurement
  modes. brokkr does no validation of its own - pbfhogg owns the semantics. The
  intended cell: `brokkr add-locations-to-ways --dataset europe --variant raw
  --index-type sparse --compression zstd:1 --force-altw --bench 3`.
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
downloading, computes xxh128 hashes and registers the new entries in
`brokkr.toml`. Filenames follow project convention: `{key}-{YYYYMMDD}-seq{N}.osc.gz`,
`{key}-{YYYYMMDD}.osm.pbf`. The indexed variant is generated with plain
`pbfhogg cat <raw> -o <indexed>` (`generate_indexed_pbf`, shared by the
primary, `--as-snapshot` and `--refresh` flows).

Every `brokkr.toml` change a download flow (or `--as-snapshot` promotion)
makes goes through one `toml_edit` transaction (`DatasetToml` in
`src/pbfhogg_mod/download_toml.rs`): keys and values are escaped by the
writer, comments and unrelated tables survive untouched, and the batch is
committed once with an atomic replace, only after every file it names exists
and is hashed. A failure before the commit leaves `brokkr.toml` unchanged -
in particular `--refresh` never leaves a dataset with its primary rotated out
and no replacement registered. `--replace-snapshot` unlinks the displaced
snapshot's files only after the new registration is committed, and keeps any
file the primary or another snapshot still names. `--refresh` refuses when
today's dated filenames are already registered (a same-day refresh would
overwrite the primary it is archiving).
