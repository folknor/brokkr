# ratatoskr project

This page covers the ratatoskr project, a multi-crate workspace selected by
`project = "ratatoskr"` in `brokkr.toml`. It describes the `[ratatoskr]`
config, the harness build, the two test harness flows, the saehrimnir mock
server contract, fixture resolution, the Lua runtime boundary and the
artefact layout. The schema-universal fields are in `brokkr man config`;
the ratatoskr-specific sections are gathered here so service, sync and gate
users find them in one place.

## Configuration

Project-specific settings live under `[ratatoskr]` in the project
`brokkr.toml`:

- **`[ratatoskr.harness]`**: the build spec for the harness binary. See
  "Harness build" below.
- **`mock_server_binary`, `fixtures_dir`**: the saehrimnir executable and its
  fixture files. Absolute paths stay absolute; relative paths resolve from the
  project root. Both are required by `mock-serve`, and by any selected service
  or sync script that uses a fixture.
- **`test_endpoint_env_PROTOCOL`**: the environment variable brokkr sets for
  that protocol's endpoint. brokkr injects these variables for
  fixture-backed service and sync runs.
- **`sync_script_dir`**: optional, the sync script directory. A relative path
  resolves from the project root. The default is
  `crates/app/tests/sync-harness`.
- **`[ratatoskr.gate.NAME]`**: a gate's script, baseline identity and metric
  rules. `brokkr man gate` gives its full shape.

The registered protocols are `jmap`, `imap`, `smtp`, `graph`, `gmail`,
`caldav`, `carddav`, `people` and `gcal`. HTTP protocols receive an
`http://host:port` origin; IMAP and SMTP receive `host:port`.

## Harness build

The `[ratatoskr.harness]` table is a self-contained build spec that both
harness flows share. Its keys:

- **`package`**: the cargo package to build.
- **`binary`**: optional, the `[[bin]]` target to spawn. Without it, brokkr
  uses the package's sole bin, else the bin named like the package, else the
  build refuses.
- **`features`**: optional cargo features.
- **`debug = true`**: optional, defaults the orchestration commands to the
  dev profile.
- **`profile = "NAME"`**: optional, defaults the orchestration commands to
  any cargo profile (`dev`, `release`, or a custom `[profile.NAME]`, built
  into `TARGET/NAME/`). It wins over `debug`, with the rules of
  `[bin] profile` in `brokkr man config`.

With neither `debug` nor `profile` set, the orchestration commands build
release. The orchestration commands accept `--debug`, `--release` or
`--profile NAME`, mutually exclusive, to override the toml for one
invocation. Every run's `run.toml` records the `profile` the harness was
built under.

The harness spec doesn't reference `[[check]]`, and orchestration builds
don't select a `[[check]]` sweep. They stay outside the everyday
`brokkr check` sweep matrix, so adding an orchestration command never widens
the per-crate feature graph that `brokkr check` and `brokkr check -p CRATE`
walk.

brokkr rejects a `[ratatoskr.harness].sweep` field at parse time with a
migration message. To migrate, drop the matching `[[check]] name = "SWEEP"`
entry, and fold its `features` and `build_packages` into
`[ratatoskr.harness]` as `features` and `package`.

## Test harnesses

brokkr drives two separate ratatoskr harness flows:

1. **Service-subprocess harness** (`service`): Lua-driven tests against the
   ratatoskr binary's `--test-harness` mode. Bare `service` lists the
   scripts, `service SCRIPT` runs one, and `--all` runs the cohort. See
   `brokkr man service`.
2. **Sync orchestration** (`sync`, `mock-serve`): two-child orchestration of
   the saehrimnir mock email server and the ratatoskr harness binary. See
   `brokkr man sync`.

## saehrimnir

saehrimnir is the mock email server, an external repo in a sibling checkout
located by `mock_server_binary` and `fixtures_dir`. It writes a readiness
sentinel naming each protocol's listening port, one line per protocol, which
brokkr reads to find the endpoints.

A run follows this lifecycle:

1. brokkr spawns saehrimnir with `--fixture PATH --readiness-file PATH`.
2. brokkr polls the readiness file every 50 ms, for up to 10s.
3. brokkr parses the endpoints and exports each as the variable its
   `test_endpoint_env_PROTOCOL` key names, such as
   `RATATOSKR_TEST_PROTO_ENDPOINT`.
4. brokkr spawns the harness binary with those variables set.
5. After the harness exits, brokkr sends saehrimnir SIGTERM, waits up to
   1.5s, then sends SIGKILL.

brokkr doesn't build saehrimnir: the binary must already exist at
`mock_server_binary`.

## Fixture resolution

Fixtures live in `fixtures_dir` and may be `.toml` or `.lua`. Given a bare
stem, brokkr picks whichever of the two exists. When both exist for one stem (`jmap-small.toml` and
`jmap-small.lua`), a bare stem is an error, and you must write the name with
its extension.

## Lua test runtime

The Lua runtime lives in ratatoskr, not brokkr. It includes the Lua VM, the
`ServiceClient` userdata bindings, the wait combinators and the
runtime-owned artefact writers: `frames.jsonl`, `events.jsonl`,
`steps.jsonl`, `service.stderr`, `runtime-outcome.json`, the
`proc-status.txt`, `proc-wchan.txt`, `proc-syscall.txt` and `proc-stack.txt`
files, and `data-dir/`.

brokkr reaches the runtime only through the binary `[ratatoskr.harness]`
builds (`package` and `binary` name it), run in its
`--test-harness SCRIPT` mode. Which ratatoskr crate holds the code, and
whether a cargo feature gates it, is ratatoskr's business; brokkr doesn't
build against either.

The runtime finds anything else it spawns itself. brokkr sets
`BROKKR_TEST_BIN_DIR` to the directory holding the binary it built. The
runtime can then start that same binary in another mode, such as its
parent-death helper, rather than look for a separately built one.

## Artefact layout

Each command writes its artefacts under `.brokkr/ratatoskr/`:

- **`service SCRIPT`**: `service/TEST/run-N/`, with `binary-stdout.log`,
  `binary-stderr.log`, `run.toml` and the runtime-emitted artefacts. A
  fixture's saehrimnir state goes to `service-mock/FIXTURE/`.
- **`mock-serve`**: `mock/`, holding `readiness`.
- **`sync SCRIPT`**: `sync/TEST/run-N/`, with `harness/` and `mock/` subdirs
  (`mock/readiness`, `mock/stderr.log`).
- **`sync SCRIPT --bench N`**: `sync/TEST/run-N/iter-K/harness/` per
  iteration. brokkr ingests the best iteration's `summary.json` as KvPair
  rows.

Neither service tree sits directly in `.brokkr/ratatoskr/`, where a script
named `mock` or `sync` would write into brokkr's own trees.

### Cleanup

brokkr decides at the end of each run whether its `run-N/` dir stays:

- A green run's dir is removed, unless you passed `--keep-artefacts` or the
  script's frontmatter sets `preserve_data_dir = on_success_too`.
- A failed run's dir is kept.
- A panic or early return keeps the dir, so diagnostics survive.

To sweep dirs that past failures left, run `brokkr clean`. In a ratatoskr
project it removes the artefact and `mock/` directories under
`.brokkr/ratatoskr/`. It takes the project lock first, so concurrent harness
runs and `mock-serve` aren't affected. It spares every file at that level,
`gate.db` especially: that store holds the only copy of the numbers a gate
baseline pins by UUID, so it's out of clean's scope, like `results.db`. See
`brokkr man gate`.

## Design documents

ratatoskr's cross-cutting harness design lives in the ratatoskr repo, in
`docs/harness/problem-statement.md`, `architecture.md` and `roadmap.md`.
