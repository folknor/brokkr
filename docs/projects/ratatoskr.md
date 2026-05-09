# ratatoskr project notes

`project = "ratatoskr"` in `brokkr.toml`. Multi-crate workspace.

## Two test harnesses

ratatoskr exercises two separate harness flows from brokkr:

1. **Service-subprocess harness** (`service-test`, `service-suite`,
   `service-list`) - lua-driven tests against the ratatoskr binary's
   `--test-harness` mode. See `docs/commands/service.md`.
2. **Sync orchestration** (`mock-serve`, `sync-list`, `sync-smoke`,
   `sync-bench`) - two-child orchestration with sæhrimnir (mock email
   server) + the ratatoskr harness binary. See `docs/commands/sync.md`.

Both share the harness build pipeline through `[ratatoskr.harness]` (which
`[[check]]` sweep to build, which package's binary to spawn, and an optional
`debug = true` to default the orchestration commands to the dev profile).
See `docs/brokkr.toml.md` for the config block, and `RatatoskrConfig` /
`HarnessConfig` rustdoc in `src/config.rs` for parse-time validation.
The orchestration commands accept `--debug` / `--release` (mutually exclusive)
to override the toml on a per-invocation basis.

## sæhrimnir contract

sæhrimnir is an external repo (sibling checkout, see
`mock_server_binary` / `fixtures_dir` in `[ratatoskr]` config). It writes a
five-line readiness sentinel naming per-protocol listening ports, which
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

- `service-test`: `.brokkr/ratatoskr/<test>/run-N/` with `binary-stdout.log`
  / `binary-stderr.log` / `run.toml` plus runtime-emitted artefacts.
- `sync-smoke`: `.brokkr/ratatoskr/sync/<test>/run-N/` with `harness/` and
  `mock/` subdirs (`mock/readiness`, `mock/stderr.log`).
- `sync-bench`: `.brokkr/ratatoskr/sync/<test>/run-N/iter-K/harness/` per
  iteration; the best iteration's `summary.json` is ingested as KvPair rows.

Per-run cleanup is the runtime's job: `ArtefactDir::finalize_success`
removes the `run-N/` dir on green runs (unless `--keep-artefacts` or
`preserve_data_dir = on_success_too` was set), `finalize_failure`
preserves it, and a panic / early return defaults to preserve so
diagnostics are never lost. To sweep accumulated dirs from past
failures, run `brokkr clean` - on ratatoskr projects it removes the
whole `.brokkr/ratatoskr/` tree (it acquires the project lock first,
so concurrent harness runs and `mock-serve` are not affected).

## Cross-cutting design

Cross-cutting design lives in the ratatoskr repo at
`<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`.
