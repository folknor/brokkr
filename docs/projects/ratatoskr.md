# ratatoskr project notes

`project = "ratatoskr"` in `brokkr.toml`. Multi-crate workspace.

## Configuration

Project-specific settings live under `[ratatoskr]` in the project
`brokkr.toml`:

- `[ratatoskr.harness]` names the cargo package (`package`), optional binary
  target (`binary`, defaulting to the package name), optional cargo
  `features`, and optional `debug` profile default. Orchestration builds use
  this spec directly; they do not select a `[[check]]` sweep.
- `mock_server_binary` points to the sæhrimnir executable and `fixtures_dir`
  to its fixture files. Absolute paths stay absolute; relative paths resolve
  from the project root. Both are required when a selected service or sync
  script uses a fixture, and by `mock-serve`.
- `test_endpoint_env_<protocol>` settings name the environment variable
  brokkr sets for each protocol's endpoint. The registered protocols are
  `jmap`, `imap`, `smtp`, `graph`, `gmail`, `caldav`, `carddav`, `people`,
  and `gcal`. These variables are injected for fixture-backed service and
  sync runs. HTTP protocols receive an `http://host:port` origin; IMAP and
  SMTP receive `host:port`.
- `sync_script_dir` optionally selects the sync script directory. Relative
  paths resolve from the project root; the default is
  `crates/app/tests/sync-harness`.
- `[ratatoskr.gate.<name>]` configures a gate's script, baseline identity,
  and metric rules. See
  [the sync gate guide](../commands/ratatoskr-gate.md) for its full shape.

The schema-universal fields are documented in `docs/brokkr.toml.md`; the
ratatoskr-specific config sections are described here so service, sync, and
gate users can find them in one place.

## Two test harnesses

ratatoskr exercises two separate harness flows from brokkr:

1. **Service-subprocess harness** (`service`: bare lists, `<SCRIPT>`
   runs one, `--all` runs the cohort) - lua-driven tests against the
   ratatoskr binary's `--test-harness` mode. See
   `docs/commands/service.md`.
2. **Sync orchestration** (`sync`, `mock-serve`) - two-child
   orchestration with sæhrimnir (mock email
   server) + the ratatoskr harness binary. See `docs/commands/sync.md`.

Both share the harness build pipeline through `[ratatoskr.harness]`. The
block is a **self-contained build spec** - it names the cargo package to
build (`package`), optionally an override `[[bin]]` to spawn (`binary`,
defaults to `package`), optional cargo `features`, and an optional
`debug = true` to default the orchestration commands to the dev profile.
It does **not** reference `[[check]]`: orchestration builds are
intentionally outside the everyday `brokkr check` sweep matrix, so adding
an orchestration command never widens the per-crate feature graph that
`brokkr check` and `brokkr check -p <crate>` walk.

See `RatatoskrConfig` rustdoc in
`src/config_parts/project_config/ratatoskr.rs` and `HarnessConfig` in
`src/config.rs` for the fields. The orchestration commands accept `--debug` /
`--release` (mutually exclusive) to override the toml on a per-invocation
basis.

A stale `[ratatoskr.harness].sweep` field from the pre-decoupling shape
is rejected at parse time with a migration message: drop the matching
`[[check]] name = "<sweep>"` entry and fold its `features` /
`build_packages` directly into `[ratatoskr.harness]` as `features` /
`package`.

## sæhrimnir contract

sæhrimnir is an external repo (sibling checkout, see
`mock_server_binary` / `fixtures_dir` in `[ratatoskr]` config). It writes a
readiness sentinel naming each protocol's listening port, one line per
protocol, which
brokkr parses via `parse_sentinel` in `src/ratatoskr/saehrimnir.rs`.

Spawn lifecycle:
1. brokkr spawns sæhrimnir with `--fixture <PATH> --readiness-file <PATH>`.
2. brokkr polls the readiness file (50ms cadence, 10s budget).
3. brokkr parses endpoints, exports them as
   `RATATOSKR_TEST_<PROTO>_ENDPOINT` env vars per `[ratatoskr]` config.
4. brokkr spawns the harness binary with those env vars set.
5. After the harness exits: SIGTERM sæhrimnir, 1.5s budget, then SIGKILL.

Auto-build of sæhrimnir is not yet wired - the binary must already exist at
`mock_server_binary`.

## Fixture resolution

Fixtures live in `<fixtures_dir>` and may be `.toml` or `.lua`. brokkr's
`resolve_fixture` (in `src/ratatoskr/saehrimnir.rs`) picks whichever exists
for a given stem; if both exist for the same stem (e.g. `jmap-small.toml`
and `jmap-small.lua`), the user must write the name with its extension to
disambiguate. Bare stems with both files present errors out.

## Lua test runtime

The Lua VM, `ServiceClient` userdata bindings, wait combinators, and
runtime-owned artefact writers (`frames.jsonl`, `events.jsonl`,
`steps.jsonl`, `service.stderr`, `runtime-outcome.json`,
`proc-{status,wchan,syscall,stack}.txt`, `data-dir/`) live in ratatoskr's
`app` crate behind `app --test-harness`, gated by the `test-helpers` feature.

`parent_death_helper` is a bin target inside the `app` package
(`crates/app/src/bin/parent_death_helper.rs`), not a separate cargo package -
`cargo build -p app` builds it as a side effect, and `BROKKR_TEST_BIN_DIR`
points at the directory containing both binaries.

## Artefact layout

- `service <SCRIPT>`: `.brokkr/ratatoskr/service/<test>/run-N/` with
  `binary-stdout.log` / `binary-stderr.log` / `run.toml` plus
  runtime-emitted artefacts; a fixture's sæhrimnir state goes to
  `.brokkr/ratatoskr/service-mock/<fixture>/`. Neither sits directly in
  `.brokkr/ratatoskr/`, where a script named `mock` or `sync` would write
  into brokkr's own trees.
- `mock-serve`: `.brokkr/ratatoskr/mock/` (`readiness`).
- `sync <SCRIPT>`: `.brokkr/ratatoskr/sync/<test>/run-N/` with `harness/` and
  `mock/` subdirs (`mock/readiness`, `mock/stderr.log`).
- `sync <SCRIPT> --bench N`: `.brokkr/ratatoskr/sync/<test>/run-N/iter-K/harness/` per
  iteration; the best iteration's `summary.json` is ingested as KvPair rows.

Per-run cleanup is the runtime's job: `ArtefactDir::finalize_success`
removes the `run-N/` dir on green runs (unless `--keep-artefacts` or
`preserve_data_dir = on_success_too` was set), `finalize_failure`
preserves it, and a panic / early return defaults to preserve so
diagnostics are never lost. To sweep accumulated dirs from past
failures, run `brokkr clean` - on ratatoskr projects it removes the
artefact and `mock/` directories under `.brokkr/ratatoskr/` (it acquires
the project lock first, so concurrent harness runs and `mock-serve` are
not affected). It spares every *file* at that level, `gate.db`
especially: that store holds the only copy of the numbers a gate
baseline pins by UUID, so it is out of clean's scope like `results.db`.
See `docs/commands/ratatoskr-gate.md`.

## Cross-cutting design

Cross-cutting design lives in the ratatoskr repo at
`<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`.
