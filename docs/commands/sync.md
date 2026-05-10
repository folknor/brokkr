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

The signal handler is installed BEFORE sæhrimnir spawns (not after readiness
returns), and the readiness wait polls the same flag, so a `brokkr kill`
arriving during the readiness window aborts cleanly with the child reaped
- it can't orphan sæhrimnir.

Auto-build of sæhrimnir is not yet wired - the binary must already exist at
`mock_server_binary`.

## `sync-list`

Walk `[ratatoskr] sync_script_dir` (default `crates/app/tests/sync-harness`),
parse top-of-file frontmatter (`description`, `expected`, `fixture`,
`protocol`, `ceiling`, `preserve_data_dir`), print a sorted table. Empty-state
output names the expected directory and notes that the cohort may not have
landed yet. Pure brokkr - no sæhrimnir or harness-binary spawn.

## `sync-smoke <SCRIPT> [--keep-artefacts] [--debug | --release]`

Two-child orchestration. Validates `[ratatoskr.harness]`, `[ratatoskr]
mock_server_binary`, and `[ratatoskr] fixtures_dir`, parses the script's
`-- fixture: <NAME>` frontmatter, acquires the lockfile, builds the harness
sweep, allocates `.brokkr/ratatoskr/sync/<test>/run-N/` with `harness/` and
`mock/` subdirs, spawns sæhrimnir with `--fixture <PATH> --readiness-file
mock/readiness` (its stderr piped to `mock/stderr.log`), parses the readiness
sentinel for per-protocol ports, then spawns `<harness binary> --test-harness
<SCRIPT>` with `BROKKR_HARNESS_ARTEFACT_DIR` and `BROKKR_TEST_BIN_DIR` set
plus one `RATATOSKR_TEST_<PROTO>_ENDPOINT` per protocol whose env-var spelling
is configured under `[ratatoskr]` (HTTP origins for jmap/graph/gmail/caldav,
`host:port` for imap/smtp).

During the run brokkr publishes both PIDs into the lockfile - sæhrimnir
joins the auxiliary `mock_pids` set, the harness binary lands in
`child_pid` - so `brokkr lock` from another shell shows live RSS/CPU for
both. PG isolation is opt-in per spawn site: callers pass `isolate_pg = true`
only when a `SigtermGuard` is active for the spawn's lifetime. Sync-smoke,
service-test, service-suite, mock-serve, and BenchHarness's sidecar
window all qualify - their tracked children spawn with `process_group(0)`
and every intentional kill (`--hard`, deadline expiry, cooperative
SIGTERM, `MockServer::shutdown`) targets the whole group via
`kill(-pgid, ...)`, so descendants (sæhrimnir's protocol listeners,
harness helpers, the ratatoskr build's rustc) go down with the leader.
Sync-bench's pre-bench-loop spawns (cargo build, sæhrimnir mock) and
nidhogg's tile-server lifecycle stay in brokkr's foreground PG instead -
they're tracked in the lockfile so `brokkr lock` shows them, but
`--hard` falls back to a single-PID kill that may leave brief helper
orphans. Untracked subprocesses (`cargo metadata`, ad-hoc `cargo clippy`
from `brokkr check`, etc.) also stay in brokkr's PG so terminal Ctrl-C
reaches them through the kernel without our help. Terminal Ctrl-C is bridged: the
captured runner installs SIGINT alongside SIGTERM, both setting the
shutdown flag, and the wait-loop forwards SIGTERM to the child PG.
After the harness exits, `child_pid` is cleared so a stale PID can't be
SIGKILLed by `--hard` once the kernel has recycled it. `brokkr kill` (cooperative SIGTERM) is
caught by a guard installed right after the lockfile and held through
build + run + teardown; the captured runner (used for both `cargo build`
and the harness binary) polls the shutdown flag every 50ms, forwards
SIGTERM to whichever child is current with a 1.5s budget, then mock-
teardown drains sæhrimnir and brokkr exits with `DevError::Interrupted`.

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

## `sync-bench <SCRIPT> [--bench N] [--force] [--keep-artefacts] [--debug | --release]`

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

Lockfile / kill semantics: sæhrimnir joins the auxiliary `mock_pids` set
for the lifetime of the bench, and each iteration's harness PID rotates
through `child_pid` (cleared between iterations so PID-recycling can't
trip `--hard`), so `brokkr lock` shows both and `brokkr kill --hard`
SIGKILLs every entry. Cooperative `brokkr kill` (SIGTERM) is handled only by the
sidecar's own `SigtermGuard` around each measured iteration - no outer
guard is installed at sync-bench entry because nesting would clobber the
sidecar's `Drop`. SIGTERM during cargo build, sæhrimnir spawn, or the gap
between iterations therefore falls through to the default terminate
action (brokkr dies; mock and any in-flight harness child are reaped via
their `Drop` impls).
