# Service test harness

`brokkr service` exists only in `project = "ratatoskr"`. It runs Lua-driven
tests against the ratatoskr harness binary, as `BINARY --test-harness
SCRIPT`. This page covers the index, running one script, running a cohort,
and fixture frontmatter.

`brokkr service` is one command with several shapes, following the same
bare-is-an-index convention as `sync`, `results`, `man` and `deps`:

| Invocation | What it does |
|---|---|
| `brokkr service` | list the discovered scripts |
| `brokkr service SCRIPT` | run one, PASS/FAIL |
| `brokkr service DIR` | run the cohort under a directory |
| `brokkr service --all` | run every discovered script |

The names `service-test`, `service-suite` and `service-list` are gone and
not aliased. Service runs are unmeasured and never recorded in results.db.

Clap's `requires` ties the flags that only make sense when running (`-N`,
`--keep-going`, `--keep-artefacts`, `--debug`, `--release`, `--profile`) to
the SCRIPT-or-`--all` group. So `brokkr service -N 5` fails at parse time
rather than silently listing. `--filter` and `--include-ignored` also
require `--all`.

For the `[ratatoskr.harness]` config block, the direct package and binary
build specification, see `brokkr man ratatoskr configuration`. The rest of
`brokkr man ratatoskr` covers the harness model: the Lua VM, the
`ServiceClient` userdata, the artefact writers and the runtime outcome.

## Index

Bare `service` walks `crates/app/tests/service-harness/**/*.lua`
recursively under the project root, parses a `-- key: value` frontmatter at
the top of each script, and prints a sorted table. It recognises these
fields:

- **`description`**: free text.
- **`expected`**: `pass` or `ignored`.
- **`ceiling`**: a per-script wall-clock backstop, with unit suffixes `ms`,
  `s`, `m` and `h`; a bare number is seconds. Default 60s.
- **`preserve_data_dir: on_success_too`**: keeps the artefact dir on
  success too; failures always keep it.
- **`fixture`**: the name of a saehrimnir fixture under `[ratatoskr]
  fixtures_dir`. It triggers a mock-server spawn and endpoint env-var
  injection at run time (see Fixtures). Give the `.toml` or `.lua`
  extension to disambiguate when both files exist.
- **`protocol`**: informational; saehrimnir binds every protocol listener
  regardless of this value. `discovery` labels WebFinger scripts, which use
  saehrimnir's JMAP endpoint rather than a separate listener.

Discovery fails, naming the script path and frontmatter line, on an unknown
lower-case annotation key, a duplicate field, a malformed recognized field
or an invalid value. It rejects the likely misspelling `fixtures` in favor
of the singular `fixture`. Ratatoskr's `@covers` and `measured` annotations
pass through. Ordinary comments after a blank line or a bare `--` are
outside the frontmatter block.

Empty-state output names the expected directory. Nested cohort dirs (`t1/`,
`extract/` and so on) are picked up automatically, and the displayed name is
the path relative to the script root, minus `.lua`.

## One script

`service SCRIPT_OR_DIR [-N COUNT] [--keep-going] [--keep-artefacts]
[--debug | --release | --profile NAME]` runs one service test script. When
the path is a directory under `crates/app/tests/service-harness/`, it is
shorthand for `service --all --filter REL/` scoped to that cohort: the same
code path and artefact layout, with `-N` as cohort cycles.

On the brokkr side, a run does this:

1. Checks the project gate and validates the script path.
2. Parses the frontmatter (`ceiling`, `preserve_data_dir`, `fixture`).
3. For a fixture-bearing script, preflights the saehrimnir binary and the
   fixture before the build.
4. Takes the lockfile and builds the harness directly from
   `[ratatoskr.harness]`: the configured package, binary, features and
   profile.
5. For a fixture-bearing script, spawns saehrimnir (see Fixtures).
6. Allocates an artefact dir under
   `.brokkr/ratatoskr/service/TEST/run-N/`.
7. Spawns `BINARY --test-harness SCRIPT` with `BROKKR_HARNESS_ARTEFACT_DIR`
   and `BROKKR_TEST_BIN_DIR` exported, plus the
   `RATATOSKR_TEST_*_ENDPOINT` family when a fixture is bound.
8. Drains stdout and stderr to `binary-stdout.log` and `binary-stderr.log`
   while a poll loop every 50 ms watches for the child's exit or the
   wall-clock ceiling (default 60s; override it with `-- ceiling: 60s`
   frontmatter).

When the ceiling fires, brokkr SIGKILLs the child and labels the run
`ceiling=SPELLING`. Otherwise the label follows the child's exit code or
signal.

brokkr writes `run.toml` with the run's reproduction metadata, including
the profile built. On success it deletes the artefact dir, unless you pass
`--keep-artefacts` or the script's frontmatter sets `preserve_data_dir:
on_success_too`; failures always keep it. brokkr records the invocation in
its general command history, but doesn't insert service results into
`results.db`.

The Lua VM, the `ServiceClient` userdata bindings, wait combinators and the
runtime-owned artefact writers live in ratatoskr. They are reached through
the `[ratatoskr.harness]` binary's `--test-harness` mode (`brokkr man
ratatoskr`, Lua test runtime). The writers produce `frames.jsonl`,
`events.jsonl`, `steps.jsonl`, `service.stderr`, `runtime-outcome.json`,
`proc-status.txt`, `proc-wchan.txt`, `proc-syscall.txt`, `proc-stack.txt`
and `data-dir/`.

The run takes these flags:

- **`-N COUNT`**: on a single script, repeats it with per-iteration status,
  a per-iteration artefact dir, `--keep-going` and exit-code aggregation. On
  a directory, runs the cohort COUNT times in order through the `--all` code
  path.
- **`--debug`, `--release`, `--profile NAME`**: force the harness build
  profile; `--profile` takes `dev`, `release` or a custom cargo profile. The
  default is release unless `[ratatoskr.harness] profile` or `debug = true`
  sets another, and the flags override the TOML. They are mutually
  exclusive.

The cross-cutting design is in ratatoskr's own `docs/harness/` directory:
`problem-statement.md`, `architecture.md` and `roadmap.md`.

## Cohort

`service --all [--filter SUBSTR] [-N COUNT] [--keep-going]
[--include-ignored] [--keep-artefacts] [--debug | --release | --profile
NAME]` runs every discovered script in sequence against one shared harness
build. `-N` runs the whole cohort COUNT times in order: 50 cycles over 11
`t1/` scripts is 550 runs. The trailing summary reports cohort totals plus a
per-script `pass/total` table.

The cohort discovers `crates/app/tests/service-harness/**/*.lua`, optionally
filters by substring against the relative name, then runs each script
through the same path the single shape uses: a per-script artefact dir, the
ceiling and `preserve_data_dir`. Scripts marked `expected: ignored` in the
frontmatter are skipped unless you pass `--include-ignored`.

Each run reports exactly one line, `[N/total] PASS NAME in Xms`, with a
`[cycle c/C]` prefix in soak mode. Nothing invariant repeats in the loop:
there is no per-script "running" line and no build echo, so you read a
550-run soak by scanning verdicts. The ceiling bounds how long a hung script
can stay silent. `brokkr lock` from another shell shows live progress and
the in-flight script's process (see `brokkr man hygiene lock`).

The cohort stops on the first failure by default, so the failing script's
artefacts are ready for triage fast. `--keep-going` runs every selected
script, and the trailing summary lists the failing names. This is
deliberately the opposite of `sync --all`, which keeps going by default;
`brokkr man sync cohort` gives the reasoning.

Empty-state messages distinguish "nothing discovered", "filter matched
none" and "all matches ignored", so the response tells you what to change.
The exit code is non-zero if any selected script failed.

## Fixtures

A service-harness script that needs a mock email-protocol server declares
the dependency in frontmatter:

```lua
-- fixture: jmap-small.toml
-- protocol: jmap
```

When `service` sees `-- fixture: NAME` on a selected script, brokkr does
this:

1. Resolves NAME against `[ratatoskr] fixtures_dir` before building. The
   string is passed through verbatim, so the explicit `.toml` or `.lua`
   extension disambiguates when both files exist.
2. Spawns saehrimnir through `[ratatoskr] mock_server_binary` with
   `--fixture PATH --readiness-file MOCK_DIR/readiness`.
3. Parses the readiness sentinel, one line per protocol, for per-protocol
   ports.
4. Injects the configured `RATATOSKR_TEST_*_ENDPOINT` env vars into the
   harness process: HTTP origins for jmap, graph, gmail, caldav, carddav,
   people and gcal, and `host:port` for imap and smtp, the same shape as
   `sync` and `sync --bench`.

After every dependent run, brokkr sends saehrimnir SIGTERM with the standard
1.5s drain budget, then escalates to SIGKILL if it overruns.

Mock artefacts (`stderr.log`, `readiness`) are written under
`.brokkr/ratatoskr/service-mock/FIXTURE_NAME/`, for single-script runs and
the cohort alike. Both service trees sit one level below
`.brokkr/ratatoskr/`, so a script stem or fixture name can never collide
with brokkr's own entries there (`sync/`, mock-serve's `mock/`, `gate.db`).

### Monitoring and stopping

`brokkr lock` from another shell shows progress as `run R/T` when the run
set has more than one entry: a soak with `--repeat` above 1, or any `--all`
invocation that runs several scripts or cycles. It also lists the harness
and every live saehrimnir among the holder's processes, each with its CPU
rate, RSS and age.

`brokkr kill --hard` SIGKILLs every recorded child and mock alongside
brokkr. A guard installed right after the lockfile catches `brokkr kill`
(SIGTERM) and is held through the build, run and teardown:

- the captured runner, used for both `cargo build` and the harness binary,
  forwards SIGTERM to whichever child is current, with a 1.5s budget
- the orchestrator then drains all mocks with the same budget
- any error mid-loop, not only an interrupt, takes the same drain path
  before propagating, so a failing cohort never falls through to a mock's
  immediate SIGKILL on drop

### Mock lifetime

For a single script, the mock spawns once before the soak begins and is
reused across all `-N` iterations of that script. Scripts without a
`fixture` line skip the spawn entirely, and need no `[ratatoskr]` mock
config.

A cohort (`--all` or a directory) runs scripts in discovery order, which is
alphabetical and so clusters cohorts under shared parent dirs. Fixture
lifecycle is cohort-scoped: each distinct fixture string spawns saehrimnir
at most once, lazily on first use. The mock stays alive for every remaining
script and cycle that shares that fixture, including no-fixture scripts
running in between.

brokkr injects the endpoint env vars (`RATATOSKR_TEST_*_ENDPOINT`) only into
scripts that declare that fixture. No-fixture scripts run as if no mock
exists, even when other mocks are alive in the background. Fixture isolation
is the script's responsibility, for example hitting the mock's
`/test/PROTOCOL/reset` or `DELETE /test/.../submissions` at the start of the
test; brokkr doesn't recycle the process between scripts.

At cohort end brokkr drains every spawned mock gracefully, with SIGTERM and
the standard 1.5s budget per mock before SIGKILL, whether the loop
succeeded or errored. When any selected script needs a fixture, the cohort
validates `[ratatoskr] mock_server_binary` and `fixtures_dir` up front, so
a missing config fails before the cargo build starts.
