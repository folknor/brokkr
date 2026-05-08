# Ratatoskr sync orchestration - planning note

Status: **planning, post-option-B reconciliation.** Brokkr-side
commands for driving sync workloads against sæhrimnir. Plan 1 (the
Service-test harness) and plan 2 (sæhrimnir itself) are no longer in
this notes tree - plan 1's brokkr-side scaffolding shipped and its
cross-cutting design lives at
`<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`;
plan 2 shipped on the sæhrimnir side and its repo
(`/home/folk/Programs/sæhrimnir/`) is authoritative for protocol
surface, fixture model, reactive callbacks, sentinel/argv contract,
and signal handling. Depends on sæhrimnir, already bootstrapped at
`/home/folk/Programs/sæhrimnir/` and
shipping all five protocols (JMAP, IMAP, SMTP, Microsoft Graph,
Gmail) for v0; the gating dependency for plan-3 commands like
`sync-smoke` and `sync-bench` is plan 1's ratatoskr-side runtime
landing during Phase 8. This rev assumes plan 1's option-B
architecture: ratatoskr's `app` crate hosts the Lua VM + ServiceClient
userdata + wait combinator + frame-log tap + artefact writers; brokkr
provides build orchestration + artefact-dir lifecycle + low-level
primitives. Sync workloads ride the same harness binary, just with
sync-shaped Lua scripts instead of service-lifecycle-shaped ones.

## Background for reviewers

Throughout this document, `<ratatoskr>` refers to the root of the
ratatoskr repository (typically `~/Programs/ratatoskr` on the
author's machine, sibling to brokkr's checkout). All other paths are
relative to brokkr's repo unless otherwise stated.

**Brokkr** is a single-binary Rust dev tool installed via
`cargo install --path ~/Programs/brokkr`. Invoked from any project
root, it reads `./brokkr.toml` to detect which project it's in
(`pbfhogg`, `elivagar`, `nidhogg`, `litehtml-rs`, `sluggrs`) and
exposes flat, project-gated top-level subcommands tagged `[pbfhogg]`,
`[nidhogg]`, etc. Brokkr already orchestrates server-shaped projects
(`brokkr serve` builds and runs nidhogg; analogous bench commands
spin up children, capture metrics, store rows in
`.brokkr/results.db`). This plan extends that to ratatoskr.

**Ratatoskr** is a Rust desktop email client. Two long-running OS
processes: an `iced` UI and a child Service worker that owns all
writes (sync, action execution, DB writes, body/inline/blob stores,
Tantivy indexing, push receivers). Ratatoskr's Service runs sync
workloads against five providers (JMAP, IMAP, SMTP, Microsoft Graph,
Gmail); correctness and performance of those workloads are the
target of this plan.

**Sæhrimnir** (`/home/folk/Programs/sæhrimnir/`, plan 2) is the mock
peer ratatoskr's Service talks to under test. Single binary, single
process; binds one TCP port per protocol; one fixture in (TOML or
Lua), five wire shapes out, byte-stable across runs. Sæhrimnir's own
`README.md` / `CLAUDE.md` / `notes/` are authoritative for internals;
the brokkr-side cross-process contract (argv, sentinel format, env
vars, SIGTERM budget) is captured in this note's "Cross-process
contract" section below.

**Three-plan architecture this note lives inside.** The work needed
to test ratatoskr's sync code splits into three independent pieces:

| | Owns |
| --- | --- |
| Plan 1 (split across brokkr + ratatoskr) | Deterministic subprocess test harness. Ratatoskr's `app` crate hosts the Lua VM + `ServiceClient` userdata + wait combinator + frame-log tap + artefact writers, exposed via `app --test-harness <script.lua>`. Brokkr provides build orchestration + artefact-dir lifecycle + low-level primitives (signal, pid_is_alive, sentinel watch, /proc snapshot). Brokkr does not depend on ratatoskr or embed dellingr. Cross-cutting design at `<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`. |
| Plan 2 (sæhrimnir, bootstrapped at `/home/folk/Programs/sæhrimnir/`) | A deterministic mock peer for all five email protocols ratatoskr's sync code talks to (JMAP, IMAP read-path, SMTP submission, Microsoft Graph mail-sync, Gmail mail-sync). One fixture in (TOML or Lua), five wire shapes out, byte-stable. Reactive `on(...)` callbacks across all five protocols plus `wait` / `mock_done` / `mock_fail` script controls. Independent of brokkr and ratatoskr. The sæhrimnir repo is authoritative for protocol surface and fixture model. |
| Plan 3 (this note, in brokkr) | Commands that spawn sæhrimnir and ratatoskr's harness binary (`app --test-harness <sync_script.lua>`) together, drive a sync workload via the script, collect metrics, store results. The Lua script speaks JSON-RPC to the Service via plan 1's `ServiceClient` userdata; brokkr never touches the wire. |

The three plans are independent in source - sæhrimnir is a separate
Rust project with its own crate; plans 1 and 3 are subcommands inside
brokkr; ratatoskr depends on none of them. Brokkr depends on neither
ratatoskr nor sæhrimnir at the source level - it spawns binaries and
talks line-delimited HTTP / TCP only.

## Problem

Ratatoskr's Service runs sync workloads (IMAP, JMAP, future providers).
Exercising them needs:

- known fixture content (small mailbox, 100k messages, malformed MIME,
  many-folder, huge-thread, etc.);
- deterministic peer behaviour (no flaky network, no rate-limited
  public test servers);
- failure injection (disconnects, retryable errors, slow responses);
- repeatable timing for performance regression tracking.

Cargo tests can't carry this. A sync workload involves a subprocess
(the Service), a network peer (the mock server), and assertions across
both. Today there's no in-tree way to run that.

## Three-piece architecture

This table summarises the boundaries from plan 3's perspective; see
the table at the top of this note for the cross-plan ownership view.

| | Owns |
| --- | --- |
| Plan 1 | Harness binary. Ratatoskr-side: Lua VM, `ServiceClient` userdata, wait combinator, frame-log tap, artefact writers, `app --test-harness` CLI flag. Brokkr-side: artefact-dir lifecycle, sentinel watch, /proc snapshot, signal / pid_is_alive primitives. |
| Plan 2 (sæhrimnir) | Mock JMAP / IMAP / SMTP / Graph / Gmail server: protocol implementations, fixture model, deterministic responses, scenario-control surface (override callbacks, `wait`, `mock_done`, `mock_fail`). Independent of brokkr and ratatoskr. |
| Plan 3 (this note, in brokkr) | Commands that build + spawn sæhrimnir alongside plan 1's harness binary, thread the per-protocol mock endpoints into the harness via env vars, let the script drive sync, collect wall-clock + sidecar metrics, store results. |

Boundary rules:

- Sæhrimnir owns protocol code, fixture loading, port binding,
  scenario-control / override callbacks. Brokkr does not parse
  fixture files; it just passes the path on the command line.
- Ratatoskr owns sync code, correctness assertions, the test-only
  `RequestParams` surface used to drive sync workloads
  (`TestStartSync`, `TestQueryDbState`, etc., named at Phase 8 design
  time), and the `.lua` scripts that exercise them.
- Brokkr owns spawn/teardown of both children, sentinel-driven port
  discovery, env-var threading, wall-clock timing of the harness
  binary's lifetime, artefact-dir aggregation, results storage.
  Backstop timing inside the script lives in plan 1's harness
  module, not in brokkr.

Plan 3 contains no protocol code, no fixture content, no sync
correctness logic, no JSON-RPC client. If something feels like one of
those, it belongs in sæhrimnir, in ratatoskr, or inside the Lua
script.

## Reuse from plan 1

Plan 1 splits across brokkr and ratatoskr's `app` crate; plan 3 reuses
both halves.

Brokkr-side primitives carried over verbatim (already in tree per
plan 1's "implementation status"):

- Process spawn + signal + `pid_is_alive` (now driving two children -
  sæhrimnir and the harness binary).
- Sentinel-file watch (`wait_for_sentinel`) - sæhrimnir writes its
  per-protocol port table before sync starts.
- `/proc` snapshot - applied to both children's PIDs at failure;
  writes the four-file `proc-{status,wchan,syscall,stack}.txt` set.
- Artefact-dir lifecycle (`ArtefactDir`) - the same allocator, just
  pointed at `.brokkr/ratatoskr/sync/<test>/run-N/` instead of the
  service-test root.
- Deadline-bounded spawn
  (`output::run_captured_with_env_and_deadline`) - usable for
  bounding the harness binary's wall-clock the same way `service-test`
  does, when `sync-bench` lands.

Ratatoskr-side machinery (in `app`'s harness module, lands during
Phase 8 alongside the rest of the harness work):

- The harness binary itself (`app --test-harness <script.lua>`).
- `ServiceClient` userdata - the Lua script speaks to the Service via
  the same Rust-level API the existing `#[tokio::test]` bodies use,
  including all `RequestParams` variants Phase 8 adds for sync
  triggering and assertion.
- Wait combinator inside the script (predicate vs child-exit vs named
  backstop).
- Frame-log tap, event-log writer, artefact-dir writers - one
  artefact dir per harness invocation, populated from inside ratatoskr.

Plan 3 depends on the brokkr-side primitives shipping (done) and on
the ratatoskr-side harness module shipping (Phase 8). Sync-smoke
cannot land before the harness module exists.

## New primitives

- **Sentinel-driven port discovery.** Sæhrimnir always binds five
  listeners (one per protocol) and writes a multi-line readiness
  sentinel with `<NAME> <port>` per protocol. Brokkr's existing
  `wait_for_sentinel` waits for presence; a new helper parses the
  file content and returns a `{ jmap: u16, imap: u16, smtp: u16,
  graph: u16, gmail: u16 }`-shaped struct. No port-pool allocator
  is needed for v0 because every port is ephemeral and chosen by
  the kernel.
- **Multi-child orchestration.** Sæhrimnir first (with readiness
  sentinel), harness binary second (inheriting the per-protocol
  endpoints via env vars). Tear down in reverse on success or
  failure. The workload-driver process is the harness binary
  itself; no third child.
- **Endpoint injection.** The harness binary (and through it, the
  Service it spawns) is told the mock endpoints via the
  `RATATOSKR_TEST_{JMAP,IMAP,SMTP,GRAPH,GMAIL}_ENDPOINT` env-var
  family, read under ratatoskr's existing `test-helpers` feature
  gate. The exact spellings are configurable from ratatoskr's
  `brokkr.toml` via `[ratatoskr] test_endpoint_env_<proto>` so
  brokkr does not hardcode them. URL shapes - HTTP origins for
  JMAP / Graph / Gmail (e.g. `http://127.0.0.1:<port>`), `host:port`
  for IMAP / SMTP - mirror what ratatoskr's existing client code
  expects.
- **Bench timing via marker FIFO.** For `sync-bench`, the Lua
  script emits `SYNC_START` and `SYNC_END` markers via brokkr's
  existing `BROKKR_MARKER_FIFO` protocol (already used by the
  sidecar). The sidecar picks up wall-clock spans for those markers
  without needing to peek at JSON-RPC. The script also writes a
  small JSON summary (e.g. `summary.json`) into
  `BROKKR_HARNESS_ARTEFACT_DIR` for richer metrics
  (`provider_request_count`, `final_db_size_bytes`,
  `messages_synced`); brokkr reads it post-exit and joins it with
  the marker spans + sidecar samples in the results DB.

## Commands

Top-level commands tagged `[ratatoskr]`, project-gated to
`Project::Ratatoskr`. Flat namespace, matching pbfhogg/elivagar/nidhogg.

- `brokkr mock-serve --fixture <NAME>` - spawn sæhrimnir standalone,
  print listening endpoints (one per protocol), run until ctrl-C.
  Manual-exploration tool, not a test command. Does not touch the
  harness binary. Tears down cleanly on signal. Sæhrimnir always
  binds all five listeners; no per-protocol opt-in / opt-out flag
  is needed at the brokkr level.
- `brokkr sync-smoke <SCRIPT>` - build sæhrimnir + harness binary,
  spawn sæhrimnir first (waiting for readiness sentinel), parse the
  per-protocol ports out of the sentinel content, spawn `app
  --test-harness <SCRIPT>` second with the
  `RATATOSKR_TEST_*_ENDPOINT` env-var family injected, wait for the
  harness binary to exit. PASS/FAIL based on the harness binary's
  exit code; assertions live inside the Lua script. No metrics
  storage. The sync analogue of plan 1's single-script run.
- `brokkr sync-bench <SCRIPT> [--bench N]` - same spawn shape as
  sync-smoke, measured. Wall-clock spans come from `SYNC_START` /
  `SYNC_END` markers the script emits via `BROKKR_MARKER_FIFO`;
  richer metrics come from a `summary.json` the script writes into
  `BROKKR_HARNESS_ARTEFACT_DIR`. Best-of-N stored in
  `.brokkr/results.db` via the existing `BenchHarness`. Comparable
  via `brokkr results --compare`.
- `brokkr sync-list` - discover sync-test scripts (analogous to
  `service-list`).

Not in v1: `sync-replay`, threshold-based regression failure. Land
when concrete demand surfaces.

## Sync-test scripts

Scripts live in `crates/app/tests/sync-harness/*.lua` (sibling to
plan 1's `service-harness/` directory). Same frontmatter format as
service-test scripts - top-of-file `-- key: value` lines parsed by
brokkr. Recommended fields:

```lua
-- description: Cold initial sync against jmap-small fixture
-- expected: pass
-- fixture: jmap-small
-- protocol: jmap
```

`fixture` resolves against `[ratatoskr] fixtures_dir` and points
brokkr at sæhrimnir's fixture file. `protocol` is informational
(used to select which `RATATOSKR_TEST_*_ENDPOINT` env var the script
will read most heavily); sæhrimnir always binds all five listeners
regardless. Scripts themselves use plan 1's `ServiceClient` userdata
to drive sync and assert; brokkr only sets endpoint env vars and
watches exit codes.

`brokkr sync-list` walks the directory, parses the frontmatter, prints
a sorted table. Empty-state message names the expected directory so a
fresh checkout (no harness module yet in ratatoskr) gets a useful
response.

## brokkr.toml configuration

Plan 3 adds optional fields to the existing `[ratatoskr]` section (the
table itself is reserved by plan 1; only `[ratatoskr.harness]` is
defined today).

```toml
[ratatoskr]
mock_server_binary = "../sæhrimnir/target/release/saehrimnir"
fixtures_dir = "../sæhrimnir/fixtures"
test_endpoint_env_jmap = "RATATOSKR_TEST_JMAP_ENDPOINT"
test_endpoint_env_imap = "RATATOSKR_TEST_IMAP_ENDPOINT"
test_endpoint_env_smtp = "RATATOSKR_TEST_SMTP_ENDPOINT"
test_endpoint_env_graph = "RATATOSKR_TEST_GRAPH_ENDPOINT"
test_endpoint_env_gmail = "RATATOSKR_TEST_GMAIL_ENDPOINT"
sync_script_dir = "crates/app/tests/sync-harness"  # optional override

[ratatoskr.harness]
sweep = "harness"   # already declared by plan 1
binary = "app"      # already declared by plan 1
```

Fixture names referenced by sync-test script frontmatter resolve
relative to `fixtures_dir`. The sæhrimnir binary path resolves
relative to `brokkr.toml`. The five endpoint env-var names are each
configurable so brokkr doesn't hardcode ratatoskr's test-only flag
spelling - missing fields are treated as "this protocol is not
exposed to scripts in this checkout." `sync_script_dir` defaults to
`crates/app/tests/sync-harness`; override only if the layout
changes.

Plan 3 reuses plan 1's `[ratatoskr.harness]` to know which harness
binary to spawn - no separate plan-3 harness binary entry.
Sæhrimnir is built on demand from `mock_server_binary`'s parent
project via `crate::build::cargo_build`, same model as `brokkr serve`
for nidhogg.

## Determinism

Sync inherits plan 1's determinism issues, plus:

- Sync protocol ordering (Service's sync code has internal scheduling).
- Timing-dependent backoff/retry inside the Service.
- Network jitter (negligible on localhost but nonzero).

Same wait-combinator rule: every wait races against observable
terminal state. Wall-clock backstops are explicit, generous, and only
fire when something else is wrong.

For `sync-bench`, timing measurements are the point - the determinism
concern flips to "the measurement is repeatable." Standard bench
hygiene: warmup runs discarded, best-of-N reported, sidecar `/proc`
samples carry peak RSS / IO / scheduler stats.

## Failure model

The harness binary writes its own artefact dir
(`BROKKR_HARNESS_ARTEFACT_DIR`, populated by ratatoskr's harness
module). Plan 3 places that dir as a `harness/` subdirectory under
the run dir and adds a parallel `mock/` subdirectory for sæhrimnir's
artefacts; symmetric layout, no top-level filename collisions.

`.brokkr/ratatoskr/sync/<test>/run-N/`:

- `run.toml` - plan-3 metadata (brokkr-written): fixture name,
  protocol, brokkr version, sæhrimnir commit, ratatoskr commit,
  sync-script path, harness exit summary, mock exit summary.
- `harness/` - `BROKKR_HARNESS_ARTEFACT_DIR`. Plan-1 contract:
  brokkr writes `run.toml`, `binary-stdout.log`, `binary-stderr.log`,
  the copied script, and `spawn-error.txt` on spawn failure;
  ratatoskr's harness module writes `frames.jsonl` (UI <-> Service
  JSON-RPC), `events.jsonl`, `steps.jsonl`, `service.stderr`,
  `proc-{status,wchan,syscall,stack}.txt`, `data-dir/`, and
  `runtime-outcome.json`. For sync-bench runs the harness module
  also writes `summary.json` - the per-run metrics dump
  (`provider_request_count`, `messages_synced`, `final_db_size_bytes`,
  etc.) - which brokkr reads post-exit and joins with the marker
  spans + sidecar samples in the results DB.
- `mock/` - sæhrimnir-side dump (brokkr-written). Sæhrimnir does
  not see this dir; brokkr captures everything from outside.
  - `stderr.log` - sæhrimnir's stderr verbatim (its primary log
    channel).
  - `outcome.toml` - exit code, signal, wait time, fixture name.
    Brokkr writes this on teardown.
  - `proc-{status,wchan,syscall,stack}.txt` - `/proc` snapshot
    taken by brokkr's `snapshot_proc` at failure-declaration time.
    Same four-file shape plan 1's harness side uses, just rooted in
    a different subdir so the two PIDs' snapshots don't collide.

Sync-protocol traffic between Service and sæhrimnir is over TCP.
Capturing it is plan 2's concern (sæhrimnir can write request /
response logs to its stderr or to a configured file via
`--log-file`); plan 3 just preserves whatever sæhrimnir writes.

## Results storage

`brokkr sync-bench` writes rows via the existing `BenchHarness`. Initial
metric set (intentionally narrow):

- `cold_initial_sync_ms`
- `incremental_sync_ms`
- `messages_per_sec`
- `provider_request_count`
- `service_peak_rss_kb`
- `mock_peak_rss_kb`
- `final_db_size_bytes`

Compare across commits via `brokkr results --compare`. Threshold-based
gating waits until there's enough data to pick thresholds.

## Acceptance for v1

1. `brokkr mock-serve --fixture small` starts sæhrimnir, prints all
   five listening endpoints, runs until ctrl-C. Tears down cleanly.
   No harness binary involved.
2. `brokkr sync-smoke crates/app/tests/sync-harness/jmap-initial.lua`
   builds sæhrimnir + harness, spawns sæhrimnir with the script's
   declared fixture, parses the per-protocol ports out of the
   readiness sentinel, spawns `app --test-harness` with the
   `RATATOSKR_TEST_*_ENDPOINT` family injected, the script drives an
   initial sync via `client:request("TestStartSync", ...)` and
   asserts the resulting DB state via
   `client:request("TestQueryDbState", ...)`, exits 0.
3. `brokkr sync-bench <script> --bench 5` does the same five times,
   reads each run's `SYNC_START`/`SYNC_END` markers and
   `summary.json`, stores rows in `.brokkr/results.db`, prints
   best-of-5 summary.
4. `brokkr sync-list` discovers all `.lua` scripts under
   `[ratatoskr] sync_script_dir` and prints them with frontmatter.
5. IMAP / SMTP / Graph / Gmail scripts work the same way as JMAP -
   the orchestration is protocol-agnostic; only the env-var the
   script reads and the per-protocol assertions inside the script
   change. Sæhrimnir is already complete for v0 across all five
   protocols.

## Out of scope

- Sæhrimnir's protocol implementations, fixture authorship,
  scenario-control surface - plan 2.
- Ratatoskr's sync correctness assertions and test-helper RPC
  surface - ratatoskr.
- Threshold-based regression gating. Add once threshold data exists.
- Sanitized real-world protocol traces as fixtures - plan 2.
- Multi-account workloads. v1 is single-account; multi-account is a
  fixture and orchestration extension for later (sæhrimnir's
  fixture format already accommodates multi-account, but the
  protocol projection layers don't surface it yet).

## Suggested implementation order

The order separates work that can land before plan 1's harness
module exists from work that requires it.

Independent of plan 1's harness module:

1. **Sæhrimnir bootstrap** *(done - `/home/folk/Programs/sæhrimnir/`,
   all five protocols complete for v0; see plan 2)*.
2. `Project::Saehrimnir` enum variant in brokkr's `src/project.rs`
   so sæhrimnir's own `brokkr.toml` parses cleanly. Sæhrimnir's
   `TODO.md` already tracks this; needed before plan 3 commands run
   from inside sæhrimnir's tree.
3. Plan-3 `[ratatoskr]` brokkr.toml field parsing - extend
   `src/config.rs` to read `mock_server_binary`, `fixtures_dir`,
   the five `test_endpoint_env_<proto>` fields, and
   `sync_script_dir`.
4. Sentinel-content parser - small helper that reads sæhrimnir's
   multi-line readiness file and returns the per-protocol ports.
5. `brokkr mock-serve` - simplest case, one child, foreground.
   Spawns sæhrimnir with `--fixture <PATH>` + `--readiness-file`,
   prints the resolved per-protocol endpoints, waits for ctrl-C.
   No harness binary involved.

Requires plan 1's harness module (Phase 8 ratatoskr-side):

6. `brokkr sync-list` - walks `sync_script_dir`, parses frontmatter,
   prints sorted table. Can land before scripts exist (empty-state
   message); useful early.
7. `brokkr sync-smoke <SCRIPT>` - spawns sæhrimnir + harness binary,
   threads the five endpoint env vars, waits for exit. The first
   script + first `RequestParams::TestStartSync` /
   `TestQueryDbState` variants on the ratatoskr side land together
   as the wedge.
8. `brokkr sync-bench <SCRIPT>` - wraps sync-smoke in `BenchHarness`,
   adds marker-FIFO span collection and `summary.json` ingestion.
9. Per-protocol scripts grow as ratatoskr's sync coverage extends
   beyond JMAP - the orchestration stays unchanged; only the env
   var the script reads + the assertions change.

## Open questions

Resolved (kept here for review-history clarity):

- ~~**Workload trigger.**~~ Resolved: a `.lua` script invoked through
  `app --test-harness` calls a Phase-8 `RequestParams::TestStartSync`
  (or whatever the variant ends up named) on the Service via plan 1's
  `ServiceClient` userdata. Brokkr never speaks JSON-RPC.
- ~~**Workload driver location.**~~ Resolved: the harness binary
  (`app --test-harness <sync_script.lua>`) is the driver. No separate
  driver process; no JSON-RPC client in brokkr.
- ~~**Sync-script discovery directory.**~~ Resolved:
  `crates/app/tests/sync-harness/` (sibling to `service-harness/`).
  Configurable via `[ratatoskr] sync_script_dir`.
- ~~**Bench timing mechanism.**~~ Resolved: `BROKKR_MARKER_FIFO` for
  wall-clock spans, `summary.json` in the artefact dir for richer
  per-run metrics. Both, not either-or.

- ~~**Fixture format / naming.**~~ Resolved: sæhrimnir owns the
  fixture format (TOML or Lua); brokkr passes a path on the command
  line. Script frontmatter says `fixture: <name>` and brokkr resolves
  to `<fixtures_dir>/<name>.<toml|lua>` (extension picked by which
  file exists; see sæhrimnir's `notes/fixture-format.md`).
- ~~**Mock-server CLI shape.**~~ Resolved: sæhrimnir's argv is fixed
  (see plan 2). No port-pool flags needed - all five listeners
  default to ephemeral and the sentinel reports the chosen ports.
- ~~**Build orchestration for sæhrimnir.**~~ Resolved: same model as
  `brokkr serve` (nidhogg) - cargo build on demand from
  `mock_server_binary`'s parent project via
  `crate::build::cargo_build`. Implementation when plan 3 commands
  land.
- ~~**Mock-server stderr capture vs streaming.**~~ Resolved: capture
  verbatim into `mock/stderr.log` under the run dir; no live stream.
  Mirrors plan 1's `service.stderr` shape.

Still open:

- **`RequestParams` surface for sync triggering / assertion.** Phase
  8 ratatoskr-side design. Names like `TestStartSync`,
  `TestQueryDbState` used in this note are placeholders. The harness
  Lua binding picks them up automatically once they exist (no
  harness module recompile, per plan 1).
- **Per-script vs per-fixture run dirs.** Today's
  `.brokkr/ratatoskr/sync/<test>/run-N/` keys on script name. If a
  script runs against multiple fixtures (sync-bench across a sweep)
  the path needs a fixture component. Defer until that shape lands.
