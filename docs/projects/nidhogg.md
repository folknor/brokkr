# nidhogg project notes

`project = "nidhogg"` in `brokkr.toml`.

## Module layout

- `src/nidhogg/commands.rs` - `NidhoggCommand` enum (Api/Ingest/Tiles) with
  command identity, build package selection, and the shared ingest argv.
- `src/nidhogg/plan.rs` - binary, HTTP and server lifecycle plans for
  `src/measure/runner.rs`; execution uses one `MeasurementSession`.
- `src/nidhogg/...` - server lifecycle (serve/stop/status), ingest, update,
  query, geocode, benchmarks (api, ingest, tiles), verify (batch, geocode,
  readonly), hotpath.
- `src/nidhogg/client.rs` - query/bbox helpers that derive API queries from
  dataset bbox.

`query` needs an explicit `--json` body because it has no dataset-independent
default bbox. `verify geocode` likewise needs one or more explicit search
terms, since the matching places depend on the configured index.

All `brokkr verify` variants use the command lock, including `verify batch`
and `verify geocode`. Those two query a running server without building, but
they still wait for the lock so they do not overlap other locked work.

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

`serve` holds a shutdown guard from the spawn through the readiness poll. A
`brokkr kill` or Ctrl-C in that window stops the half-started server (verified
SIGTERM, then SIGKILL, pid file removed) and ends the command as interrupted
(exit 130). The build before the spawn holds its own guard, and the earlier
stop of the previously recorded server is outside both. Once the server is
ready the guard is gone and the server stays detached as before.

The persistent server uses the shared plain spawn policy, carrying OOM and
hold protection while surviving the `serve` command. The server started for
a tiles measurement uses service policy, including parent-death termination
and the orphan-reap token. HTTP requests use the shared captured child
supervisor; incomplete captured output fails rather than being parsed. A
request that hits its deadline fails as a deadline; every other ending goes
through the runner's shared ending verdict (`check_success`), with curl's
error text and any response preview carried in the failure.

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
call runs under the shared child supervisor's wall deadline (the budgets live
in `src/nidhogg/policy.rs`), so a server that accepts and hangs cannot stall a
command; a health check past its deadline reads as "not up".

## `bench api` timing

`bench api` runs each query N times through `run_distribution` and records one
row per query (`command = api-<query>`) with the min/p50/p95/max of the N
samples. A sample is curl's `%{time_total}` - the HTTP round trip, excluding
process spawn - kept at curl's microsecond resolution. Queries against a warm
server often finish in well under a millisecond, so the summary is stored in
microseconds (`run_distribution`'s `*_us` columns, and the row's `elapsed_us`
is the fastest sample) and `brokkr results` prints it as fractional
milliseconds (`0.312 ms`). The integer `*_ms` columns are those rounded, and
read 0 for such a query; rows recorded before schema v19 have only those, so
their sub-millisecond detail is gone.

See `docs/brokkr.toml.md` for full dataset schema.

## Measured ingest lifecycle

`nid-ingest` uses one standalone runner for run, bench, hotpath and alloc.
Run executes once without recording; stored modes take their sample count and
invocation provenance from the request. The result command remains
`nid-ingest`, and subprocess argv retains the existing mode-specific output
names (`run-ingest-output`, `bench-ingest-output`, `hotpath-ingest-output`).

`--dry-run` is supported for ingest: resolve the input and construct argv,
without building, executing, or creating scratch directories. API and tiles
also support dry-run through HTTP/server lifecycle plans: resolution does not
contact or start a server. They still reject hotpath/alloc. Run executes one
sample per selected query or lifecycle without recording. Stored command names
remain `api-<query>` and `tiles`, with absent `cli_args`.

Scratch is reset before every sample in every mode, run and hotpath/alloc
included, so no sample can find data an earlier one ingested. A successful
invocation removes its scratch; any failure, a nonzero exit included, leaves
the current scratch in place for diagnosis. Stored modes gate a dirty tree
unless forced; run allows one.
