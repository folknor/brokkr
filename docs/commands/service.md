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
frontmatter parse (`ceiling`, `preserve_data_dir`, `fixture`), lockfile,
sweep-aware build via `[ratatoskr.harness]` (same feature contract as
`brokkr check`), optional sæhrimnir spawn for fixture-bearing scripts (see
"Fixture frontmatter" below), per-run artefact-dir allocation under
`.brokkr/ratatoskr/<test>/run-N/`, sync `std::process::Command` spawn of
`<binary> --test-harness <script>` with `BROKKR_HARNESS_ARTEFACT_DIR` and
`BROKKR_TEST_BIN_DIR` exported (plus the `RATATOSKR_TEST_*_ENDPOINT` family
when a fixture is bound), stdout/
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

## Fixture frontmatter

A service-harness script that needs a mock email-protocol server declares
the dependency in frontmatter:

```
-- fixture: jmap-small.toml
-- protocol: jmap
```

When `service-test` or `service-suite` sees `-- fixture: <NAME>` on a
selected script, brokkr resolves `<NAME>` against `[ratatoskr]
fixtures_dir` (the verbatim string is passed through, so the explicit
`.toml` / `.lua` extension form disambiguates when both files exist),
spawns sæhrimnir via `[ratatoskr] mock_server_binary` with
`--fixture <PATH> --readiness-file <mock_dir>/readiness`, parses the
five-line readiness sentinel for per-protocol ports, then injects the
configured `RATATOSKR_TEST_*_ENDPOINT` env vars into the harness
process (HTTP origins for jmap/graph/gmail, `host:port` for imap/smtp -
same shape as `sync-smoke` / `sync-bench`). After every dependent run,
sæhrimnir is SIGTERM'd with the standard 1.5s drain budget then
escalated to SIGKILL if it overruns.

Mock artefacts (`stderr.log`, `readiness`) land under
`.brokkr/ratatoskr/<test>/mock/<fixture-name>/` for `service-test` and
`.brokkr/ratatoskr/mock/<fixture-name>/` for `service-suite`.

`brokkr lock` from another shell shows the live harness PID (and progress
`run R/T,` when the run set has more than one entry - soak with `--repeat
> 1`, or any `service-suite` invocation that runs multiple scripts /
cycles), plus one `mock PID …` line per live sæhrimnir. `brokkr kill --hard`
SIGKILLs every recorded child + mock alongside brokkr; `brokkr kill`
(SIGTERM) is caught by a guard installed right after the lockfile and
held through build + run + teardown - the captured runner (used for both
`cargo build` and the harness binary) forwards SIGTERM to whichever child
is current with a 1.5s budget, the orchestrator then drains all mocks
with the same budget, and any error mid-loop (not just `Interrupted`)
takes the same drain path before propagating, so a failing suite never
falls through to `MockServer::Drop`'s SIGKILL fast-path.

`service-test`: the mock spawns once before the soak begins and is
reused across all `-N` iterations of the same script. Scripts without a
`fixture` line skip the spawn entirely; no `[ratatoskr]` mock config is
required for those.

`service-suite`: scripts are run in discovery order (alphabetical, which
naturally clusters cohorts under shared parent dirs). Fixture lifecycle
is **suite-scoped**: each distinct fixture string spawns sæhrimnir at
most once, lazily on first use, and the mock stays alive for every
remaining script and cycle that shares that fixture - including
no-fixture scripts running in the middle. Endpoint env vars
(`RATATOSKR_TEST_*_ENDPOINT`) are injected only into scripts that
declare that fixture; no-fixture scripts run as if no mock exists, even
when other mocks are alive in the background. Fixture isolation is the
script's responsibility (e.g. hitting the mock's `/test/<protocol>/reset`
or `DELETE /test/.../submissions` at start-of-test); brokkr does not
recycle the process between scripts. Every spawned mock is drained
gracefully at suite end (SIGTERM with the standard 1.5s budget per mock
before SIGKILL escalation), regardless of whether the loop succeeded or
errored. The suite validates `[ratatoskr] mock_server_binary` and
`fixtures_dir` up front when any selected script needs a fixture, so a
missing config fails before the cargo build starts.

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
- `fixture` - name of a sæhrimnir fixture under `[ratatoskr] fixtures_dir`.
  Triggers mock-server spawn + endpoint env-var injection at run time
  (see "Fixture frontmatter" above). Pass with `.toml` / `.lua`
  extension to disambiguate when both files exist.
- `protocol` - informational; sæhrimnir binds all five protocol
  listeners regardless of this value.

Unknown fields are ignored so scripts can carry their own annotations.
Empty-state output names the expected directory. Nested cohort dirs (`t1/`,
`extract/`, etc.) are picked up automatically; the displayed name is the path
relative to the script root, minus `.lua`.
