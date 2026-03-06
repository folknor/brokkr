# TODO

## Won't fix

### Inconsistent path-to-string conversion
All paths are constructed from known UTF-8 components, so `.display().to_string()` won't corrupt in practice. 100+ occurrences across 30+ files — not worth the churn.

### Hand-rolled UUID via `/dev/urandom`
10 correct lines in `src/db.rs`. Not worth adding the `uuid` crate as a dependency.

### `#[allow(clippy::too_many_arguments)]` proliferation
Functions genuinely need many parameters. `BenchContext` and `HarnessContext` cover the common cases; remaining allows are the pragmatic choice.

---

## Backlog

### Make default binary configurable per-project in brokkr.toml

Currently `find_executable` infers the expected binary name from `BuildConfig.bin` or `BuildConfig.package`. This should be configurable in `brokkr.toml` (e.g. a `default_bin` field per project) so projects with multiple binaries can declare which one brokkr should run by default.

### `Worktree` has no `Drop` impl

If the process panics or is killed (SIGKILL/SIGTERM) inside a `--commit` benchmark, the worktree at `.brokkr/worktree/<hash>` is left behind. Mitigated: `Worktree::create` cleans up stale worktrees at the same path before creating a new one. A `Drop` impl would require interior mutability or an `Option` wrapper — probably not worth the complexity.

### `--mem` systemd-run wrapping for `brokkr run`

The old `run_elivagar` had `--mem 8G` support via systemd-run. Could be promoted to a project-agnostic `brokkr run --mem 8G` flag in `src/cli.rs`.

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

