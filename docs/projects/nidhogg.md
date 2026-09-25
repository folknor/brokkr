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

`serve` / `stop` / `status` manage the long-running nidhogg server. `status`
is an HTTP health check against the configured port. `serve` records the
server's PID in `.brokkr/nidhogg.pid` together with its `/proc` starttime, and
`stop` signals that process only after re-verifying the starttime - a recycled
PID, a pid file with no starttime, or an unreadable `/proc` entry is refused,
never signalled, and there is no name-based `pkill` fallback. `ingest`,
`update`, `query`, and `geocode` operate against a running server or against
the on-disk data dir directly; `ingest` creates the dataset's `data_dir` if it
does not exist yet.

The port is `[<host>] port` in `brokkr.toml`, defaulting to 3033. brokkr hands
it to the server as `PORT` but does not read `PORT` from its own environment.

`verify readonly` strips write permission from `geocode_index/` for the
duration of the test and restores each entry's original mode exactly, from an
RAII guard - so a failing or panicking test never leaves the index read-only,
and files that had no write bit before never gain one. Ctrl-C and `brokkr kill`
during the test are caught and turned into an early exit that runs the restore
(exit 130); only `brokkr kill --hard` (SIGKILL) can still skip it.

HTTP checks fail loudly: `bench api` treats an HTTP error or a request over its
time ceiling as a failed sample rather than a fast one, and its post-run
element/byte report errors instead of printing zeros; the verify commands'
FAIL lines carry curl's exit code, error line and a response-body preview; a
missing `curl` is reported as such, not as "server not running". Every curl
call has a `--max-time`, so a server that accepts and hangs cannot stall a
command.

See `docs/brokkr.toml.md` for full dataset schema.
