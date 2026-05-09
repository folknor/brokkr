# Sync orchestration: mock-serve, sync-list, sync-smoke, sync-bench

Gated to `project = "ratatoskr"`. These commands spawn sæhrimnir (an external
mock email server) alongside the ratatoskr harness binary so sync scripts can
exercise real protocol stacks.

For the `[ratatoskr]` config block (mock_server_binary, fixtures_dir, endpoint
env-var spellings, sync_script_dir) see `docs/brokkr.toml.md`. For the harness
model and sæhrimnir contract see `docs/projects/ratatoskr.md`.

Helpers live in `src/ratatoskr/sync.rs` (orchestration) and
`src/ratatoskr/saehrimnir.rs` (spawn/sentinel/teardown).

## `mock-serve --fixture <NAME>`

Plan-3 manual-exploration tool. Reads `[ratatoskr] mock_server_binary` and
`[ratatoskr] fixtures_dir` from `brokkr.toml` (both required), resolves the
fixture to `<fixtures_dir>/<NAME>.toml` or `<fixtures_dir>/<NAME>.lua`
(whichever exists; both is an error - pass the name with its extension to
disambiguate), spawns sæhrimnir with `--fixture <PATH> --readiness-file
.brokkr/ratatoskr/mock/readiness` and stdio inherited so logs land live, polls
(50ms cadence, 10s budget) for the readiness sentinel, parses the five-line
`<NAME> <port>` content via `parse_sentinel`, prints the per-protocol
HTTP/host:port endpoints, then loops until SIGINT/SIGTERM. On signal: SIGTERM
the child, grant 1.5s, escalate to SIGKILL. If sæhrimnir exits before writing
the sentinel (fixture-validation error, port-in-use, etc.) the spawn-side
error surfaces with the captured stderr already on the user's terminal.

Auto-build of sæhrimnir is not yet wired - the binary must already exist at
`mock_server_binary`.

## `sync-list`

Walk `[ratatoskr] sync_script_dir` (default `crates/app/tests/sync-harness`),
parse top-of-file frontmatter (`description`, `expected`, `fixture`,
`protocol`, `ceiling`, `preserve_data_dir`), print a sorted table. Empty-state
output names the expected directory and notes that the cohort may not have
landed yet. Pure brokkr - no sæhrimnir or harness-binary spawn.

## `sync-smoke <SCRIPT> [--keep-artefacts] [--debug]`

Two-child orchestration. Validates `[ratatoskr.harness]`, `[ratatoskr]
mock_server_binary`, and `[ratatoskr] fixtures_dir`, parses the script's
`-- fixture: <NAME>` frontmatter, acquires the lockfile, builds the harness
sweep, allocates `.brokkr/ratatoskr/sync/<test>/run-N/` with `harness/` and
`mock/` subdirs, spawns sæhrimnir with `--fixture <PATH> --readiness-file
mock/readiness` (its stderr piped to `mock/stderr.log`), parses the readiness
sentinel for per-protocol ports, then spawns `<harness binary> --test-harness
<SCRIPT>` with `BROKKR_HARNESS_ARTEFACT_DIR` and `BROKKR_TEST_BIN_DIR` set
plus one `RATATOSKR_TEST_<PROTO>_ENDPOINT` per protocol whose env-var spelling
is configured under `[ratatoskr]` (HTTP origins for jmap/graph/gmail,
`host:port` for imap/smtp).

During the run brokkr publishes both PIDs into the lockfile - sæhrimnir
goes into the auxiliary `mock_pid` slot, the harness binary into `child_pid`
- so `brokkr lock` from another shell shows live RSS/CPU for both, and
`brokkr kill --hard` SIGKILLs both. `brokkr kill` (cooperative SIGTERM) is
caught by a guard installed for the post-build window; the captured runner
forwards SIGTERM to the harness child with a 1.5s budget, then mock-teardown
drains sæhrimnir, then brokkr exits with `DevError::Interrupted`.

After the harness exits, brokkr SIGTERMs sæhrimnir with the standard 1.5s
budget then escalates to SIGKILL. PASS/FAIL on the harness exit code; FAIL
preserves the artefact dir with `run.toml` (top-level metadata: brokkr
version, sweep, harness exit code/elapsed, mock outcome) plus the harness's
own artefacts and the captured mock stderr.

The PASS/FAIL line carries a phase summary so a slow run is decomposable at
a glance: `PASS in 3.7s (mock 0.4s, harness 3.2s, shutdown 0.1s)`. Phases
that didn't run (e.g. a spawn-side failure before the harness started) are
omitted, and the leading `in <total>` is dropped entirely if no phase
recorded.

## `sync-bench <SCRIPT> [--bench N] [--force] [--keep-artefacts] [--debug]`

Measured variant of sync-smoke. Same two-child shape, but sæhrimnir is spawned
once and reused across `--bench` iterations (default 3), and the harness
binary runs N times with `BROKKR_MARKER_FIFO` set. Each iteration gets its own
`iter-K/harness/` subdir under the run dir; the script emits `SYNC_START` and
`SYNC_END` markers via the FIFO around the measured region (last `SYNC_START`
wins, first `SYNC_END` after it ends the span - so a warmup loop under the
same name is fine).

Best-of-N selection: marker span if both markers fired, else wall-clock
elapsed. The best iteration's `summary.json` (if the script writes one into
`BROKKR_HARNESS_ARTEFACT_DIR`) gets ingested as `meta.<key>` KvPair rows:
numeric values become Int/Real, strings become Text, nested objects/bools/null
are skipped. Storage is via the standard `BenchHarness`, so `brokkr results
--compare` and the sidecar DB work the same as for pbfhogg/elivagar benches;
sidecar provenance (RunInfo) is omitted in v0 because the helper that builds
it is private to BenchHarness today. `--force` allows recording on a dirty
git tree (rows land under the `dirty` alias).
