# Sync and mock-serve

`brokkr sync` and `brokkr mock-serve` exist only in `project = "ratatoskr"`.
They spawn saehrimnir, an external mock email server, alongside the
ratatoskr harness binary, so sync scripts can exercise real protocol stacks.
This page covers `mock-serve`, then each shape of `sync`: the index, the
cohort, one script, and measuring one.

`brokkr sync` is one command with several shapes, following the same
bare-is-an-index convention as `results`, `man` and `deps`:

| Invocation | What it does |
|---|---|
| `brokkr sync` | list the discovered scripts |
| `brokkr sync SCRIPT` | run one, PASS/FAIL |
| `brokkr sync --all` | run every discovered script |
| `brokkr sync SCRIPT --bench [N]` | measure one (default N=3) |
| `brokkr sync --gate all --bench [N]` | sweep every configured gate |

The names `sync-list`, `sync-smoke` and `sync-bench` are gone and not
aliased. results.db migration v17 to v18 rewrote rows recorded by
`sync-bench` to `sync`, in both the `command` column and the `brokkr_args`
argv, since `--compare` keys on both, so those rows still pair with new
runs. `gate.db` rows never had a command name, so every pinned baseline is
unaffected.

Clap's `requires` enforces the flags that only make sense while measuring
(`--bench`, `--force`, `--gate`, `--as-baseline`, `--commit`). So `brokkr
sync --gate x` fails at parse time rather than silently listing.

For the ratatoskr project config (`[ratatoskr]`, `[ratatoskr.harness]` and
`[ratatoskr.gate.*]`), see `brokkr man ratatoskr configuration`. The rest of
`brokkr man ratatoskr` covers the harness model and the saehrimnir contract.

## mock-serve

`mock-serve --fixture NAME` is a manual exploration tool. It reads
`[ratatoskr] mock_server_binary` and `[ratatoskr] fixtures_dir` from
`brokkr.toml`, both required, then does this:

1. Resolves the fixture to `FIXTURES_DIR/NAME.toml` or
   `FIXTURES_DIR/NAME.lua`, whichever exists. Both existing is an error;
   pass the name with its extension to disambiguate.
2. Spawns saehrimnir with `--fixture PATH --readiness-file
   .brokkr/ratatoskr/mock/readiness`, its stdio inherited so logs show live.
3. Polls for the readiness sentinel every 50 ms, for up to 10s.
4. Parses the sentinel's `NAME PORT` content, one line per protocol, and
   prints the per-protocol HTTP or `host:port` endpoints.
5. Loops until SIGINT or SIGTERM. On a signal, it sends the child SIGTERM,
   grants 1.5s, then escalates to SIGKILL.

If saehrimnir exits before writing the sentinel (a fixture-validation error,
a port in use), the spawn-side error is reported with the captured stderr
already on your terminal.

brokkr installs the signal handler before saehrimnir spawns, not after
readiness returns, and the readiness wait polls the same flag. A `brokkr
kill` during the readiness window therefore aborts cleanly with the child
reaped, and can't orphan saehrimnir.

Managed mock children use the shared service spawn policy: OOM marking,
parent-death termination, the orphan-reap token for descendants, and the
hold capability applied after the caller's environment.

brokkr doesn't build saehrimnir: the binary must already exist at
`mock_server_binary`.

## Index

Bare `sync` walks `[ratatoskr] sync_script_dir` (default
`crates/app/tests/sync-harness`), parses each script's top-of-file
frontmatter and prints a sorted table. The frontmatter fields are
`description`, `expected`, `fixture`, `protocol`, `ceiling` and
`preserve_data_dir`. The index is pure brokkr: it spawns neither saehrimnir
nor the harness binary.

Every sync script must declare a fixture. Discovery fails, naming the script
path and frontmatter line, on any of these:

- an unknown lower-case annotation key
- a duplicate field
- a malformed recognized field or an invalid value
- a missing fixture

Ratatoskr's `@covers` and `measured` annotations pass through. Ordinary
comments after a blank line or a bare `--` are outside the frontmatter
block. The informational `protocol: discovery` label describes WebFinger
tests that use the JMAP endpoint; it doesn't name a separate saehrimnir
listener.

Empty-state output names the expected directory and notes that the cohort
may not exist yet.

## Cohort

`sync --all [--filter SUB] [--include-ignored]` runs every discovered script
unmeasured, in discovery order. It is the sync-side counterpart to `service
--all`, and a cohort means the same thing in both families. `--filter` is a
substring match against the discovered name. Scripts whose frontmatter says
`expected: ignored` are skipped unless you pass `--include-ignored`.

brokkr builds the harness once, before the loop, since the cohort varies the
script and never the binary. Each script then reports exactly one line:

```text
[ratatoskr] account-verify-imap-success: PASS in 400ms (mock 100ms,
  harness 400ms, shutdown 0ms)
[ratatoskr] bifrost-consumer-lag-recovery: FAIL - config: sync: script ...
  has no `-- fixture: <NAME>` frontmatter line. ...
```

The example wraps each line for width; brokkr prints each on one line.

A failure that got far enough to allocate an artefact dir adds one
`artefacts preserved at ...` line. The `brokkr clean` hint prints once at
the end, not per failure. A 160-script sweep is therefore about 160 lines,
plus a header and a `sync cohort: N/M passed` summary.

Discovery validates every script and resolves each selected fixture before
the lock and the harness build, so a missing `fixture:` line or a fixture
typo can't fail halfway through the cohort.

The cohort keeps going after a failure by default, unlike `service --all`,
which stops on the first. Each sync script owns its own `run-N/` artefact
dir, so a later failure can't overwrite an earlier one's triage material.
There is nothing to protect by stopping, and a cohort is most useful when it
reports the whole blast radius. The command exits non-zero if any script
failed, and the exit error names the failures without repeating the
messages already printed inline.

Keep-going covers script failures, not interrupts. A `brokkr kill` or Ctrl-C
stops the sweep:

- the in-flight script reports `INTERRUPTED`
- no further script starts; brokkr checks the shutdown request before each
  spawn as well as on each result
- the cohort prints how far it got, and brokkr exits as interrupted

saehrimnir's readiness wait polls the same request, so an interrupt during
mock startup doesn't wait out the readiness budget.

The cohort holds the global lock for the whole sweep, not per script.
Another brokkr invocation can't interleave a build or bench between two
scripts, and the sweep can't stall midway waiting behind one. With the build
before the loop, this is the path's only acquire. `--gate all` exercises the
lockfile's in-process re-entrancy, since its swept members still acquire
internally (see `brokkr man gate`).

`--all` conflicts with SCRIPT and with `--bench`. Measuring a cohort would
interleave N iterations of one script with the saehrimnir spawn of the next,
and best-of-N across different scripts means nothing. To measure across the
cohort, use `--gate all`, which is scoped to the gates that have pinned
baselines to compare against.

## One script

`sync SCRIPT [--keep-artefacts] [--debug | --release | --profile NAME]` runs
one script with two children. brokkr does this:

1. Validates `[ratatoskr.harness]`, `[ratatoskr] mock_server_binary` and
   `[ratatoskr] fixtures_dir`.
2. Parses the script's `-- fixture: NAME` frontmatter and resolves the
   fixture, before building.
3. Takes the lockfile and builds the configured harness directly.
4. Allocates `.brokkr/ratatoskr/sync/TEST/run-N/` with `harness/` and
   `mock/` subdirs.
5. Spawns saehrimnir with `--fixture PATH --readiness-file mock/readiness`,
   its stderr piped to `mock/stderr.log`, and parses the readiness sentinel
   for per-protocol ports.
6. Spawns `HARNESS_BINARY --test-harness SCRIPT` with the environment
   described after this list.

The harness environment has `BROKKR_HARNESS_ARTEFACT_DIR` and
`BROKKR_TEST_BIN_DIR`, plus one `RATATOSKR_TEST_PROTO_ENDPOINT` per protocol
whose env-var spelling is configured under `[ratatoskr]`. The endpoints are
HTTP origins for jmap, graph, gmail, caldav, carddav, people and gcal, and
`host:port` for imap and smtp.

### Processes and stopping

During the run brokkr publishes both PIDs into the lockfile, so `brokkr kill
--hard` reaches both. saehrimnir joins the auxiliary `mock_pids` set, and the
harness binary takes the spawning thread's child slot. `brokkr lock` from
another shell finds both among the holder's processes, by its launch token
rather than the lockfile, and shows each one's CPU rate, RSS and age.

Tracked saehrimnir, cargo-build and harness children run in their own
process groups across service and sync. Deadline, cooperative shutdown and
`--hard` cleanup target the child group, including helper descendants. The
captured runner bridges terminal Ctrl-C and `brokkr kill`:

1. It forwards SIGTERM to the active group.
2. It waits up to 1.5s for the group leader to exit.
3. It SIGKILLs the group, whether or not the leader exited, so a member that
   outlived the SIGTERM doesn't outlive the stop.

After the harness exits, brokkr clears its child slot. A slot left behind is
never a hazard, since `--hard` signals only the recorded starttime and skips
a target that is provably gone. Every mock is unregistered when its shared
lifecycle shuts it down. If a later step fails, the service cohort drains its
mocks before propagating the original failure.

### Outcome

After the harness exits, brokkr sends saehrimnir SIGTERM with the standard
1.5s budget, then escalates to SIGKILL. PASS or FAIL follows the harness exit
code. A FAIL keeps the artefact dir with these:

- `run.toml`, the top-level metadata: brokkr version, script, binary,
  features, profile, the harness exit code, signal and elapsed time, git
  state, and a `[mock]` table with the mock outcome
- the harness's own artefacts
- the captured mock stderr

brokkr serializes `run.toml` rather than formatting it as a string, so a
quote or backslash in a path can't corrupt it. Its field names match
`service`'s `run.toml`.

The PASS or FAIL line has a phase summary, so you can decompose a slow run
at a glance: `PASS in 3.7s (mock 400ms, harness 3.2s, shutdown 100ms)`.
Phases that didn't run, such as a spawn-side failure before the harness
started, are omitted. The leading `in TOTAL` is dropped if no phase was
recorded.

## bench

`sync SCRIPT --bench [N] [--force] [--keep-artefacts] [--debug | --release |
--profile NAME] [--gate NAME] [--commit REF]` measures one script. It uses
the same two-child spawn, but brokkr spawns saehrimnir once and reuses it
across the `--bench` iterations (default 3). The harness binary runs N times
with `BROKKR_MARKER_FIFO` set, and each iteration gets its own
`iter-K/harness/` subdir under the run dir.

The script emits `SYNC_START` and `SYNC_END` markers through the FIFO around
the measured region. The last `SYNC_START` wins, and the first `SYNC_END`
after it ends the span, so a warmup loop under the same name is fine.

Best-of-N selection ranks every iteration on its `SYNC_START` to `SYNC_END`
span, kept in microseconds. An iteration missing either marker fails the
measurement rather than being ranked on a whole-process wall.

If the script writes a `summary.json` into `BROKKR_HARNESS_ARTEFACT_DIR`,
brokkr ingests the best iteration's as `meta.KEY` values. For `results.db`,
top-level numbers become Int or Real and strings become Text; booleans,
nulls, arrays and nested objects are omitted. The gate row keeps booleans,
so gate selectors can treat them as `1` or `0`.

### Recording

The run uses the shared measurement loop. A completed clean-tree measurement
is recorded in `results.db` with the usual provenance:

- every iteration's ranking value, in execution order
- `cargo_profile` from the profile built: `dev` under `[ratatoskr.harness]
  debug = true` or `--debug`, `cargo:NAME` under a custom `profile` or
  `--profile`; `brokkr results --compare` pairs rows only on equal profiles
- the configured features
- the `capture_env` snapshot
- sidecar RunInfo for the winning iteration's process
- a measurement-start stamp, so `prev.gap_seconds` excludes the run's own
  duration

A failed, hung or interrupted run has no `results.db` row. You can still
reach its collected sidecar data by the id its `[sidecar]` line names, or
with `brokkr sidecar dirty` while it is the latest such run. `--force`
permits measuring a dirty git tree but doesn't insert that run into
`results.db`; its sidecar data is reached the same way.

A completed gated bench also writes a row to `gate.db`, with a `dirty` flag
when applicable, even if a metric rule then fails. A harness failure before
the gate hook writes no gate row; see `brokkr man gate`.

The script's frontmatter `ceiling:`, or discovery's default, bounds each
iteration, the same watchdog the unmeasured run uses. An iteration that
outlives it has its process group SIGKILLed and the run fails, so a hung
harness can't hold the global lock through a `--gate all` sweep.

### commit

`--commit REF` builds and measures the harness from a persistent git
worktree at `REF` instead of the current tree, to benchmark an older sync
implementation. brokkr reuses worktrees across runs, so their isolated
target dir survives, and `brokkr clean --worktrees` removes them.

Only the harness build moves. The script, the fixture, saehrimnir, the
artefact dir, `results.db` and `gate.db` all stay anchored to the main tree.
This is dellingr's split-tree rule, for the same reason: a comparison should
vary the code under test and nothing else. A `--commit` run measures an old
sync engine against today's test definition rather than replaying an old
test too. Sluggrs' `hotpath` has no such rule because there the example and
the renderer are both code; here the `.lua` script is the test, not the
subject.

The build spec stays too. brokkr passes `[ratatoskr.harness]`'s `package`,
`binary` and `features`, and the profile name, to cargo as written, and the
worktree and its effective cargo configuration decide what they mean. A
custom profile of the same name can be defined differently at `REF`, and an
empty feature list means that package's defaults there.

A `REF` that lacks a name the spec uses fails its build: one from before the
harness package existed under that name, or before the custom profile was
defined. `--profile`, `--debug` or `--release` replaces the profile for the
run. The row records the profile and features, not the package or binary.

brokkr reads git state from the worktree, so the recorded commit describes
what was built. That also makes a `--commit` run clean, since a detached
worktree has nothing uncommitted in it. `--force` isn't needed, and the row
isn't filed under the `dirty` alias. Passing `--force` anyway warns that it
has no effect and the row will be stored, as on any clean tree.

`--commit` composes with `--gate`, which is the intended way to bisect a
gate failure: re-run the same gate at successive refs and watch which one
first breaches. It also composes with `--as-baseline`, the cleanest way to
record a baseline, since the commit being pinned is explicit and its tree is
clean. The gate's script-identity check keeps working because the script
didn't move with the build; it would refuse on a path mismatch otherwise.

### Stopping a bench

saehrimnir joins the auxiliary `mock_pids` set for the lifetime of the
bench. Each iteration's harness PID rotates through the published child
slot, cleared between iterations so PID recycling can't trip `--hard`. So
`brokkr kill --hard` SIGKILLs every entry, and `brokkr lock` shows both
among the holder's processes.

One phase guard covers cooperative `brokkr kill` (SIGTERM) for the whole
bench path: the cargo build, the saehrimnir spawn and readiness wait, every
measured iteration and the gaps between them, mock teardown, recording and
the gate hook. Guards nest, so the sidecar's own guard around each iteration
is an inner one. It neither clears a request that is already pending nor
uncovers the window when it drops.

A request already pending before an iteration spawns stops the bench as
interrupted instead of starting a run. Either way the request ends the same:
brokkr stops the current child, tears saehrimnir down and exits as
interrupted. A request that arrives after the last iteration still ends the
bench as interrupted once the run has recorded, so a `--gate all` sweep
doesn't start its next gate.

The cargo build and saehrimnir run in their own process groups during the
bench setup, and each measured harness iteration also gets its own group
under the sidecar. The captured runner forwards terminal Ctrl-C and
cooperative shutdown to whichever tracked child is active.
