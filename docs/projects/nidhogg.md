# nidhogg project notes

`project = "nidhogg"` in `brokkr.toml`.

## Module layout

- `src/nidhogg/commands.rs` - `NidhoggCommand` enum (Api/Ingest/Tiles) with
  `id()`, `supports_hotpath()`, `needs_build()`, `needs_server()`,
  `metadata()`.
- `src/nidhogg/dispatch.rs` - exposes `run_command()`. Delegates to per-module
  functions (server lifecycle, ingest, query, geocode, etc.) due to divergent
  lifecycles. Does NOT use `BenchContext` for everything (unlike pbfhogg /
  elivagar) - benchmarking commands route through `BenchContext` but server
  commands have their own lifecycle.
- `src/nidhogg/...` - server lifecycle (serve/stop/status), ingest, update,
  query, geocode, benchmarks (api, ingest, tiles), verify (batch, geocode,
  readonly), hotpath.
- `src/nidhogg/client.rs` - query/bbox helpers that derive API queries from
  dataset bbox.

## Dataset specifics

- `data_dir` field on dataset entries is nidhogg-specific (used for the
  ingested data directory layout).
- `pmtiles.<variant>` entries are consumed by `serve` and `bench tiles`.
- Variant default is `raw` (same as elivagar).

## Server commands

`serve` / `stop` / `status` manage the long-running nidhogg server. Status is
file-based (PID file under the host's scratch dir). `ingest`, `update`,
`query`, and `geocode` operate against a running server or against the
on-disk data dir directly.

See `docs/brokkr.toml.md` for full dataset schema.
