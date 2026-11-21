# ratatoskr internals

This page records how brokkr's ratatoskr support is built: where the config
schema lives, the protocol registry, the saehrimnir seams, run artefact
retention, and why orchestration builds stay out of the check matrix. How
the commands and config are used is in `docs/projects/ratatoskr.md`,
`docs/commands/service.md`, `docs/commands/sync.md` and
`docs/commands/ratatoskr-gate.md`.

## Config schema

The `[ratatoskr]` fields are documented in the `RatatoskrConfig` rustdoc in
`src/config_parts/project_config/ratatoskr.rs`, and the `[ratatoskr.harness]`
fields in `HarnessConfig` in `src/config_parts/schema.rs`. The rustdoc is the
field-level reference; the user docs describe behavior.

## Protocol registry

The protocol list is spelled once, in the `ratatoskr_protocols!` registry in
`src/config_parts/project_config/ratatoskr.rs`. That registry drives:

- the `test_endpoint_env_PROTOCOL` config keys
- the readiness-sentinel parse
- the endpoint shapes (an `http://host:port` origin or a bare `host:port`)
- the frontmatter `protocol` values

Adding a protocol means one registry entry; nothing else may restate the
list.

## saehrimnir seams

`src/ratatoskr/saehrimnir.rs` owns brokkr's side of the mock server:

- **`parse_sentinel`**: parses the readiness file saehrimnir writes, one
  line per protocol naming its listening port.
- **`resolve_fixture`**: maps a fixture name to a `.toml` or `.lua` file in
  `fixtures_dir`, and refuses a bare stem that matches both.

## Artefact retention

Run directories are `ArtefactDir` handles from `src/artefacts.rs`, which
takes a caller-supplied parent (`.brokkr/ratatoskr/...` for ratatoskr) and
allocates `TEST/run-N/`, N one past the highest present. The retention
policy lives in the finalizers:

- `ArtefactDir::finalize_success` removes the directory, unless the caller
  asked to keep artefacts at allocation (`--keep-artefacts`, or
  `preserve_data_dir = on_success_too`).
- `ArtefactDir::finalize_failure` keeps it.
- Dropping the handle does nothing, so a panic or early return keeps it.

Only `finalize_success` ever deletes. Losing diagnostics by default is the
worst failure mode for a test harness, so any new exit path must keep that
property; `brokkr clean` is the sweep for what accumulates.

## Orchestration builds and check

`[ratatoskr.harness]` is a self-contained build spec and doesn't reference a
`[[check]]` sweep. That separation is deliberate: adding an orchestration
command must never widen the per-crate feature graph that `brokkr check` and
`brokkr check -p CRATE` walk. A `[ratatoskr.harness].sweep` field is refused
at parse time with a migration message rather than accepted, so the two
can't be coupled again by config.

## Lua runtime boundary

The Lua VM, the `ServiceClient` bindings, the wait combinators and the
runtime-owned artefact writers live in ratatoskr. brokkr builds against none
of them: it reaches the runtime only through the harness binary's
`--test-harness SCRIPT` mode, and passes `BROKKR_TEST_BIN_DIR` so the
runtime can re-spawn that binary in another mode. Which ratatoskr crate
holds the runtime, and whether a feature gates it, must stay ratatoskr's
business; a brokkr change that names either couples the two repos.
