# Service-subprocess test harness: service-test, service-suite, service-list

Gated to `project = "ratatoskr"`. Runs lua-driven tests against the ratatoskr
harness binary (`<binary> --test-harness <script>`).

For the `[ratatoskr.harness]` config block (sweep + binary linkage) see
`docs/brokkr.toml.md`. For the harness model (lua VM, ServiceClient userdata,
artefact writers, runtime-outcome) see `docs/projects/ratatoskr.md`.

## `service-test <SCRIPT_OR_DIR>`

Run a Service-subprocess test script. When the path is a directory under
`crates/app/tests/service-harness/`, this is sugar for `service-suite --filter
<rel>/` scoped to that cohort - same code path, same artefact layout, `-N`
becomes cohort cycles.

End-to-end on the brokkr side: project gating, script-path validation,
frontmatter parse (`ceiling`, `preserve_data_dir`), lockfile, sweep-aware
build via `[ratatoskr.harness]` (same feature contract as `brokkr check`),
per-run artefact-dir allocation under `.brokkr/ratatoskr/<test>/run-N/`, sync
`std::process::Command` spawn of `<binary> --test-harness <script>` with
`BROKKR_HARNESS_ARTEFACT_DIR` and `BROKKR_TEST_BIN_DIR` exported, stdout/
stderr drained to `binary-stdout.log` / `binary-stderr.log` while a 50ms-
cadence poll loop watches for child exit or for the wall-clock ceiling to
expire (default 60s, override via `-- ceiling: 60s` frontmatter).

When the ceiling fires brokkr SIGKILLs the child and labels the run
`ceiling=<spelling>`; otherwise the label tracks the child's exit code/signal.
`run.toml` is written with reproducibility metadata; on success the artefact
dir is deleted unless `--keep-artefacts` is passed or the script's frontmatter
sets `preserve_data_dir = on_success_too`; failures always preserve;
history-DB records every run.

The Lua VM, `ServiceClient` userdata bindings, wait combinators, and
runtime-owned artefact writers (`frames.jsonl`, `events.jsonl`, `steps.jsonl`,
`service.stderr`, `runtime-outcome.json`,
`proc-{status,wchan,syscall,stack}.txt`, `data-dir/`) live in ratatoskr's
`app` crate behind `app --test-harness`, gated by the `test-helpers` feature.

Flags:
- `-N <COUNT>` - on a single script, repeats with per-iter status, per-iter
  artefact dir, `--keep-going`, exit-code aggregation; on a directory, runs
  the cohort `<COUNT>` times in order via the `service-suite` code path.
- `--debug` - flips the build to dev profile (default release).

Cross-cutting design lives at
`<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`.

## `service-suite [--filter SUBSTR] [-N COUNT] [--keep-going] [--include-ignored] [--keep-artefacts] [--debug]`

Run every discovered service-test script in sequence against a single shared
harness build. `-N` runs the whole cohort `<count>` times in order (e.g. 50
cycles over 11 t1/ scripts = 550 runs); the trailing summary reports cohort
totals plus a per-script `pass/total` table.

Discovers `crates/app/tests/service-harness/**/*.lua`, optionally filters by
substring against the relative name, then runs each script through the same
`spawn_and_capture` path `service-test` uses (per-script artefact dir,
ceiling, `preserve_data_dir`). Scripts marked `expected = ignored` in the
frontmatter are skipped unless `--include-ignored` is passed.

Default is stop-on-first-failure so the failing script's artefacts land fast
for triage; `--keep-going` runs every selected script and the trailing summary
lists the failing names. Empty-state messages distinguish "nothing
discovered" / "filter matched none" / "all matches ignored" so the user gets
a directly actionable response. Exit code is non-zero if any selected script
failed.

## `service-list`

List discovered service-test scripts. Walks
`crates/app/tests/service-harness/**/*.lua` recursively under the project
root, parses a `-- key: value` frontmatter at the top of each script, prints
a sorted table.

Recognised fields:
- `description` - free text
- `expected` - `pass | ignored`
- `ceiling` - per-script wall-clock backstop, with unit suffixes
  `ms`/`s`/`m`/`h`; bare numbers are seconds; default 60s when omitted.
- `preserve_data_dir = on_success_too` - keeps the artefact dir even on
  success; failures always preserve.

Unknown fields are ignored so scripts can carry their own annotations.
Empty-state output names the expected directory. Nested cohort dirs (`t1/`,
`extract/`, etc.) are picked up automatically; the displayed name is the path
relative to the script root, minus `.lua`.
