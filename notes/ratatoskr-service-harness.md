# Ratatoskr Service test harness - planning note

Status: **scaffolding landed; architecture in flight.** Replaces
`notes/ratatoskr-support.md` for everything related to Service-subprocess
tests. Provider mocks and sync benchmarks move to sibling notes
(`notes/ratatoskr-mock-server.md`,
`notes/ratatoskr-sync-orchestration.md`). See "Implementation status"
below for what's currently in tree.

## Background for reviewers

Throughout this document, `<ratatoskr>` refers to the root of the
ratatoskr repository (typically `~/Programs/ratatoskr` on the
author's machine, sibling to brokkr's checkout). All other paths are
relative to brokkr's repo unless otherwise stated.

**Brokkr** is a single-binary Rust dev tool installed via
`cargo install --path ~/Programs/brokkr`. Invoked from any project root,
it reads `./brokkr.toml` to detect which project it's working in
(`pbfhogg`, `elivagar`, `nidhogg`, `litehtml-rs`, `sluggrs`, etc.) and
exposes project-gated top-level subcommands tagged `[pbfhogg]`,
`[nidhogg]`, etc. It already owns: lockfile coordination, build
orchestration with feature sweeps, a sidecar profiler that samples
`/proc` at 100ms while a child runs, a results DB
(`.brokkr/results.db`) for benchmark history, a global command history
DB, persistent worktrees for retroactive benchmarking, structured
artefact retention, project-specific download/verify pipelines, and
visual reference testing for HTML/glyph projects. There is no
`brokkr pbfhogg ...` namespace - every command is flat and
project-gated. This note proposes adding `[ratatoskr]` to that set.

**Ratatoskr** is a Rust desktop email client. Two long-running OS
processes:

- The **UI** is an `iced` app. It owns rendering, input, action
  planning, search reads, and SQLite/content-store reads.
- The **Service** is a child worker process spawned by the UI. It
  owns *all writes*: sync, action execution, DB writes, body/inline/
  blob store writes, Tantivy indexing, attachment extraction, push
  receivers, and pending-ops recovery across crashes.

The two processes talk JSON-RPC over stdio. The Service binary is
`app --service` (the same binary the UI runs, with a flag). Boot is
two-phase: UI awaits a `ChildSpawned` event after the version-check
ping, then `BootReady` after schema/key/recovery work completes. Boot
failures (`AnotherInstanceRunning`, `MigrationFailure`,
`KeyLoadFailure`) are terminal; runtime crashes after `BootReady`
should respawn with stale notifications dropped by service-generation
tags.

Tests for the Service's lifecycle live in
`<ratatoskr>/crates/app/tests/service_subprocess.rs` - a 1466-line file of
`#[tokio::test]` functions that each spawn `app --service` as a
subprocess, drive JSON-RPC over its stdio, and assert on observed
behaviour. The Service is load-bearing for boot, has tight invariants
around shutdown drain ordering, and is the focus of "Phase 8" in the
Service roadmap (`<ratatoskr>/docs/service/implementation-roadmap.md`)
- a phase that deliberately reshapes the test harness because the
existing one can't carry the test cohort Phase 8 needs.

This plan is for that reshape.

## Problem

Ratatoskr's Service is a child worker process spawned by the UI. It owns
all writes (sync, action execution, DB writes, blob/body/inline stores,
Tantivy, push receivers). UI and Service talk JSON-RPC over stdio.

Tests for the Service's lifecycle - boot handshake, shutdown drain, drop
behaviour, parent-death, signal handling, crash recovery, schema-version
respawn, JSON-RPC framing defense, etc. - currently live as
`#[tokio::test]` functions in
`<ratatoskr>/crates/app/tests/service_subprocess.rs`.
Each test spawns the `app --service` binary, drives the protocol, and
asserts on observed behaviour with internal `tokio::time::timeout` calls.

That shape doesn't work. Two of those tests are `#[ignore]`'d today
(`service_subprocess_ping_and_shutdown`,
`spawn_with_events_emits_terminal_on_missing_key`); they hang
intermittently. The deeper problem: every test of this shape is
structurally racy. Wall-clock timeouts inside the test race against the
implementation's own timeout ceilings (the comment at line 481 of
`service_subprocess.rs` already concedes this).
`DataDirGuard::Drop`
loses diagnostic state on every failure. `kill_on_drop(false)` orphans
the Service when the test itself is killed. Running them 200 times
doesn't make them deterministic; it averages the noise.

The cohort facing this problem is large. Phase 8 of the Service
roadmap (`<ratatoskr>/docs/service/implementation-roadmap.md`)
names ~15+
similarly-shaped real-subprocess tests planned across Phases 2-7
(`pre_ack_crash_rolls_back_subprocess`, `journal_replays_after_respawn`,
`compose_send_50mb_attachment`, `bulk_archive_200_threads_under_budget`,
the `--test-fake-schema=N` e2e, the whole T1 cohort) that haven't
landed because today's framework can't carry them. Phase 8 is explicit
that the harness is on the critical path: "building T1 against today's
framework would mean rebuilding it once Phase 8's work lands."

The manual test matrix
(`<ratatoskr>/docs/service/manual-test-matrix.md`) has two
items (heartbeat-detects-killed-Service; SIGTERM-triggers-shutdown-drain)
that sit in manual-only because they're "too noisy to assert reliably
from automation." A deterministic harness should pull them in too.

## Goal

A test harness in brokkr that makes Service-lifecycle tests
deterministic by construction, generic enough to drive any JSON-RPC
subprocess, and decoupled from ratatoskr at the source level so that
adding a ratatoskr test never requires rebuilding brokkr.

The two ignored tests are the wedge. Re-enabling them stable proves
the harness exists. The real target is the Phase 8 cohort plus the
manual-matrix items.

## Why brokkr

Brokkr already owns project-aware orchestration: lockfiles, build
selection, sidecar profiling, history, artefact retention. A
subprocess test harness extends that surface with the same shape -
brokkr is the runtime, tests are data, project gating selects which
runtime applies.

Ratatoskr keeps owning the Service implementation, the JSON-RPC
protocol, test-only protocol surface (`--test-fake-version`,
`--test-fake-schema=N`, `test.println`, future `test.seed_account`
etc.), and assertion semantics. Anything ratatoskr-specific lives in
test scripts and ratatoskr's test-hook handlers; brokkr stays generic.

## Dependency direction

**Open. Originally specified as "brokkr depends on ratatoskr"; the user
has since pushed back and we're choosing between two replacements
before any binding code lands.** The original wording is preserved
below for context, with the live constraint listed first.

Live constraint:

- ratatoskr **must not** depend on brokkr (unchanged).
- brokkr **must not** depend on ratatoskr (new; user-imposed).
- Two architectures still satisfy both rules; they trade where the
  Lua VM and `ServiceClient` Lua bindings live:
  - **(B) Flip the VM to ratatoskr.** Ratatoskr's `app` crate depends
    on `dellingr` directly and gains a `harness` module that registers
    `ServiceClient` / `SpawnEvent` / `ClientError` /
    `NotificationQueue` as Lua userdata, exposed via a new
    `app --test-harness <script.lua>` CLI flag. Brokkr provides
    orchestration only: project gating, build, lockfile, artefact-dir
    lifecycle, history, soak/suite. Brokkr ships zero ratatoskr or
    dellingr deps. **Currently the leading recommendation.**
  - **(C) Reimplement protocol logic in brokkr.** Ship a brokkr-side
    JSON-RPC client that re-derives `BootClassification`,
    `SchemaVersionChanged { was, now }`, `ClientError` discrimination,
    generation-tag tracking on notifications. The original note
    rejected this as "far cheaper to embed than reimplement"; cost is
    duplicated stateful code that has to track ratatoskr's protocol
    forever. Listed for completeness, not recommended.
- Adding or changing a test does not rebuild brokkr (unchanged in
  either architecture).

Original wording (now superseded - kept so review history reads
cleanly):

> brokkr **does** depend on ratatoskr at the source level. Specifically
> on the `app` crate (or a slimmer support crate carved from it) plus
> `service-api`. This is required, not optional - the existing tests
> in `<ratatoskr>/crates/app/tests/service_subprocess.rs` lean on
> `ServiceClient`'s Rust-level API in ways that cannot be re-derived
> from raw JSON-RPC frames + child-exit (see Architecture). Brokkr
> embedding `ServiceClient` directly is far cheaper than reimplementing
> it.

## Architecture

The harness is a Lua VM (`dellingr`) embedded in brokkr, with
`ServiceClient` and friends exposed as Lua userdata, wrapped in a
deterministic wait-and-artefact-capture layer.

### What brokkr provides

- **Embedded Lua VM** (`dellingr`) running test scripts.
- **`ServiceClient` userdata** - Lua scripts construct one via
  `harness.spawn(args)` or `harness.spawn_with_events(args)` and call
  the same methods the existing `#[tokio::test]` functions call:
  `client:request("HealthPing")`, `client:request("Shutdown")`,
  `client:notifications()`, `client:current_generation()`,
  `client:child_pid()`, `client:shutdown()`, `drop(client)`.
- **`SpawnEvent` receiver userdata** - `events:next(timeout_secs)`
  returns one of `ChildSpawned { client }`, `BootReady { response }`,
  `Terminal { error }`. The classification logic stays inside
  `ServiceClient`; brokkr does not synthesise events.
- **`NotificationQueue` userdata** - `queue:recv(timeout)` /
  `queue:drain_for(duration)` return `Notification` userdata that
  scripts inspect for `service_generation`, `method`, etc.
- **Deterministic wait combinator** - exposed as
  `harness.wait_for { predicate, child = client, backstop = "30s" }`.
  Every wait races the predicate against `client:observe_child_exit()`
  internally; failure verdicts name which fired.
- **Process orchestration not covered by `ServiceClient`** -
  process-group spawn for non-`ServiceClient` children (the
  `parent_death_helper` binary, future stub helpers); SIGKILL/SIGTERM
  to a named PID; data-dir snapshotting; sentinel-file watch
  (`harness.wait_for_sentinel(path, backstop)`); JSON-RPC frame log
  for diagnostic purposes (taps the wire underneath ServiceClient,
  not the primary test surface).
- **Artefact retention** - per-test scratch dir under
  `.brokkr/ratatoskr/<test>/<run-N>/` populated on failure; deleted
  on success unless `--keep-artefacts`.
- **Cost-budgeted execution** - Lua VM aborts a runaway script via
  `dellingr`'s instruction-cost ceiling.

### What scripts express

A script is a Lua file. It calls `harness.spawn(...)`, drives the
Service via `ServiceClient` methods exactly like an
`#[tokio::test]` body would, and asserts on returned values via
ordinary Lua comparisons. The script does not parse JSON-RPC
itself; it consumes the structured Rust types `ServiceClient`
already returns. The frame log is captured under the hood and
emitted to the artefact dir on failure.

### Determinism rule

Every wait has the shape `wait(condition) until
(condition_satisfied | child_terminated | declared_backstop_expires)`.
The first transition to fire wins. The harness records which one fired
in the test trace. Tests assert on the transition that should have
fired; failure messages name the transition that actually did.

This is the Phase 1.6 `request_or_observe_child_exit` pattern
(`<ratatoskr>/crates/app/src/service_client.rs`) lifted from "one
helper inside ratatoskr" to "the default wait shape exposed through
the Lua binding." `ServiceClient` already provides the underlying
mechanism (`observe_child_exit` polls `Child::try_wait` on a 50ms
interval); the harness's wait combinator just wires that polling
into every wait the script can express. Wall-clock is never the
primary signal.

Backstops are still wall-clock - the harness can't escape physical
time entirely - but they are explicit, named, generous, and only fire
when a determinism bug elsewhere (a missing sentinel, an unmatched
event) leaves the harness with nothing else to wait on. A backstop
firing is a test-design bug, not a flake.

### Test scripts

Tests are Lua scripts. Brokkr embeds the `dellingr` Lua VM
(publication imminent) and exposes the existing
`<ratatoskr>/crates/app/src/service_client.rs` API as Lua
userdata. Adding a test means adding a `.lua` file in ratatoskr's
tree; no brokkr rebuild.

Why Lua via `dellingr`:

- Pure Rust, no FFI, no system Lua dep.
- `HostCallbacks` redirects `print()` to per-test capture and hooks
  errors for the failure dump.
- `RustFunc` is the existing pattern for exposing Rust functions to
  Lua; `ServiceClient` methods plug in directly as userdata methods.
- Per-script cost budget via `State::cost_remaining` aborts runaway
  scripts without leaning on wall-clock kill paths.
- Variable capture, loops, conditionals come from the language.

The plan does not specify the Lua API in syntax-accurate form.
What it specifies is the **required capabilities** scripts must be
able to express, derived from the existing tests in
`<ratatoskr>/crates/app/tests/service_subprocess.rs`, the Phase 8
named cohort, and manual-matrix items 4 and 5. The capabilities
below name the `ServiceClient` (and friends) methods that the Lua
binding has to surface.

#### `ServiceClient` methods exposed to Lua

The existing test bodies call these. The Lua binding wraps each
one-for-one. Names below are Rust; Lua spelling is whatever the
binding picks.

- `spawn_for_test(binary, data_dir, extra_args) -> Arc<ServiceClient>`
- `spawn_with_events_for_test(binary, data_dir, extra_args) -> mpsc::Receiver<SpawnEvent>`
- `request::<R>(params: RequestParams) -> Result<R, ClientError>` -
  including the typed `RequestParams` variants
  (`HealthPing`, `Shutdown`, `TestPrintln`, `TestSlow`,
  `ExecutePlan`, `JobStatus`, `MarkChatRead`, etc.).
- `notifications() -> Arc<NotificationQueue>` - returns a queue
  userdata with `recv(timeout)` and drain helpers.
- `current_generation() -> u32`
- `child_pid() -> Option<u32>`
- `shutdown() -> Result<(), ClientError>`
- `drop(client)` - explicitly invocable from Lua to test the Drop
  teardown path.

`SpawnEvent` becomes Lua userdata with three case constructors:
`ChildSpawned { client }`, `BootReady { response }`, `Terminal { error }`.
`ClientError` becomes Lua userdata with case-discriminating accessors
(`is_service_crashed()`, `boot_classification()`,
`schema_version_changed()` returning `{ was, now }`, etc.) so scripts
can pattern-match the way the existing tests do.

#### Process and connection control

- **`drop(client)`** - exercises the explicit Drop teardown path
  (see `dropping_client_terminates_child_within_one_second`,
  `deadlocked_service_drop_escalates_to_kill`).
- **PID-existence polling** - `harness.pid_is_alive(pid)` mirroring
  ratatoskr's `pid_is_alive` test helper. Used by every test that
  asserts on a child's eventual death after a Drop or signal.
- **Stub-parent helper invocation** - the `parent_death_helper`
  binary already exists in ratatoskr (registered as
  `CARGO_BIN_EXE_parent_death_helper`). The harness builds it
  alongside `app` and exposes
  `harness.spawn_parent_death_helper(service_binary, data_dir)
  -> { service_pid, helper_handle }`. Required for
  `linux_parent_sigkill_terminates_service_within_two_seconds`.
- **Send signal** - `harness.kill(pid, signal)` for SIGKILL/SIGTERM
  to a captured PID. Used directly in 6 of the 17 existing tests.
- **Respawn** - already a `ServiceClient` capability via
  `spawn_with_events_for_test` + SIGKILL + waiting for follow-up
  events on the same receiver. Lua scripts express it the same way
  the existing tests do; the harness provides nothing new.

#### Assertion shapes

- **Pattern-match on `SpawnEvent`** - distinguish `ChildSpawned` vs
  `BootReady` vs `Terminal`, with `Terminal` carrying a
  `BootClassification` the script can compare against named
  constants (`BootExitCode::KeyLoadFailure`,
  `BootExitCode::AnotherInstanceRunning`, etc.).
- **Pattern-match on `ClientError`** - `Io`, `Service`,
  `ServiceCrashed`, `Timeout`, `VersionMismatch { ui, service }`,
  `BootFailure { classification }`, `SchemaVersionChanged { was, now }`.
- **Arc identity across respawn** -
  `harness.same_client(a, b) -> bool` exposes `Arc::ptr_eq`. Used
  by `respawn_after_sigkill_succeeds` to assert the in-place state
  swap.
- **Notification-queue introspection** - `queue:drain_for(duration)
  -> [Notification]`, with `Notification:service_generation()`
  exposing the tag. Required for
  `stale_notifications_dropped_after_generation_bump_end_to_end`
  and the cardinality-on-notifications cases
  (`mark_chat_read_emits_only_action_completed`).
- **Absence over window** - "no event received in N seconds, after
  a known transition." The wait combinator returns "expired" as a
  first-class verdict so the absence assertion is structural, not
  an exception. Used by
  `terminal_failure_at_initial_boot_does_not_respawn` (post-Terminal
  no-respawn window) and
  `println_from_handler_does_not_corrupt_json_rpc_framing` (canary
  not on wire).
- **Cardinality** - "exactly N notifications matching predicate."
  Lua expresses with `drain_for` + `#table` + `assert`.
- **Counter probe with delta** - `client:request("TestCounterRead",
  ...)` (or whatever helper RPC is added in Phase 8) called before
  and after, with Lua subtraction. No new harness primitive needed
  beyond the test-helper RPC.
- **Resource budget** - peak RSS, IO bytes from sidecar samples
  during a script's lifetime. Reuse brokkr's existing sidecar; expose
  `harness.resource_summary(client) -> { rss_kb, io_bytes, ... }`.
  Wall-clock deltas come from Lua + `os.time`.
- **Child exit** - `harness.wait_exit(client, backstop) -> ExitStatus`
  with `code()`, `signal()`, `wall_time_ms()`. Used by every
  exit-code test (`missing_key_file_*`, the parallel-instance test).
- **Time-floor assertion** - "drop took at least N ms before
  escalating to kill." A simple `os.time` delta in Lua. Used by
  `deadlocked_service_drop_escalates_to_kill`.

#### Determinism scaffolding

- **Wait combinator** - `harness.wait_for { ... }`. Composes a
  predicate against `client:observe_child_exit()` so any Service
  death short-circuits the wait with a "child exited while awaiting
  X" verdict.
- **Sentinel-file watch** - `harness.wait_for_sentinel(path,
  backstop)`. Required for the `clean_shutdown` sentinel in
  manual-matrix items 4 and 5; available for any future test that
  benefits from a non-clock readiness signal.
- **Frame log** - captured under the hood via ServiceClient's wire
  layer (the harness taps stdin/stdout). Diagnostic only; emitted to
  the artefact dir on failure. Tests do not pattern-match on raw
  frames - they pattern-match on `ServiceClient` return values and
  `Notification` userdata.
- **Backstop policy** - explicit, named, generous. Backstop firing
  is a test-design bug, not a flake.

#### Cohort coverage table

| Test | Surface used |
| --- | --- |
| `service_subprocess_ping_and_shutdown` (existing, ignored) | direct subprocess + raw frames; rewrite to `ServiceClient`-based once stable. |
| `dropping_client_terminates_child_within_one_second` | `spawn_for_test`, `child_pid`, `drop(client)`, `pid_is_alive` poll. |
| `spawn_failure_against_missing_binary_returns_io_error` | `spawn_for_test` against bogus path, expect `ClientError::Io`. |
| `linux_parent_sigkill_terminates_service_within_two_seconds` | `parent_death_helper` invocation, PID handoff via stdout, `kill(helper, SIGKILL)`, `pid_is_alive` poll on Service PID. |
| `println_from_handler_does_not_corrupt_json_rpc_framing` | `spawn_for_test`, `request("TestPrintln")`, `request("HealthPing")`, two-step round-trip. |
| `version_mismatch_surfaces_during_handshake` | `spawn_for_test` with `--test-fake-version=999`, expect `ClientError::VersionMismatch { ui, service }`. |
| `pending_request_fails_with_service_crashed_when_child_killed` | `request("TestSlow", 60_000)` in background, `kill(pid, SIGKILL)`, expect `ClientError::ServiceCrashed`. |
| `spawn_with_events_emits_child_spawned_then_boot_ready_on_healthy_boot` | `spawn_with_events_for_test`, ordered events, `BootReady` field assertions. |
| `spawn_with_events_emits_terminal_on_missing_key` (existing, ignored) | `spawn_with_events_for_test` against keyless dir, expect `Terminal { BootFailure { KeyLoadFailure } }`. |
| `missing_key_file_exits_with_key_load_failure_code` | direct subprocess, hold stdin, `wait_exit` with code 73. |
| `second_instance_against_same_data_dir_exits_with_already_running` | two parallel children, drive A's IPC, B's `wait_exit` with code 71. |
| `spawn_with_events_classifies_another_instance_running` | A direct, B via events, expect `Terminal { BootFailure { AnotherInstanceRunning } }`. |
| `respawn_after_sigkill_succeeds` | `spawn_with_events_for_test`, `kill`, follow-up events, `same_client(a, b)`, ping after respawn. |
| `pending_request_fails_at_respawn_then_subsequent_succeeds` | combined `pending_request_fails_*` + `respawn_after_sigkill_*`. |
| `terminal_failure_at_initial_boot_does_not_respawn` | `spawn_with_events`, `Terminal`, post-Terminal absence-over-window. |
| `crashloop_threshold_emits_terminal_after_third_crash` | loop of `kill` + observe `ChildSpawned + BootReady`, third kill expects `Terminal`. |
| `stale_notifications_dropped_after_generation_bump_end_to_end` | `notifications()`, `current_generation()`, drain across kill+respawn, generation-tag check. |
| `deadlocked_service_drop_escalates_to_kill` | `spawn_for_test --test-hang-on-stdin-eof`, `drop`, `pid_is_alive` poll with time-floor + ceiling. |
| `pre_ack_crash_*` / `post_ack_crash_*` (Phase 8 cohort) | `request("ExecutePlan")`, fault-inject via test-helper RPC, kill, respawn, follow-up `request`, `Notification` drain. |
| `compose_send_50mb_attachment` | `request("ComposeSend", payload_from_file)`, `wait_exit` budget. |
| `bulk_archive_200_threads_under_budget` | Lua loop dispatching 200 `request` calls in parallel, wall-clock budget assertion via `os.time`. |
| `mark_chat_read_emits_only_action_completed` | `request("MarkChatRead")`, `notifications():drain_for`, cardinality-1 assertion. |
| `action_skips_search_index_write` / `handler_does_not_drive_batch_execute` | `request("TestCounterRead", ...)` before/after, Lua subtraction. |
| `journal_replays_after_respawn` / `stale_outcomes_dropped_after_respawn` | `request` + `kill` + respawn + `notifications` drain. |
| `test_fake_schema_propagates_via_terminal` | `spawn_with_events_for_test` first run + `kill` + respawn with `--test-fake-schema=N`, expect `Terminal { SchemaVersionChanged { was, now } }`. |
| Manual matrix #4 (heartbeat detects killed Service) | `spawn_with_events_for_test`, `kill(service_pid, SIGKILL)`, `wait_for_sentinel("logs/heartbeat-exiting")` or follow-up event. |
| Manual matrix #5 (SIGTERM triggers shutdown drain) | `spawn_for_test`, `kill(pid, SIGTERM)`, `wait_for_sentinel("clean_shutdown")`, `wait_exit`. |

If a capability above doesn't have a Lua binding, the binding is
incomplete; extend it. The harness binding never reimplements
`ServiceClient` behaviour - it forwards.

### Failure model

When a test fails, brokkr writes a self-contained artefact directory
to `.brokkr/ratatoskr/<test-name>/<run-N>/` containing:

- **`frames.jsonl`** - every JSON-RPC frame, both directions,
  timestamped from spawn. Single most useful artefact for
  drain-ordering / framing bugs.
- **`events.jsonl`** - every spawn event observed (`ChildSpawned`,
  `BootReady`, `Terminal`), timestamped.
- **`steps.jsonl`** - the test's step trace: which step was active,
  what condition was awaited, which transition fired.
- **`service.stderr`** - Service's stderr verbatim. Captured per-run,
  not race-mingled with test stdout.
- **`proc-at-failure.txt`** - snapshot of `/proc/<pid>/status`,
  `/proc/<pid>/wchan`, `/proc/<pid>/syscall`, `/proc/<pid>/stack` at
  the moment failure was declared. Distinguishes "blocked on futex"
  from "blocked on closed pipe" without re-running.
- **`data-dir/`** - copy of the test's app-data dir at failure time.
  SQLite WAL state, lockfile presence, key file, `clean_shutdown`
  sentinel.
- **`exit.txt`** - exit code, signal, wait time, exit reason
  (clean / harness-killed-on-backstop / signal / etc.).
- **`run.toml`** - the test script, env vars, brokkr version, git
  commit. Reproducibility metadata.

On success, the artefact directory is deleted unless
`--keep-artifacts` is passed.

The data dir copy and `/proc` snapshot are the two pieces of state
that today's tokio-test pattern destroys at failure (`DataDirGuard::Drop`
unconditional cleanup; no `/proc` capture at all). Recovering them is
the largest single jump in debug ergonomics.

## Where it lives in brokkr's CLI

Top-level commands tagged `[ratatoskr]`, project-gated to
`Project::Ratatoskr` (new variant). Same convention as `[pbfhogg]`,
`[elivagar]`, `[nidhogg]`, `[litehtml]`. No nested namespace.

Tentative command set (names not final, easily renamed):

- `brokkr service-test <SCRIPT>` - run one script. Equivalent of
  today's "run one cargo test."
- `brokkr service-test <SCRIPT> -N 200` - run the same script
  repeatedly. Stop on first failure unless `--keep-going`.
- `brokkr service-suite [--filter X]` - run every script under a
  configured root, optionally filtered.
- `brokkr service-list` - list available scripts with description and
  any `expected = "ignored"` markers (for tests that are still
  expected to fail because of an open Service bug).

## Required ratatoskr-side work

Most of the surface the harness needs already exists. Specifically:

- **`ServiceClient` and friends** already have the public API the
  Lua binding wraps - `spawn_for_test`, `spawn_with_events_for_test`,
  `request`, `notifications`, `current_generation`, `child_pid`,
  `shutdown`, plus the `SpawnEvent` and `ClientError` enums with
  their classification fields. No new method needed for v1.
- **Test-helper argv flags** already exist:
  `--test-fake-version=N`, `--test-hang-on-stdin-eof`. Phase 8 calls
  for `--test-fake-schema=N` (used by
  `test_fake_schema_propagates_via_terminal`); that's the only new
  argv flag the harness needs.
- **Test-helper `RequestParams` variants** already exist:
  `TestPrintln { message }`, `TestSlow { millis }`. Phase 8's
  named cohort needs additional variants for fault injection
  (`TestCrashAfterNWrites { ... }` or similar) and counter probes
  (`TestCounterRead { ... }`); names and shapes are Phase-8 design
  work, not harness design work.
- **`parent_death_helper` binary** already exists, registered as
  `CARGO_BIN_EXE_parent_death_helper`. Brokkr's harness invokes it
  by that name; ratatoskr keeps maintaining it.
- **Account seeding** (Phase 8 named harness gap) lands as a new
  test-helper `RequestParams` variant (`TestSeedAccount { ... }`)
  when T1 tests start needing FK-constrained writes. The Lua
  binding picks it up automatically once it's a `RequestParams`
  variant.

What does NOT need to be built:

- A new test-script DSL on the ratatoskr side. The DSL is brokkr's
  Lua via dellingr.
- A separate harness crate exporting types to brokkr. Brokkr depends
  on the `app` crate (or a slim support sub-crate carved from it)
  directly; `app` already exports `ServiceClient`, `SpawnEvent`,
  `ClientError`. If a slim crate is desired for compile-time
  hygiene, that's a ratatoskr-side refactor unrelated to the
  harness's correctness.

What's a ratatoskr-side decision but a brokkr-side dependency:

- Whether `ServiceClient` and friends move into a thinner crate
  (e.g. `app-service-client`) so brokkr's dependency surface is
  smaller. Phase 8 reshapes `ServiceClient` internally, so a slim
  crate may emerge naturally. Either way works for brokkr.

## Acceptance for v1

1. The harness builds, ships in brokkr, gated to `Project::Ratatoskr`.
2. The two ignored tests can be expressed as Lua scripts. When the
   underlying Service bug (writer-task drain ordering on shutdown) is
   present, the harness produces an artefact directory sufficient to
   diagnose the deadlock without re-running. When the bug is fixed,
   the scripts pass.
3. The Phase 8 T1 cohort (`journal_replays_after_respawn`,
   `pre_ack_crash_rolls_back_subprocess`, `post_ack_crash_replays_subprocess`,
   `compose_send_50mb_attachment`, etc.) is expressible in the same
   format - "expressible" here means a script can be written for each
   without harness changes, not that the underlying Service behaviour
   is necessarily complete yet.
4. Manual-matrix items 4 and 5 (heartbeat-detects-killed-Service,
   SIGTERM-triggers-shutdown-drain) move from manual-only to
   automated.
5. The Lua binding exposes the `ServiceClient` surface used by the
   existing 17 tests in `<ratatoskr>/crates/app/tests/service_subprocess.rs`,
   plus sentinel-file watch and process-tree primitives.
6. Adding a new ratatoskr test does not require rebuilding brokkr.
   Changes to ratatoskr's `ServiceClient` API surface do.

## Out of scope

- **Mock IMAP/JMAP servers and sync benchmarks.** Separate planning
  note (`notes/ratatoskr-provider-mocks.md`). Shares some plumbing
  (process orchestration, port allocation, artefact retention) but
  the hard problems are unrelated.
- **Replacing brokkr test.** The cargo single-test runner stays.
  Subprocess-lifecycle tests use the new harness; everything else
  uses `brokkr test`.
- **Fixing the underlying Service bugs.** Phase 8 owns the
  drain-ordering / class-aware-emit / crashloop work. The harness
  exists to make those bugs deterministic and diagnosable, not to
  hide them.
- **Migrating the existing tokio-tests.** The new harness coexists.
  Tests that work today as `#[tokio::test]` stay there. New tests in
  the cohort start in the new harness; old tests migrate only if
  their authors choose to.
- **CI-only features.** First user is a local developer
  root-causing a flake. CI integration follows once the local story
  works.

## Implementation status

Brokkr-internal scaffolding landed in a single commit; the steps that
need an architecture decision (B vs C above) or any ratatoskr-side work
are deferred. Annotations below ride alongside each step in the
suggested order.

Brokkr-side, in tree:

- `Project::Ratatoskr` is a first-class variant. `project = "ratatoskr"`
  in `brokkr.toml` resolves to it; `[ratatoskr]`-tagged commands
  show up grouped in `brokkr --help`; `project::require()` gates
  cleanly with the conventional error message.
- `brokkr service-test <SCRIPT>` is a skeleton: project-gated,
  validates the script path, exits non-zero with a "harness
  pending" message that points at this note. No build, no spawn,
  no artefact dir wired yet.
- `brokkr service-list` discovers `crates/app/tests/service-harness/
  *.lua` under the project root, parses a top-of-file
  `-- key: value` frontmatter (`description`, `expected =
  pass|ignored`), prints a sorted table. Empty-state message names
  the expected directory so a fresh checkout (no harness module
  yet in ratatoskr) gets a useful response.
- `ratatoskr::artefacts::ArtefactDir` allocates
  `<parent>/<test>/run-N/` with collision-incrementing N,
  finalize-success drops the dir unless `keep_on_success` was set,
  finalize-failure preserves unconditionally, `Drop` defaults to
  preserve so panicked tests keep diagnostics. Generic on
  `<parent>` so it lifts to a shared module the day a second
  project wants the same shape.
- `ratatoskr::process` provides `send_signal`, `pid_is_alive`
  (zombies count as alive), `wait_for_sentinel` (returns
  `Appeared` / `BackstopExpired` as first-class outcomes - the
  determinism rule's "predicate OR named backstop" shape, in its
  simplest form), `snapshot_proc` (copies
  `/proc/<pid>/{status,wchan,syscall,stack}` into the artefact
  dir; tolerant of read failures so the rest of the dump survives
  when `stack` needs `CAP_SYS_PTRACE`).

Deferred (architecture-decision-blocked):

- Lua VM embedding. If architecture (B) wins, brokkr never embeds
  dellingr - the VM lives in ratatoskr's `app` binary and brokkr
  spawns it.
- `ServiceClient` Lua bindings, wait combinator, frame-log tap.
  These live wherever the VM lives.
- Wedge tests, T1 cohort, manual-matrix items 4 / 5.

Deferred (ratatoskr-side):

- `--test-fake-schema=N`, `TestSeedAccount`, fault-injection /
  counter-probe `RequestParams` variants.
- (architecture B only) the new `harness` module + `app
  --test-harness` CLI flag.

Brokkr-side, not yet started but unblocked by the architecture
decision:

- Build orchestration: invoke `[[check]]` sweep machinery from
  `service-test` so the spawned binary matches `brokkr check`'s
  feature matrix.
- Lockfile + history-DB integration on the `service-test` path.
- Soak (`-N`), suite (`--filter`), JSON output for `service-list`.

## Suggested implementation order

1. **Brokkr depends on ratatoskr.** *(deferred / under revision -
   user has ruled out brokkr -> ratatoskr deps; see
   "Dependency direction" above. Architecture (B) replaces this
   step with "ratatoskr depends on dellingr; brokkr stays clean".)*
2. **`Project::Ratatoskr` enum variant + project gating.** *(done.)*
3. **Process / sentinel / artefact primitives.** *(partial - process
   primitives, sentinel watcher, /proc snapshot, artefact directory
   writer all landed in
   `src/ratatoskr/{process,artefacts}.rs`. Frame-log tap deferred -
   it has to live wherever the wire layer ends up, which depends
   on the architecture decision.)*
4. **Lua VM embedding.** *(deferred - depends on architecture
   decision. Dellingr is published as `0.1.0`; pull-in is mechanical
   once we know which side hosts the VM.)*
5. **Lua binding for `ServiceClient` and friends.** *(deferred -
   same.)*
6. **Wait combinator.** *(deferred - same. The simplest form
   (`wait_for_sentinel`) is in tree as a building block; the
   ServiceClient-aware variant follows once the VM is sited.)*
7. **CLI: `brokkr service-test <SCRIPT>`.** *(skeleton landed -
   project-gated, validates script path, exits non-zero with
   "harness pending" pointing at this note. Wiring to actually run
   a script is deferred behind the architecture decision.)*
8. **Wedge.** *(deferred.)*
9. **Phase-8 ratatoskr-side additions.** *(deferred -
   ratatoskr-side work, untouched.)*
10. **Cohort.** *(deferred.)*
11. **Soak / suite / list commands.** *(`service-list` landed
    early - it has no architecture-decision dependency, just walks
    the filesystem. Soak / suite still deferred until the
    single-script run path lands.)*
12. **Sidecar integration.** *(deferred; not required for v1.)*

## Open questions

- **Lua API surface naming and shape.** The capabilities list pins
  semantics; the function names, argument conventions, and return
  shapes are open. Sketch and iterate during plan-1 implementation.
- **Async / blocking model in Lua.** The harness drives async work
  (process I/O, child-exit polling, sentinel watching) from Rust;
  the Lua side calls a Rust function that blocks until the wait
  resolves. The VM is single-threaded so this is fine - no Lua
  coroutines or async-runtime in scripts. Confirm this maps cleanly
  to all required-capability shapes (in particular the
  loops-with-parallel-dispatch case for `bulk_archive_200_*`,
  which probably means a Rust-side `parallel_send(table_of_requests)`
  rather than Lua-level concurrency).
- **Cost budget defaults.** The VM exposes per-script
  instruction-cost accounting. Pick a default ceiling generous
  enough for normal tests but tight enough to catch runaways.
  Per-script override via a script-top-level `cost_budget = N`.
- **Sentinel-watch addressing.** Path relative to data dir?
  Absolute? File-glob support? Lean data-dir-relative with optional
  globs.
- **Backstop policy.** Per-call or per-script ceiling? Leaning
  per-call (every wait takes its own backstop arg) plus a
  per-script wall-clock ceiling enforced by the harness around the
  whole run.
- **Cargo-build integration.** Brokkr already knows how to build
  the right binary with the right features; the harness needs to
  invoke that machinery before spawning. Reuse `[[check]]` sweep
  selection or invent a `[ratatoskr.harness]` config? Lean toward
  reusing `[[check]]` so feature parity with `brokkr check` is
  automatic.
- **Test discovery.** Where do scripts live? Suggest
  `<ratatoskr>/crates/app/tests/service-harness/*.lua` so they're
  co-located with the existing tokio-tests.
- **Trace format stability.** `frames.jsonl` / `events.jsonl` /
  `steps.jsonl` schemas need to be stable enough for scripts and
  failure-triage tooling to consume across brokkr versions.
- **Concurrency between scripts.** Does the suite runner run scripts
  in parallel? Default no - subprocess tests touch real files and
  ports. Add `--jobs N` later if a class of scripts opts in.
- **`dellingr` dep timing.** The crate is being published soon.
  Brokkr should depend on the published version once it lands; a
  short-lived path-dep during plan-1 implementation is fine if the
  publication slips.

## Non-goals worth restating

The harness is not a replacement for ratatoskr's correctness
assertions, not a JSON-RPC server, not a CI-only tool, not a
benchmark framework, not a cargo-test wrapper. It is one thing: a
deterministic runtime for subprocess-lifecycle tests, with a
failure dump that lets a developer diagnose hangs without
re-running.
