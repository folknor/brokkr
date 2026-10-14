# Sync orchestration: sync, mock-serve

Gated to `project = "ratatoskr"`. These commands spawn sæhrimnir (an external
mock email server) alongside the ratatoskr harness binary so sync scripts can
exercise real protocol stacks.

`brokkr sync` is one command with three shapes, following the same
bare-is-an-index convention as `results`, `man` and `deps`:

| Invocation | What it does |
|---|---|
| `brokkr sync` | list the discovered scripts |
| `brokkr sync <SCRIPT>` | run one, PASS/FAIL |
| `brokkr sync --all` | run every discovered script |
| `brokkr sync <SCRIPT> --bench [N]` | measure one (default N=3) |
| `brokkr sync --gate all --bench [N]` | sweep every configured gate |

It replaced `sync-list` / `sync-smoke` / `sync-bench`, which were three
spellings of one workflow. The old names are gone, not aliased. Rows
recorded by `sync-bench` were rewritten to `sync` by results.db migration
v17→v18 (both the `command` column and the `brokkr_args` argv, since
`--compare` keys on both), so pre-rename history still pairs with new
runs. `gate.db` was untouched - its rows never carried a command name, so
every pinned baseline survived the rename.

Flags that only make sense while measuring (`--bench`, `--force`,
`--gate`, `--as-baseline`, `--commit`) are enforced by clap's `requires`,
so `brokkr sync --gate x` fails at parse time rather than silently
listing.

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
(50ms cadence, 10s budget) for the readiness sentinel, parses the
`<NAME> <port>` content (one line per protocol) via `parse_sentinel`, prints the per-protocol
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

## `sync` (no SCRIPT) - the index

Walk `[ratatoskr] sync_script_dir` (default `crates/app/tests/sync-harness`),
parse top-of-file frontmatter (`description`, `expected`, `fixture`,
`protocol`, `ceiling`, `preserve_data_dir`), print a sorted table. Empty-state
output names the expected directory and notes that the cohort may not have
landed yet. Every sync script must declare a fixture. Pure brokkr - no
sæhrimnir or harness-binary spawn. Unknown lower-case annotation keys,
duplicate fields, malformed recognized fields, invalid values, and missing
fixtures fail discovery with the script path and frontmatter line. Ratatoskr's
`@covers` and `measured` annotations are passed through; ordinary comments
after a blank line or bare `--` are outside the frontmatter block. The
informational `protocol: discovery` label describes WebFinger tests that use
the JMAP endpoint; it does not name a separate sæhrimnir listener.

## `sync --all [--filter SUB] [--include-ignored]` - run the cohort

Runs every discovered script unmeasured, in discovery order - the
sync-side counterpart to `service --all`, and the reason a cohort now
means the same thing in both families. `--filter` is a substring match
against the discovered name; scripts whose frontmatter says
`expected: ignored` are skipped unless `--include-ignored` is passed.

**The harness is built once, before the loop** - the cohort varies the
script, never the binary - and each script then reports exactly one
line:

```
[ratatoskr] account-verify-imap-success: PASS in 0.4s (mock 0.1s, harness 0.4s, shutdown 0.0s)
[ratatoskr] bifrost-consumer-lag-recovery: FAIL - config: sync: script ... has no `-- fixture: <NAME>` frontmatter line. ...
```

A failure that got far enough to allocate an artefact dir adds one
`artefacts preserved at ...` line; the `brokkr clean` hint prints once
at the end, not per failure. A 160-script sweep is therefore ~160 lines
plus a header and a `sync cohort: N/M passed` summary - it used to be
six lines and a no-op cargo invocation per script.

Discovery validates every script and resolves each selected fixture before
the lock and harness build begin, so a missing `fixture:` line or a fixture
typo cannot fail halfway through the cohort.

**Keep-going is the default here**, unlike `service --all`, which stops
on the first failure. The reason is that each sync script owns its own
`run-N/` artefact dir, so a later failure can't overwrite an earlier
one's triage material - there is nothing to protect by stopping, and a
cohort is most useful when it reports the whole blast radius. Exits
non-zero if any script failed; the exit error names the failures
without repeating the messages already printed inline.

Keep-going covers script failures, not interrupts. A `brokkr kill` or
Ctrl-C stops the sweep: the in-flight script reports `INTERRUPTED`, no
further script is started (the shutdown request is checked before each
spawn as well as on each result), the cohort prints how far it got, and
brokkr exits with `DevError::Interrupted`. Sæhrimnir's readiness wait
polls the same request, so an interrupt during mock startup does not wait
out the readiness budget.

The cohort holds the global lock for the whole sweep, not per-script,
so another brokkr invocation can't interleave a build or bench between
two scripts and the sweep can't stall mid-way waiting behind one. With
the build hoisted out of the loop this is the path's only acquire; the
lockfile's in-process re-entrancy is exercised by `--gate all`, whose
swept members still acquire internally (see
`docs/commands/ratatoskr-gate.md`).

`--all` conflicts with SCRIPT and with `--bench`: measuring a cohort
would interleave N iterations of one script with the sæhrimnir spawn of
the next, and best-of-N across different scripts means nothing. For
measurement across the cohort, use `--gate all`, which is scoped to the
gates that have pinned baselines to compare against.

## `sync <SCRIPT> [--keep-artefacts] [--debug | --release]` - run one

Two-child orchestration. Validates `[ratatoskr.harness]`, `[ratatoskr]
mock_server_binary`, and `[ratatoskr] fixtures_dir`, parses the script's
`-- fixture: <NAME>` frontmatter and resolves the fixture before building,
acquires the lockfile, builds the configured harness directly, and allocates
`.brokkr/ratatoskr/sync/<test>/run-N/` with `harness/` and
`mock/` subdirs, spawns sæhrimnir with `--fixture <PATH> --readiness-file
mock/readiness` (its stderr piped to `mock/stderr.log`), parses the
readiness sentinel for per-protocol ports, then spawns `<harness binary>
--test-harness <SCRIPT>` with `BROKKR_HARNESS_ARTEFACT_DIR` and
`BROKKR_TEST_BIN_DIR` set
plus one `RATATOSKR_TEST_<PROTO>_ENDPOINT` per protocol whose env-var spelling
is configured under `[ratatoskr]` (HTTP origins for
jmap/graph/gmail/caldav/carddav/people/gcal, `host:port` for imap/smtp).

During the run brokkr publishes both PIDs into the lockfile - sæhrimnir
joins the auxiliary `mock_pids` set, the harness binary lands in
`child_pid` - so `brokkr lock` from another shell shows live RSS/CPU for
both. Tracked sæhrimnir, cargo-build, and harness children use their own
process groups across service and sync. Deadline, cooperative shutdown, and
`--hard` cleanup target the child group, including helper descendants.
Terminal Ctrl-C and `brokkr kill` are bridged by the captured runner: it
forwards SIGTERM to the active group, waits up to 1.5s, and then escalates.
After the harness exits, `child_pid` is cleared so a recycled PID cannot be
SIGKILLed by `--hard`. Every mock is unregistered when its shared lifecycle
shuts it down; if a later step fails, the service cohort drains its mocks
before propagating the original failure.

After the harness exits, brokkr SIGTERMs sæhrimnir with the standard 1.5s
budget then escalates to SIGKILL. PASS/FAIL on the harness exit code; FAIL
preserves the artefact dir with `run.toml` (top-level metadata: brokkr
version, script, binary, features, harness exit code/signal/elapsed, git
state, and a `[mock]` table with the mock outcome) plus the harness's own
artefacts and the captured mock stderr. `run.toml` is serialized, not
string-formatted, so a quote or backslash in a path cannot corrupt it, and
its field names match `service`'s `run.toml`.

The PASS/FAIL line carries a phase summary so a slow run is decomposable at
a glance: `PASS in 3.7s (mock 0.4s, harness 3.2s, shutdown 0.1s)`. Phases
that didn't run (e.g. a spawn-side failure before the harness started) are
omitted, and the leading `in <total>` is dropped entirely if no phase
recorded.

## `sync <SCRIPT> --bench [N] [--force] [--keep-artefacts] [--debug | --release] [--gate NAME] [--commit REF]` - measure one

The measured shape. Same two-child spawn, but sæhrimnir is spawned
once and reused across `--bench` iterations (default 3), and the harness
binary runs N times with `BROKKR_MARKER_FIFO` set. Each iteration gets its own
`iter-K/harness/` subdir under the run dir; the script emits `SYNC_START` and
`SYNC_END` markers via the FIFO around the measured region (last `SYNC_START`
wins, first `SYNC_END` after it ends the span - so a warmup loop under the
same name is fine).

Best-of-N selection ranks every iteration on its `SYNC_START` -> `SYNC_END`
span, kept in microseconds; an iteration missing either marker fails the
measurement rather than being ranked on a whole-process wall. The best
iteration's `summary.json` (if the script writes one into
`BROKKR_HARNESS_ARTEFACT_DIR`) gets ingested as `meta.<key>` KvPair rows:
numeric values become Int/Real, strings become Text, nested objects/bools/null
are skipped. The run goes through the shared measurement loop and
`BenchHarness::record` every other measured command uses - one sæhrimnir
across iterations is the only sync-specific part - so `brokkr results
--compare` and the sidecar DB work the same as for pbfhogg/elivagar benches,
and the row carries the same provenance: every iteration's ranking value in
execution order, `cargo_profile` from the profile actually built (`dev` under
`[ratatoskr.harness] debug = true` or `--debug`), the configured features, the
`capture_env` snapshot, sidecar RunInfo naming the winning iteration's process,
and a measurement-start stamp so `prev.gap_seconds` excludes the run's own
duration. A failed, hung or interrupted run keeps its collected sidecar data
under the `dirty` alias - including an iteration whose child succeeded but
whose post-processing (the per-iteration log write, or reading
`summary.json`) then failed - and the dirty record carries the child's real
exit status. `--force` allows recording on a dirty git tree (rows land under
the `dirty` alias). A gated run also records the harness build's features
in `gate.db`, where the gate requires them to match the baseline's (see
`docs/commands/ratatoskr-gate.md`).

Each iteration is bounded by the script's frontmatter `ceiling:` (or
discover's default), the same watchdog the unmeasured run uses: an iteration
that outlives it has its process group SIGKILLed and the run fails, so a hung
harness cannot hold the global lock through a `--gate all` sweep.

### `--commit REF`

Builds and measures the harness from a persistent git worktree at `REF`
instead of the current tree, for retroactively benchmarking an older sync
implementation. Worktrees are reused across runs (their isolated target dir
survives) and removed by `brokkr clean --worktrees`.

**Only the harness build moves.** The script, the fixture, sæhrimnir, the
artefact dir, `results.db` and `gate.db` all stay anchored to the main
tree. This is dellingr's split-tree rule, applied for the same reason: a
comparison should vary the code under test and nothing else, so a
`--commit` run measures an old sync engine against *today's* test
definition rather than replaying an old test too. Sluggrs' `hotpath` has
no such rule because there the example and the renderer are both code;
here the `.lua` script is the test, not the subject.

Git state is read from the worktree, so the recorded commit describes what
was built. That also makes a `--commit` run inherently clean - a detached
worktree has nothing uncommitted in it - so `--force` is not needed and the
row is not filed under the `dirty` alias.

`--commit` composes with `--gate`. It is the intended way to bisect a
gate failure: re-run the same gate at successive refs and watch which one
first breaches. It also composes with `--as-baseline`, which is the
cleanest way to record one, since the commit being pinned is explicit and
the tree it came from is guaranteed clean. The gate's script-identity
check keeps working precisely because the script did not move with the
build; it would hard-error on a path mismatch otherwise.

Lockfile / kill semantics: sæhrimnir joins the auxiliary `mock_pids` set
for the lifetime of the bench, and each iteration's harness PID rotates
through `child_pid` (cleared between iterations so PID-recycling can't
trip `--hard`), so `brokkr lock` shows both and `brokkr kill --hard`
SIGKILLs every entry.

Cooperative `brokkr kill` (SIGTERM) is covered by one *phase* guard held
for the whole bench path - cargo build, sæhrimnir spawn and readiness
wait, every measured iteration and the gaps between them, mock teardown,
recording and the gate hook. `SigtermGuard`s nest, so the sidecar's own
guard around each iteration is an inner one: it neither clears a request
that is already pending nor uncovers the window when it drops. A request
already pending before an iteration spawns surfaces as `Interrupted`
instead of starting a run. Either way the request ends the same: the
current child is stopped, sæhrimnir is torn down through
`MockServer::shutdown`, and brokkr exits with `DevError::Interrupted`. A
request that lands after the last iteration still surfaces as
`Interrupted` once the run has recorded, so a `--gate all` sweep does not
start its next gate.

Cargo build and sæhrimnir use their own process groups during the bench
setup; each measured harness iteration also gets its own group under the
sidecar. The captured runner forwards terminal Ctrl-C and cooperative
shutdown to whichever tracked child is active.
