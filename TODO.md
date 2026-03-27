# TODO

## Won't fix

### Inconsistent path-to-string conversion
All paths are constructed from known UTF-8 components, so `.display().to_string()` won't corrupt in practice. 100+ occurrences across 30+ files — not worth the churn.

### Hand-rolled UUID via `/dev/urandom`
10 correct lines in `src/db.rs`. Not worth adding the `uuid` crate as a dependency.

### `#[allow(clippy::too_many_arguments)]` proliferation
Functions genuinely need many parameters. `BenchContext` and `HarnessContext` cover the common cases; remaining allows are the pragmatic choice.

---

## Bugs

### ~~`run_curl_timed` silently defaults to 0.0~~ FIXED
`nidhogg/bench_api.rs`: now returns `DevError::Verify` if `time_total` can't be parsed.

### ~~`run_passthrough_timed` loses signal-kill information~~ FIXED
`output.rs`: now returns `DevError::Subprocess` with signal number and name (SIGKILL, SIGTERM, SIGSEGV) when a process is killed by a signal.

### ~~`serde_json::Error` maps to `DevError::Config`~~ FIXED
`error.rs`: blanket `From<serde_json::Error>` now maps to `DevError::Build`. Nidhogg API response parsing uses explicit `.map_err()` to `DevError::Verify`.

---

## Config validation

### ~~No `deny_unknown_fields` on config structs~~ FIXED
`config.rs`: added `#[serde(deny_unknown_fields)]` to `HostConfig`, `Dataset`, `PbfEntry`, `OscEntry`, `PmtilesEntry`.

### ~~`sha256` + `xxhash` coexistence silently accepted~~ FIXED
`config.rs`: `deny_unknown_fields` + serde alias causes duplicate field rejection. Added test.

### ~~No validation of empty file names~~ FIXED
`config.rs`: `validate_datasets()` rejects empty file names at parse time with a clear error path.

### ~~No bbox format validation~~ FIXED
`resolve.rs`: `validate_bbox()` checks for exactly 4 comma-separated floats.

### ~~`resolve_nidhogg_data_dir` does not check directory existence~~ FIXED
`resolve.rs`: now checks `path.exists()` like the other resolvers.

---

## Code duplication

### ~~resolve_pbf/osc/pmtiles share identical 5-step pattern~~ FIXED
`resolve.rs`: `FileEntry` trait + `resolve_entry_path` / `resolve_default_entry_path` generic helpers replace 3 resolve functions and 2 default resolvers.

### ~~env.rs dataset check loops are identical~~ FIXED
`env.rs`: `check_file_entries` generic helper replaces 3 identical loops using the `FileEntry` trait.

### ~~JSON element-count parsing repeated in 4 nidhogg files~~ FIXED
`client.rs`: `element_count()` helper used by `bench_api.rs`, `verify_batch.rs`, `verify_readonly.rs`.

### ~~Geocode response parsing repeated in 3 nidhogg files~~ FIXED
`client.rs`: `geocode_top_name()` helper used by `geocode.rs`, `verify_geocode.rs`.

### ~~Path-to-string conversion boilerplate in nidhogg~~ FIXED
`client.rs`: `path_str()` helper replaces 7 instances across `ingest.rs`, `hotpath.rs`, `profile.rs`, `bench_ingest.rs`, `update.rs`.

---

## Inconsistencies

### ~~Double hashing on mismatch in env.rs~~ FIXED
`env.rs`: `check_hash_status` now calls `cached_xxh128` once and compares directly.

### ~~io_uring status in env.rs incomplete~~ FIXED
`env.rs`: `check_uring_blocked` now checks all 3 kernel parameters (kill switch + both AppArmor restrictions), matching `preflight.rs`.

---

## Fragility

### ~~bench_tiles startup detection via string matching~~ FIXED
`nidhogg/bench_tiles.rs`: now uses case-insensitive match on "listening" instead of exact `"Listening on"` string.

### ~~`find_executable` fallback is order-dependent~~ FIXED
`build.rs`: when `expected_name` is `None`, now requires exactly one executable — errors with a clear message if multiple are found instead of picking the last one.

### ~~All nidhogg test data hardcoded to Denmark~~ FIXED
`bench_api`, `verify_batch`, `verify_readonly`: queries are now derived from the dataset bbox in `brokkr.toml` via `client::build_api_queries()` / `build_batch_queries()`. Geocode queries remain as user-supplied with Denmark defaults (geocode terms are inherently locale-specific).

---

## Backlog

### Make default binary configurable per-project in brokkr.toml

Currently `find_executable` infers the expected binary name from `BuildConfig.bin` or `BuildConfig.package`. This should be configurable in `brokkr.toml` (e.g. a `default_bin` field per project) so projects with multiple binaries can declare which one brokkr should run by default.

### `Worktree` has no `Drop` impl

If the process panics or is killed (SIGKILL/SIGTERM) inside a `--commit` benchmark, the worktree at `.brokkr/worktree/<hash>` is left behind. Mitigated: `Worktree::create` cleans up stale worktrees at the same path before creating a new one. A `Drop` impl would require interior mutability or an `Option` wrapper — probably not worth the complexity.

### `--mem` systemd-run wrapping for `brokkr run`

The old `run_elivagar` had `--mem 8G` support via systemd-run. Could be promoted to a project-agnostic `brokkr run --mem 8G` flag in `src/cli.rs`.

---

## History command enhancements

### Capture brokkr's own output
All brokkr output goes through `output::*` helpers (`build_msg`, `bench_msg`, `error`, etc.). Add a tee layer that copies prefixed lines into a global buffer, flushed to a nullable `output TEXT` column at end of invocation. Cap at 64KB. Does NOT cover passthrough subprocess output (`brokkr run`, `brokkr serve`) which uses `Stdio::inherit()` — capturing that would require piped tee threads and changes live output UX. Schema v2 migration alongside `error_tail`.

### Capture stderr tail on failure
On non-zero exit, capture the last 4KB of stderr into a nullable `error_tail TEXT` column. Requires schema v2 migration. Only stored on failure — success path stays lightweight. The `history <id>` detail view would display it, and `brokkr history --failed` could show a one-line preview.

### `--json` output
Useful for scripting (jq, dashboards, CI analysis) instead of only human-formatted lines.

### `history <id>` detail view
One command to inspect full metadata for a specific entry (cwd, commit, dirty, kernel, memory, exit status).

### `--from-last-success` / `--failed` + `--rerun <id>`
Fast recovery loop: find last failed command and re-execute it exactly.

### `--project-dir <path-substring>` filter
`--project` is great, but directory filter helps when you have multiple clones/worktrees of the same project.

### `--until <date>` in addition to `--since`
Time-range queries are much more useful than one-sided filtering.

### `--status <code>` filter
`--failed` is coarse; filtering specific exit codes (e.g. 130 for interrupt) is valuable.

### `--sort slow|recent` and `--top-slowest N`
Makes performance triage easier without external tooling.

---

## CLI redesign remaining issues

### Elivagar/nidhogg default mode runs through full harness
`brokkr tilegen` (no `--bench` flag) still goes through `BenchHarness` with DB storage. Pbfhogg has a lightweight `run_pbfhogg_run` path; elivagar and nidhogg need equivalents.

### Suite without --bench stores results in DB
`brokkr suite pbfhogg` (no `--bench`) calls `bench_all` which stores results. May not be worth fixing — suite is inherently a benchmarking operation.

### Suite builds without host features
`bench_all.rs` calls `cargo_build` without host features from `brokkr.toml`. Individual commands correctly include them via `BenchContext::new`. Pre-existing.

### Standalone extract commands use hardcoded Copenhagen bbox
`ExtractSimple/Complete/Smart` (bench_commands variants) hardcode `12.4,55.6,12.7,55.8`. The `Extract { strategy }` variant uses dataset bbox. Pre-existing, intentional for consistent benchmarking.

### --bench 0 not validated early
Harness catches it after building. Low priority.

### Nidhogg hotpath ignores command-specific context
`RunApi --hotpath` ignores `--query`, `RunTiles --hotpath` ignores `--tiles`/`--uring`. Nidhogg hotpath is a single generic function. Pre-existing.

### Remove --runs from elivagar/nidhogg CLI variants
Mode-based run counts (`--bench 5`) now take precedence. The standalone `--runs` field is dead when a mode flag is set. Should be removed.

---

## CLI sync backlog

Last synced at brokkr commit `e9bb402` (2026-03-03). Both pbfhogg and elivagar have expanded significantly since then.

### pbfhogg: new commands missing from `bench commands`

6 new CLI commands have no brokkr benchmark or verify coverage:

- `renumber` — reassign element IDs sequentially with ref remapping
- `merge-pbf` — merge N sorted PBFs with blob-level passthrough and dedup
- `merge-changes` — merge multiple OSC files into one, with simplify mode
- `getparents` — reverse lookup for ways/relations referencing given IDs
- `tags-filter-osc` — filter OSC changes by tag expressions (with delete passthrough)
- `time-filter` — filter history PBF to a snapshot at a timestamp

`is-indexed` is also new but too trivial to benchmark (instant check).

### pbfhogg: new flags on existing commands

New flags that could warrant additional `bench commands` variants or verify coverage:

- `tags-filter`: `-i/--invert-match`, `-e/--expressions`, `-t/--remove-tags`
- `getid`: `-I/--id-osm-file`, `--remove-tags`, `--verbose-ids`
- `diff`: `--summary`, `--quiet`, `--output`
- `inspect`: `-e/--extended`, `-g/--get`, `--json`
- `extract`: `--config` (multi-extract from JSON), `--clean`, `--set-bounds`
- `cat`: `--clean`
- `check-refs`: `--show-ids`
- `derive-changes`: `--update-timestamp`, `--increment-version`
- `tags-count`: `-M`, `-s`

### elivagar: missing `verify` integration

Elivagar now has a `verify` subcommand for PMTiles output validation. Not wired into brokkr — should be added as a verify command.

### elivagar: new `run` flags not exposed in benchmarks

The following elivagar flags are not forwarded through `bench self`, `hotpath`, or `profile`:

- `--tile-format` (mvt/mlt) — MLT is a new tile format, benchmarking it matters
- `--tile-compression` (gzip/brotli) — compression strategy affects perf
- `--compress-sort-chunks` — LZ4 compression of sort spill data
- `--in-memory` — keep tile blob in RAM
- `--locations-on-ways` — PBF format flag
- Memory budgets (`--sort-budget`, `--way-budget`, `--rel-budget`, `--assemble-budget`) — tuning knobs, lower priority

No schema changes needed: `bench_self.rs` already stores flags as `meta.*` kv pairs in `run_kv` and the full command line in `cli_args`. New flags just need CLI plumbing + `KvPair::text("meta.<flag>", ...)` entries in the metadata vec.

