# Ratatoskr Service test harness - planning note

Status: **architecture decided (option B); brokkr-side scaffolding
complete through harness-roadmap M5.** Replaces `notes/ratatoskr-support.md`
for everything related to Service-subprocess tests. Provider mocks
and sync benchmarks move to sibling notes
(`notes/ratatoskr-mock-server.md`, `notes/ratatoskr-sync-orchestration.md`).
See "Current state" immediately below for the tight version, or
"Implementation status" further down for the full breakdown. Matching
ratatoskr-side documents:
`<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`
(`architecture.md` is authoritative for the ratatoskr side; this note
is authoritative for the brokkr side; the two stay in sync).

## Current state

- **Done on the brokkr side:** Project gating, sweep-aware build,
  `service-test` (single-shot + soak) end-to-end with
  deadline-bounded spawn, `ceiling` / `preserve_data_dir`
  frontmatter, recursive `service-harness/**/*.lua` discovery,
  history-DB recording, and the orchestrator-side process /
  sentinel / artefact primitives. See "Implementation status" for
  the per-item breakdown.
- **Next on the brokkr side:** `brokkr service-list --json`. M7
  polish - unblocked but not Phase-8-blocking.
  (`brokkr service-suite [--filter X]` landed - see
  "Implementation status".)
- **Waiting on ratatoskr (Phase 8 / harness-roadmap M1):** the `app`
  crate's harness module (Lua VM bootstrap, `ServiceClient`
  userdata, `wait_for` / `expect_quiet` combinators, registry-backed
  request binding, frame-log tap, runtime-owned artefact writers),
  the `app --test-harness <script.lua>` CLI flag gated behind
  `test-helpers`, the `[[check]] name = "harness"` and
  `[ratatoskr.harness]` blocks in ratatoskr's `brokkr.toml`, and the
  wedge cohort under `crates/app/tests/service-harness/`.
  `dellingr 0.2.0` is published on crates.io; pulling it in is one
  workspace-dep line.
- **Phase 8 close-out gate (M2 of the harness roadmap):** the two
  wedge scripts (`ping_and_shutdown.lua` and
  `terminal_on_missing_key.lua`) passing under
  `brokkr service-test <script>` plus a 200-iteration soak. Brokkr
  is structurally ready - the gate is the ratatoskr-side runtime
  shipping.

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

**Decided: option (B), flip the VM to ratatoskr.** Both directions
are off the table for source-level deps:

- ratatoskr **must not** depend on brokkr.
- brokkr **must not** depend on ratatoskr.

Cross-process communication is by subprocess spawn + env vars only.
The Lua VM and `ServiceClient` userdata bindings live in ratatoskr's
`app` crate (already where `ServiceClient` is defined); ratatoskr
takes a `dellingr` dep, exposes the runtime via a new
`app --test-harness <script.lua>` CLI flag gated behind the existing
`test-helpers` feature. Brokkr orchestrates only: project gating,
sweep-aware build via `[ratatoskr.harness]`, lockfile, per-run
artefact-dir lifecycle, history-DB recording, soak/suite. Brokkr
ships zero ratatoskr or dellingr deps; brokkr stays sync (no tokio).

Why this resolution rather than the original "brokkr depends on
ratatoskr" framing: the harness needs `ServiceClient`'s typed
classification (boot exit codes, ClientError variants,
`SchemaVersionChanged { was, now }`, generation-tag tracking on
notifications), which is hundreds of lines of stateful protocol
logic. Embedding it in brokkr would force tokio in (the wait
combinator, notification routing, and child-exit polling all need
concurrent stdio + timeout handling) and either a heavy `app`-crate
dep or a parallel JSON-RPC client implementation. Hosting the VM
in ratatoskr keeps the protocol logic where the protocol is, keeps
brokkr small, and lets the Lua bindings sit one file over from
`ServiceClient` itself.

The matching ratatoskr-side document is
`<ratatoskr>/docs/service/brokkr-phase-8-scaffolding.md`.

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

The harness is split across the two repos. Ratatoskr's `app` crate
hosts the Lua VM and the `ServiceClient` userdata bindings (the
runtime); brokkr orchestrates the runtime from outside (build,
spawn, artefact dir, history). Concretely:

    brokkr service-test foo.lua
        |
        +-- builds [ratatoskr.harness].binary via the configured [[check]] sweep
        +-- allocates .brokkr/ratatoskr/<test>/run-N/ as artefact dir
        +-- spawns: <project_root>/<target>/<profile>/app --test-harness foo.lua
        |       env: BROKKR_HARNESS_ARTEFACT_DIR=<artefact dir>
        |            BROKKR_TEST_BIN_DIR=<bin dir>
        +-- waits for child exit (sync, std::process::Command-level)
        +-- preserves artefact dir on failure / non-zero exit

The runtime inside `app --test-harness`:

    +-- dellingr Lua VM
    +-- ServiceClient / SpawnEvent / ClientError / NotificationQueue
    |   exposed as Lua userdata, one method per Rust method
    +-- wait_for { predicate, child, backstop } / expect_quiet
    |   { predicate, child, window } combinators racing against
    |   ServiceClient::observe_child_exit
    +-- script-visible process primitives (kill, pid_is_alive,
    |   sentinel watch)
    +-- artefact writers (frames.jsonl, events.jsonl, steps.jsonl,
        proc-{status,wchan,syscall,stack}.txt, data-dir/,
        service.stderr, runtime-outcome.json) into
        BROKKR_HARNESS_ARTEFACT_DIR

### What brokkr provides

- **Project gating + CLI surface.** `Project::Ratatoskr` first-class
  variant; `[ratatoskr]`-tagged commands (`service-test`,
  `service-list`, eventual `service-suite`) in `brokkr --help`.
- **Sweep-aware build.** Reads `[ratatoskr.harness] sweep / binary`
  out of `brokkr.toml`, matches `sweep` to a `[[check]]` entry,
  builds every `build_packages` entry with the sweep's feature
  flags, returns the path to the `binary`-package executable.
  Cross-checked at parse time so a typo errors before cargo runs.
- **Subprocess spawn + capture.** `std::process::Command::output()`
  level of concurrency: spawn the harness binary with
  `--test-harness <script>` plus the artefact-dir env var, wait
  for exit, capture stdout / stderr / exit code / signal. No tokio,
  no JSON-RPC parsing on brokkr's side.
- **Per-run artefact directory lifecycle.**
  `.brokkr/ratatoskr/<test>/run-N/` with collision-incrementing N,
  preserve-on-failure, delete-on-success-unless-`--keep-artefacts`,
  preserve-on-panic (Drop default).
- **Orchestrator-side process-tree primitives** (signal, pid_is_alive,
  sentinel watch with named backstop, /proc snapshot tolerant of
  CAP_SYS_PTRACE). These are useful for brokkr's own hang cleanup
  and for implementation sharing later, but they are **not** directly
  callable from Lua scripts. V1 implements equivalent script-visible
  bindings in ratatoskr's harness module and does not add a
  brokkr/runtime control channel.
- **Script discovery.** `crates/app/tests/service-harness/**/*.lua`,
  top-of-file `-- key: value` frontmatter (`description`,
  `expected = pass | ignored`, `ceiling = 60s`,
  `preserve_data_dir = on_success_too`). Discovery is recursive so
  cohorts can live under `t1/`, `extract/`, etc.; non-`.lua` files
  such as fixtures are ignored by extension. `preserve_data_dir` is
  brokkr-side frontmatter because brokkr owns artefact-dir deletion.
- **Soak (`-N`) and suite (`--filter`) runners** on top of the
  single-script run path.
- **History-DB recording, optional sidecar /proc profiling.**

### What ratatoskr provides (in `app`'s harness module)

- **Embedded Lua VM** (`dellingr`) running test scripts.
- **`ServiceClient` userdata** - Lua scripts construct one via
  `harness.spawn(args)` or `harness.spawn_with_events(args)` and call
  the same methods the existing `#[tokio::test]` functions call:
  `client:request("HealthPing")`, `client:request("Shutdown")`,
  `client:notifications()`, `client:current_generation()`,
  `client:child_pid()`, `client:shutdown()`, `drop(client)`.
  The `request` binding is registry-backed: Rust owns a
  request/response registry that maps Lua method names and argument
  tables onto `RequestParams` variants, decodes the typed Rust
  response, and returns a plain Lua table. Bad method names,
  malformed argument tables, and mismatched response shapes fail in
  Rust with a structured harness error.
- **`SpawnEvent` receiver userdata** - `events:next(timeout_secs)`
  returns one of `ChildSpawned { client }`, `BootReady { response }`,
  `Terminal { error }`. The classification logic stays inside
  `ServiceClient`; the binding does not synthesise events.
  `SpawnEvent`, `ClientError`, `BootClassification`, and
  `SchemaVersionChanged { was, now }` are exposed as typed userdata
  so scripts can pattern-match without parsing strings.
- **`NotificationQueue` userdata** - `queue:recv(timeout)` /
  `queue:drain_for(duration)` return `Notification` userdata that
  scripts inspect for `service_generation`, `method`, etc.
  Notification payloads are the exception to typed request/response
  decoding: they expose a `serde_json::Value`-backed Lua view for
  `params`, so scripts can filter on `notif.method == "X"` and
  inspect varied payload details without one typed shell per
  notification.
- **Deterministic wait combinator** - exposed as
  `harness.wait_for { predicate, child = client, backstop = "30s" }`.
  Every wait races the predicate against `client:observe_child_exit()`
  internally; failure verdicts name which fired.
- **Quiet observation combinator** - exposed as
  `harness.expect_quiet { predicate, child = client, window = "2s" }`.
  This is the absence assertion shape. The window expiring without
  the predicate firing is success; child termination still
  short-circuits with a named verdict.
- **Process orchestration not covered by `ServiceClient`** -
  process-group spawn for non-`ServiceClient` children (the
  `parent_death_helper` binary, future stub helpers); SIGKILL/SIGTERM
  to a named PID; data-dir snapshotting; sentinel-file watch
  (`harness.wait_for_sentinel { path = "...", backstop = "5s" }` for
  data-dir-relative paths and
  `harness.wait_for_sentinel { absolute = "/...", backstop = "5s" }`
  for explicit absolute paths; no leading-slash auto-detection and
  no glob support in v1); JSON-RPC frame log for diagnostic purposes
  (taps the wire underneath `ServiceClient`, not the primary test
  surface). These script-visible primitives are implemented in
  ratatoskr's harness module. Brokkr has similar helpers for its own
  cleanup path, but there is no brokkr/runtime control channel in v1.
- **Artefact-dir writers** - the frame log, event log, step trace,
  Service-specific `/proc` snapshot, data-dir copy, `service.stderr`,
  and `runtime-outcome.json` are populated by the harness module into
  the directory pointed at by `BROKKR_HARNESS_ARTEFACT_DIR`. Brokkr
  owns the directory lifecycle and writes brokkr-owned files
  (`run.toml`, `binary-stdout.log`, `binary-stderr.log`, copied
  script, `spawn-error.txt` on spawn failure). The runtime must not
  claim ownership of those same files unless the contract is changed.
  `service.stderr` is a v1 artefact: the harness uses a Service spawn
  path that pipes the child Service's stderr to that file instead of
  inheriting it.
- **Per-script wall-clock backstop** - runaway scripts are bounded
  by a per-script wall-clock ceiling enforced around the whole run.
  Scripts may set frontmatter `-- ceiling: 60s`; omitted scripts use
  a sane default ceiling. dellingr's per-opcode cost accounting is
  **not** used for this - the cost budget is structurally unable to
  bound wall-clock execution (`while true do end` is free by design).
  Wall-clock is the right mechanism for runaway abort, same shape as
  every other backstop in the harness.
- **`app --test-harness <script.lua>` CLI flag** - the runtime's
  entry point, gated behind the existing `test-helpers` feature so
  production builds never carry the Lua VM.

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

Tests are Lua scripts. Ratatoskr's `app` crate embeds the
`dellingr` Lua VM (`0.2.0` on crates.io) and exposes its existing
`crates/app/src/service_client.rs` API as Lua userdata in the
harness module. Brokkr never embeds dellingr; it spawns the
ratatoskr-side runtime via `app --test-harness <script.lua>`.
Adding a test means adding a `.lua` file in ratatoskr's tree; no
brokkr rebuild, and no harness-module rebuild either unless the
new test exercises a Lua API surface that does not exist yet.

Why Lua via `dellingr`:

- Pure Rust, no FFI, no system Lua dep.
- `HostCallbacks` redirects `print()` to per-test capture and hooks
  errors for the failure dump.
- `RustFunc` is the existing pattern for exposing Rust functions to
  Lua; `ServiceClient` methods plug in directly as userdata methods.
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
- **Absence over observation window** - "no event received in N
  seconds, after a known transition." Scripts use
  `harness.expect_quiet { predicate, child, window }`; the window
  expiring is the expected success verdict, not a harness-timeout
  failure. Used by
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
- **Sentinel-file watch** -
  `harness.wait_for_sentinel { path = "clean_shutdown", backstop = "5s" }`
  for data-dir-relative paths, or
  `harness.wait_for_sentinel { absolute = "/var/run/foo", backstop = "5s" }`
  for explicit absolute paths. Required for the `clean_shutdown`
  sentinel in manual-matrix items 4 and 5; available for any future
  test that benefits from a non-clock readiness signal.
- **Frame log** - captured under the hood via ServiceClient's wire
  layer (the harness taps stdin/stdout). Diagnostic only; emitted to
  the artefact dir on failure. Tests do not pattern-match on raw
  frames - they pattern-match on `ServiceClient` return values and
  `Notification` userdata.
- **Backstop policy** - explicit, named, generous. Safety-backstop
  firing in `wait_for` is a test-design bug, not a flake.
  Observation-window expiry in `expect_quiet` is a success verdict
  for absence assertions.

#### Cohort coverage table

| Test | Surface used |
| --- | --- |
| `dispatch_in_process.rs` tests using `spawn_harness_with_suffix` | Migrates as a cohort because they share the same IO-boundary wait failure mode even though they use `tokio::io::duplex` instead of an OS subprocess. V1 rewrites the boot/dispatch lifecycle coverage onto the real-subprocess `ServiceClient` path; in-process Lua mode is deferred until a future test needs it. |
| `boot_ready_returns_after_sequence_completes` | Current example of the cohort failing under `brokkr check`; needs frame/step trace around `boot.ready`, boot shared state, and shutdown drain rather than only an outer libtest timeout. |
| `health_ping_succeeds_during_long_migration` / `health_ping_works_concurrently_with_boot_ready` | Need concurrent request driving while `boot.ready` is parked; Lua API may need explicit background request / parallel request primitive, not just sequential `client:request`. |
| `boot_ready_blocks_until_sequence_completes` / `boot_progress_notifications_emitted_in_order` (currently ignored) | Existing in-process harness hangs; migrate with the same diagnostic artefact contract as the subprocess wedge. |
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

When a test fails, brokkr preserves a self-contained artefact directory
at `.brokkr/ratatoskr/<test-name>/<run-N>/`. Brokkr creates the
directory and writes brokkr-owned run metadata; the ratatoskr runtime
writes Service/runtime diagnostics into the same directory. The target
contents are:

- **`frames.jsonl`** - every JSON-RPC frame, both directions,
  timestamped from spawn. Single most useful artefact for
  drain-ordering / framing bugs. Versioned JSONL records (each line
  carries `schema = 1`); writers emit `raw_redacted` rather than raw
  payload bytes so credentialed scripts can land later without
  schema churn.
- **`events.jsonl`** - every spawn/runtime event observed
  (`ChildSpawned`, `BootReady`, `Terminal`), timestamped.
- **`steps.jsonl`** - the test's step trace: which step was active,
  what condition was awaited, which transition fired
  (`predicate` / `child_exit` / `backstop` / `window_expired`).
- **`service.stderr`** - Service's stderr verbatim. Captured per-run,
  not race-mingled with test stdout. V1 requires a harness-specific
  Service spawn path that pipes stderr to this file, because today's
  `ServiceClient::launch_subprocess` inherits stderr.
- **`proc-{status,wchan,syscall,stack}.txt`** - snapshot of
  `/proc/<pid>/status`, `/proc/<pid>/wchan`, `/proc/<pid>/syscall`,
  `/proc/<pid>/stack` at the moment failure was declared.
  Distinguishes "blocked on futex" from "blocked on closed pipe"
  without re-running.
- **`data-dir/`** - copy of the test's app-data dir at failure time.
  SQLite WAL state, lockfile presence, key file, `clean_shutdown`
  sentinel.
- **`runtime-outcome.json`** - runtime-side exit reason
  (clean / harness-killed-on-backstop / child-exited / signal / etc.).
- **`run.toml`** - brokkr-owned reproducibility metadata: test script,
  env vars, brokkr version, git commit, sweep label, exit code/signal.
  Brokkr also writes `binary-stdout.log` / `binary-stderr.log` (piped
  child output), a copied script, and `spawn-error.txt` on spawn
  failure.

Ownership: brokkr writes `run.toml`, `binary-stdout.log`,
`binary-stderr.log`, the copied script, and `spawn-error.txt`. The
ratatoskr runtime writes everything else. Brokkr does not parse
runtime-owned artefacts in v1, just preserves them. Trace-schema
details (record shapes, redaction posture) are documented in
`<ratatoskr>/docs/harness/architecture.md`.

On success, the artefact directory is deleted unless
`--keep-artefacts` is passed.

The data dir copy, protocol/step trace, and `/proc` snapshot for real
subprocesses are the pieces of state that today's tokio-test pattern
destroys or never records (`DataDirGuard::Drop` / `TestDataDir::Drop`
unconditional cleanup; no structured frame/step capture). Recovering
them is the largest single jump in debug ergonomics.

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

Most of the *types* the harness binding wraps already exist; the
binding itself does not. Phase 8's largest single ratatoskr-side
build is the `harness` module in the `app` crate (see "What
ratatoskr provides" above). The pre-existing surface the binding
sits on top of:

- **`ServiceClient` and friends** already have the public API the
  Lua binding wraps - `spawn_for_test`, `spawn_with_events_for_test`,
  `request`, `notifications`, `current_generation`, `child_pid`,
  `shutdown`, plus the `SpawnEvent` and `ClientError` enums with
  their classification fields. No new `ServiceClient` method needed
  for v1.
- **Test-helper argv flags** already exist:
  `--test-fake-version=N`, `--test-hang-on-stdin-eof`. Phase 8 calls
  for `--test-fake-schema=N` (used by
  `test_fake_schema_propagates_via_terminal`); the harness module
  also adds `--test-harness <script.lua>` (the runtime entry point).
- **Test-helper `RequestParams` variants** already exist:
  `TestPrintln { message }`, `TestSlow { millis }`. Phase 8's
  named cohort needs additional variants for fault injection
  (`TestCrashAfterNWrites { ... }` or similar) and counter probes
  (`TestCounterRead { ... }`); names and shapes are Phase-8 design
  work, not harness design work. Each new `RequestParams` variant
  is automatically usable from Lua once the binding's `request<R>`
  wrapper covers the full enum.
- **`parent_death_helper` binary** already exists, registered as
  `CARGO_BIN_EXE_parent_death_helper`. The harness module invokes
  it directly via std::process; brokkr's `[[check]]` sweep ensures
  it's pre-built (declare it in `build_packages`).
- **Account seeding** (Phase 8 named harness gap) lands as a new
  test-helper `RequestParams` variant (`TestSeedAccount { ... }`)
  when T1 tests start needing FK-constrained writes. Picked up by
  the Lua binding automatically.

What needs to be newly built on the ratatoskr side:

- The harness module itself in `app` (Lua VM bootstrap, RustFunc
  wrappers, userdata, wait combinator, artefact-dir writers, frame
  log tap). See "What ratatoskr provides" above.
- The `--test-harness <script.lua>` CLI flag (gated behind the
  existing `test-helpers` feature).
- A `dellingr` workspace dep at `0.2.0` (already on crates.io;
  pulling it in is one Cargo.toml line, not an upstream wait).
- The `crates/app/tests/service-harness/` directory plus the first
  cohort of `.lua` scripts.
- The Phase-8 `RequestParams` / argv additions named above, as the
  cohort needs them.

What is *not* built on the ratatoskr side and not delegated to
brokkr either:

- Brokkr will not be a source-level dep of `app` (or any other
  ratatoskr crate). The harness module owns its own protocol
  classification via `ServiceClient`; brokkr only spawns the
  resulting binary.

Optional ratatoskr-side refactor unrelated to harness correctness:

- Carving `ServiceClient` and friends into a slim sub-crate (e.g.
  `crates/service-client`). Tidies compile-time scope but is not
  on the critical path - the Lua bindings work fine in either
  layout. Phase 8 may surface a natural seam.

## Acceptance for v1

1. Brokkr orchestrates `service-test` against a built `app` binary,
   gated to `Project::Ratatoskr`; the harness module ships inside
   ratatoskr's `app` crate behind the `test-helpers` feature.
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
   It does not require rebuilding the harness module either, unless
   the test exercises a Lua API surface that does not exist yet.

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

Architecture decision is made (option B). Brokkr-internal scaffolding
landed across two commits; the next pieces split between brokkr-side
plumbing (still independently buildable) and ratatoskr-side harness
module work (waiting on Phase 8). Annotations below ride alongside
each step in the suggested order.

Brokkr-side, in tree:

- `Project::Ratatoskr` is a first-class variant. `project = "ratatoskr"`
  in `brokkr.toml` resolves to it; `[ratatoskr]`-tagged commands
  show up grouped in `brokkr --help`; `project::require()` gates
  cleanly with the conventional error message.
- `brokkr service-test <SCRIPT>` is wired end-to-end on the brokkr
  side: project-gated, validates the script path, parses the
  script's frontmatter once (so `ceiling` and `preserve_data_dir`
  apply uniformly across soak iterations), acquires the global
  lockfile, runs the sweep-aware build (see below), allocates a
  per-run artefact dir, then spawns
  `<binary> --test-harness <SCRIPT>` with
  `BROKKR_HARNESS_ARTEFACT_DIR` and `BROKKR_TEST_BIN_DIR` set in
  the env. Stdout/stderr drain in background threads while a
  50ms-cadence poll loop watches `Child::try_wait` and the
  ceiling deadline. The captured streams land in
  `binary-stdout.log` / `binary-stderr.log` next to a copy of the
  script and a `run.toml` (brokkr version, sweep label, exit code
  or signal, elapsed_ms, git commit/dirty when collectable). When
  the ceiling fires brokkr SIGKILLs the child and labels the run
  `ceiling=<spelling>`; otherwise the label tracks the child's
  own exit. Success deletes the artefact dir unless
  `--keep-artefacts` is set or the script's frontmatter has
  `preserve_data_dir = on_success_too`; failure preserves it.
  Spawn errors (binary missing) drop a `spawn-error.txt`
  breadcrumb and preserve the dir. History-DB recording is
  automatic via `main()`'s `record_history`. `--debug` flips the
  build to the dev profile; default is release. Until ratatoskr
  ships the harness module, `app --test-harness` errors out with
  "unknown flag" and the artefact dir captures that faithfully -
  the brokkr side is structurally ready for the wedge tests the
  moment the ratatoskr-side runtime lands.
- Soak via `brokkr service-test <SCRIPT> -N <COUNT>`. Builds once,
  loops `COUNT` iterations with one fresh artefact dir per iteration
  (`run-1/`, `run-2/`, ...). Default bails on the first failed
  iteration so the artefact dir for triage lands fast; `--keep-going`
  runs every iteration regardless. Per-iter status line
  (`iter N/total: PASS in Xms` / `iter N/total: FAIL exit=Y in Zms
  (artefacts: ...)`) plus a trailing summary: all-passed reports
  min/max/avg elapsed; bail reports "stopped at iter F/total";
  keep-going lists every failed iteration index. Exit code is
  non-zero if any iteration failed.
- Sweep-aware harness build (`src/ratatoskr/build.rs`). Reads
  `[ratatoskr.harness] sweep / binary` out of `brokkr.toml`,
  matches `sweep` against `[[check]]`, builds every
  `build_packages` entry through `crate::build::cargo_build` with
  the sweep's feature flags, returns the path to the
  `binary`-package executable plus the bin dir for sibling
  helpers. Same feature contract `brokkr check` enforces - a
  script can never run against a feature combination the rest of
  the toolchain has not validated. Cross-checks at parse time:
  unknown `sweep` and binary-not-in-`build_packages` both error
  before cargo runs.
- `brokkr service-suite [--filter X]` runs every discovered script in
  sequence against a single shared harness build. Discovery + filter
  (substring match against the relative name) happens before the build
  so an empty selection bails with a useful message - "no scripts
  found", "filter matched none", "all matches are ignored", etc. -
  before paying the cargo cost. `expected = ignored` scripts are
  skipped by default; `--include-ignored` opts them in. Each script
  reuses the same `spawn_and_capture` path `service-test` uses, so
  per-script artefact-dir lifecycle, ceiling, and `preserve_data_dir`
  semantics are identical. Default is stop-on-first-failure (the
  failing script's artefacts land fast for triage); `--keep-going`
  runs every selected script and the trailing summary lists the
  failing names. `--keep-artefacts` and `--debug` mirror
  `service-test`. Exit code is non-zero if any selected script
  failed. `--filter` is a substring match (no glob), keeping the CLI
  surface deliberately small until a use case justifies more.
- `brokkr service-list` discovers
  `crates/app/tests/service-harness/**/*.lua` recursively under the
  project root, parses a top-of-file `-- key: value` frontmatter,
  prints a sorted table. Recognized fields: `description`,
  `expected = pass | ignored`, `ceiling = 60s` (wall-clock backstop
  with `ms` / `s` / `m` / `h` suffixes; bare numbers are seconds),
  `preserve_data_dir = on_success_too`. Unknown fields are ignored.
  Display name is the path relative to the script root, minus
  `.lua` (e.g. `t1/journal_replays_after_respawn`). Empty-state
  message names the expected directory so a fresh checkout gets a
  useful response.
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

Deferred (ratatoskr-side, post Phase 8 start):

- The harness module in `app` (Lua VM bootstrap, `ServiceClient` /
  `SpawnEvent` / `ClientError` / `NotificationQueue` userdata,
  wait combinator, sentinel watch, frame-log tap, artefact-dir
  writers).
- `app --test-harness <script.lua>` CLI flag, gated behind the
  existing `test-helpers` feature.
- `dellingr 0.2.0` workspace dep.
- `--test-fake-schema=N`, `TestSeedAccount`, fault-injection /
  counter-probe `RequestParams` variants.
- The wedge tests, T1 cohort, manual-matrix items 4 / 5 expressed
  as `.lua` scripts under `crates/app/tests/service-harness/`.
- Adding `[[check]] name = "harness"` and `[ratatoskr.harness]` to
  ratatoskr's `brokkr.toml`.

Brokkr-side, not yet started but unblocked:

- `service-list --json` for machine consumption.

## Suggested implementation order

1. **Architecture commitment.** *(decided - option B: VM lives in
   ratatoskr's `app` crate, brokkr orchestrates by subprocess
   spawn. Brokkr does not depend on ratatoskr; ratatoskr does
   not depend on brokkr; ratatoskr depends on `dellingr` directly.
   See "Dependency direction" above.)*
2. **`Project::Ratatoskr` enum variant + project gating.** *(done.)*
3. **Process / sentinel / artefact primitives.** *(done on the
   brokkr side - process primitives, sentinel watcher, /proc
   snapshot, artefact directory lifecycle helper all landed in
   `src/ratatoskr/{process,artefacts}.rs`. The deadline-bounded
   spawn primitive (`run_captured_with_env_and_deadline` in
   `src/output.rs`) is also in tree - drains stdout/stderr in
   threads, polls `Child::try_wait` at 50 ms cadence, SIGKILLs on
   ceiling expiry. Frame-log tap is ratatoskr-side - lives in the
   harness module since brokkr never sees the wire.)*
4. **Lua VM embedding.** *(ratatoskr-side, deferred until Phase 8 -
   `dellingr 0.2.0` pulled in by `app`'s Cargo.toml. Brokkr does
   not embed dellingr.)*
5. **Lua binding for `ServiceClient` and friends.** *(ratatoskr-side,
   deferred until Phase 8 - in the harness module.)*
6. **Wait combinators (`wait_for` + `expect_quiet`).**
   *(ratatoskr-side, deferred until Phase 8. The simplest form
   (`wait_for_sentinel`) is in brokkr as a building block for
   orchestrator-side hang cleanup; the ServiceClient-aware variants
   (`wait_for` for positive waits, `expect_quiet` for absence
   assertions) live in the harness module alongside the userdata
   bindings.)*
7. **CLI: `brokkr service-test <SCRIPT>`.** *(done on the brokkr
   side - project gate + script validation + frontmatter parse
   (`ceiling`, `preserve_data_dir`) + sweep-aware build via
   `[ratatoskr.harness]` + `--debug` profile flag + lockfile +
   per-run artefact dir + deadline-bounded spawn that drains
   stdout/stderr in threads and SIGKILLs on ceiling expiry +
   `run.toml` + script-copy + outcome reporting (with a
   `ceiling=<spelling>` exit label when the deadline fires) +
   finalize-on-success-or-failure (with `preserve_data_dir`
   honoured). History-DB recording is automatic via `main()`.
   Until ratatoskr ships `app --test-harness`, the spawn faithfully
   captures the unknown-flag failure - the wedge tests work the
   moment the ratatoskr-side runtime lands.)*
8. **Wedge.** *(ratatoskr-side - re-express the two ignored tests
   as `.lua` scripts. Deferred until the harness module lands.)*
9. **Phase-8 ratatoskr-side additions.** *(deferred - ratatoskr
   roadmap.)*
10. **Cohort.** *(deferred - lands incrementally inside Phase 8.)*
11. **Soak / suite / list commands.** *(`service-list`, soak
    (`brokkr service-test <SCRIPT> -N <COUNT> [--keep-going]`), and
    suite (`brokkr service-suite [--filter X]`) all landed.
    `service-list` walks `service-harness/**/*.lua` recursively (so
    M4 / M5 cohorts under `t1/` and `extract/` pick up automatically)
    and parses the `description`, `expected`, `ceiling`, and
    `preserve_data_dir` frontmatter fields; the soak loop reuses the
    same per-script frontmatter so the ceiling and artefact-dir
    policy apply uniformly across iterations; `service-suite` reuses
    the same `spawn_and_capture` path so artefact lifecycle is
    identical, with substring `--filter`, default-skip-ignored
    (override via `--include-ignored`), and stop-on-first-failure
    bail (override via `--keep-going`). `service-list --json` still
    deferred (M7 polish); unblocked, not Phase-8-blocking.)*
12. **Sidecar integration.** *(deferred; not required for v1.)*

## Open questions

Resolved (kept here for review-history clarity):

- ~~**Cargo-build integration.**~~ Resolved: brokkr ships
  `[ratatoskr.harness]` referencing a `[[check]]` sweep; the
  build path reuses `crate::build::cargo_build` so feature parity
  with `brokkr check` is automatic. Cross-checked at parse time.
- ~~**Test discovery.**~~ Resolved:
  `<ratatoskr>/crates/app/tests/service-harness/*.lua`. `brokkr
  service-list` walks it and parses a `-- key: value` frontmatter.
- ~~**`dellingr` dep timing.**~~ Resolved: published as `0.2.0`
  on crates.io; ratatoskr's `app` crate takes the workspace dep
  when the harness module lands. Brokkr never depends on
  dellingr.
- ~~**Async / blocking model in Lua.**~~ Resolved: VM lives in
  ratatoskr's `app` process (which already runs tokio for the
  existing service paths). Lua RustFuncs block on tokio-driven
  waits; the Lua side calls Rust functions that block until the
  wait resolves. The `bulk_archive_200_*` parallel-dispatch case
  probably means a Rust-side `parallel_send(table_of_requests)`
  rather than Lua-level concurrency. Brokkr stays sync because
  it never sees the wire.
- ~~**Cost budget defaults.**~~ Resolved: dellingr's per-opcode cost
  accounting is **not** used for runaway-script abort. Cost-budget
  cannot bound wall-clock execution (`while true do end` is free by
  design). Runaway scripts are bounded by per-script wall-clock
  backstop set via the `-- ceiling: 60s` frontmatter, with a sane
  default for scripts that omit it.
- ~~**Sentinel-watch addressing.**~~ Resolved:
  `harness.wait_for_sentinel { path = "...", backstop = "5s" }` for
  data-dir-relative paths, or
  `harness.wait_for_sentinel { absolute = "/...", backstop = "5s" }`
  for explicit absolute paths. No leading-slash auto-detection. No
  glob support in v1.
- ~~**Backstop policy.**~~ Resolved: per-call backstop arg on every
  wait, plus a per-script wall-clock ceiling enforced around the
  whole run via the `-- ceiling: 60s` frontmatter. Safety-backstop
  firing in `wait_for` is a test-design bug; observation-window
  expiry in `expect_quiet` is the success verdict for absence
  assertions.
- ~~**Trace format stability.**~~ Resolved: schemas live in
  `<ratatoskr>/crates/app/src/harness/trace_schema.rs` as serde
  structs; each JSONL record carries `schema = 1`. Readers tolerate
  unknown fields for forward compatibility; writers bump the schema
  version only on incompatible field changes. Writers emit
  `raw_redacted` rather than raw payload bytes from day one.

Still open (mostly ratatoskr-side, settle during Phase 8):

- **Lua API surface naming and shape.** The capabilities list
  pins semantics; the function names, argument conventions, and
  return shapes are open. Settle as the binding is implemented.
- **Concurrency between scripts.** Does the suite runner run
  scripts in parallel? Default no - subprocess tests touch real
  files and ports. Add `--jobs N` later if a class of scripts
  opts in. (Brokkr-side decision.)
- **Slim crate carve-out.** Should `ServiceClient` and friends
  carve into `crates/service-client` before the harness module
  lands, or stay in `app`? Either layout works for the Lua
  bindings; a slim crate is a compile-time-hygiene call. The M1
  call (per the ratatoskr roadmap) is to keep `ServiceClient` in
  `app` until a second crate genuinely needs it or compile-time
  profiling shows pressure.

## Non-goals worth restating

The harness is not a replacement for ratatoskr's correctness
assertions, not a JSON-RPC server, not a CI-only tool, not a
benchmark framework, not a cargo-test wrapper. It is one thing: a
deterministic runtime for subprocess-lifecycle tests, with a
failure dump that lets a developer diagnose hangs without
re-running.
