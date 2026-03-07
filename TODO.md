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

### ~~`query_compare_last` may not return the two most recent commits~~ FIXED
`db/compare.rs`: changed `SELECT DISTINCT ... ORDER BY id DESC` to `GROUP BY [commit] ORDER BY MAX(id) DESC LIMIT 2`.

### ~~Orphaned server process in bench_tiles on mid-benchmark error~~ FIXED
`nidhogg/bench_tiles.rs`: added `ChildGuard` RAII wrapper that kills+waits the child on drop. Guard is consumed before SIGTERM in the normal path.

---

## Missing error handling

### ~~No HTTP status code checking in nidhogg curl helpers~~ FIXED
`nidhogg/client.rs`: added `--fail-with-body` to `curl_get` and `curl_post`. Server 4xx/5xx now fail with curl exit code 22.

### ~~No timeout on `curl_get`/`curl_post`~~ FIXED
`nidhogg/client.rs`: added `--max-time 30` to both helpers.

### ~~Silent hotpath JSON failure~~ FIXED
`harness.rs` `run_hotpath_capture`: now prints `[error]` diagnostic when the hotpath JSON file is missing, unreadable, or invalid.

### `run_curl_timed` silently defaults to 0.0
`nidhogg/bench_api.rs`: if `time_total` can't be parsed as f64, it defaults to `0.0`, silently recording a 0ms benchmark result.

### `run_passthrough_timed` loses signal-kill information
`output.rs`: uses `status.code().unwrap_or(1)` for signal-killed processes, losing the information that the process was killed by a signal (e.g. OOM killer SIGKILL).

### `serde_json::Error` maps to `DevError::Config`
`error.rs`: a cargo metadata JSON parse failure in `build.rs` gets reported as a "config" error instead of "build" error, which is misleading.

---

## Config validation

### No `deny_unknown_fields` on config structs
`config.rs`: `HostConfig`, `Dataset`, `PbfEntry`, `OscEntry`, `PmtilesEntry` all silently accept unknown fields. A typo like `origni = "Geofabrik"` or `sha265 = "abc"` is silently ignored. Adding `#[serde(deny_unknown_fields)]` would catch these.

### `sha256` + `xxhash` coexistence silently accepted
`config.rs`: with `#[serde(alias = "sha256")]`, if both `sha256` and `xxhash` are present in the same TOML entry, serde silently uses last-writer-wins. No error or warning during migration.

### No validation of empty file names
`config.rs`: `file = ""` parses fine and propagates to path resolution, failing later with an opaque I/O error.

### No bbox format validation
`resolve.rs`: `--bbox` values and dataset-configured bbox strings are accepted verbatim with no check for 4 comma-separated floats or min < max. Fails downstream.

### `resolve_nidhogg_data_dir` does not check directory existence
`resolve.rs`: unlike PBF/OSC/PMTiles resolvers which verify `path.exists()`, this returns a path without checking the directory exists.

---

## Code duplication

### resolve_pbf/osc/pmtiles share identical 5-step pattern
`resolve.rs`: `resolve_pbf_path`, `resolve_osc_path`, `resolve_pmtiles_path` all do: lookup dataset -> lookup entry -> join path -> check exists -> verify hash. Could be collapsed into one generic helper + thin wrappers, saving ~40 lines. Same for the two `resolve_default_*` functions.

### env.rs dataset check loops are identical
`env.rs`: PBF, OSC, and PMTiles loops in `check_datasets` are structurally identical (build label, join path, check exists, check hash). Could extract a helper.

### ~~Missing `#[derive(Clone)]` causes ~60 lines of manual clone code~~ FIXED
`db/types.rs`: added `#[derive(Clone)]` to `Distribution`, `HotpathFunction`, `HotpathThread`, `HotpathData`. Removed `reconstruct_hotpath` (harness.rs) and `take_hotpath_for_compare` (format.rs).

### JSON element-count parsing repeated in 4 nidhogg files
`nidhogg/bench_api.rs`, `query.rs`, `verify_batch.rs`, `verify_readonly.rs`: each reimplements `parsed.get("elements").and_then(|v| v.as_array())`. A shared helper in `client.rs` would reduce this.

### Geocode response parsing repeated in 3 nidhogg files
`nidhogg/geocode.rs`, `verify_geocode.rs`, `verify_readonly.rs`: parsing geocode JSON array and extracting `displayName`/`lat`/`lon` from the top result is repeated.

### Path-to-string conversion boilerplate in nidhogg
`nidhogg/ingest.rs` (3x), `hotpath.rs` (2x), `profile.rs` (1x): the `path.to_str().ok_or_else(|| DevError::Config("... not valid UTF-8"))` pattern could be a utility function.

### ~~Duplicate geocode defaults~~ FIXED
`nidhogg/cmd.rs`: now reuses `client::GEOCODE_TEST_QUERIES` instead of a local duplicate array.

---

## Inconsistencies

### ~~Inconsistent available-variant listing in resolve errors~~ FIXED
`resolve.rs`: all three resolve functions now list available keys on miss, all sorted.

### ~~`DatasetStatus::NoPbf` name is misleading~~ FIXED
`env.rs`: renamed to `DatasetStatus::NoFiles`.

### Double hashing on mismatch in env.rs
`env.rs` `check_hash_status`: `verify_file_hash` computes the hash internally, then on failure `cached_xxh128` is called again to get the actual hash for display. The second call hits cache so no perf issue, just redundant logic.

### io_uring status in env.rs incomplete
`env.rs`: `check_uring_disabled` only checks the kernel kill switch (`/proc/sys/kernel/io_uring_disabled`), ignoring AppArmor restrictions that `preflight.rs` checks for. `brokkr env` could report "supported" when io_uring would actually be blocked.

---

## Fragility

### bench_tiles startup detection via string matching
`nidhogg/bench_tiles.rs`: waits for stderr to contain `"Listening on"`. If nidhogg changes this message text, the benchmark hangs for 30s then fails.

### `find_executable` fallback is order-dependent
`build.rs`: when `expected_name` is `None`, falls back to `last_exe` — the last executable in cargo's JSON output. Cargo doesn't guarantee ordering.

### All nidhogg test data hardcoded to Denmark
`nidhogg/bench_api.rs`, `verify_batch.rs`, `client.rs`: all query bboxes, geocode terms, and API test queries use Copenhagen/Denmark coordinates. Would not work for non-Denmark datasets.

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

