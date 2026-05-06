# Ratatoskr sync orchestration - planning note

Status: **planning, post-option-B reconciliation.** Brokkr-side
commands for driving sync workloads against the mock-server project.
Companion to `notes/ratatoskr-service-harness.md` (plan 1) and
`notes/ratatoskr-mock-server.md` (plan 2). Depends on the
not-yet-bootstrapped mock-server project (plan 2; lives in its own
repo, Norse-mythology name TBD). This rev assumes plan 1's option-B
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
workloads against IMAP and JMAP providers; correctness and
performance of those workloads are the target of this plan.

**JMAP** (RFC 8620 + RFC 8621) is a JSON-over-HTTP mail protocol;
ratatoskr drives it via reqwest and a local fork of `jmap-client` at
`/home/folk/Programs/jmap-client`. **IMAP** is the legacy stateful
mail protocol; ratatoskr's IMAP support lives in
`<ratatoskr>/crates/imap/`.

**Three-plan architecture this note lives inside.** The work needed
to test ratatoskr's sync code splits into three independent pieces:

| | Owns |
| --- | --- |
| Plan 1 (`notes/ratatoskr-service-harness.md`, split across brokkr + ratatoskr) | Deterministic subprocess test harness. Ratatoskr's `app` crate hosts the Lua VM + `ServiceClient` userdata + wait combinator + frame-log tap + artefact writers, exposed via `app --test-harness <script.lua>`. Brokkr provides build orchestration + artefact-dir lifecycle + low-level primitives (signal, pid_is_alive, sentinel watch, /proc snapshot). Brokkr does not depend on ratatoskr or embed dellingr. See `notes/ratatoskr-service-harness.md` for full design. |
| Plan 2 (`notes/ratatoskr-mock-server.md`, eventually a standalone repo) | A small mock JMAP/IMAP server: protocol, fixture model, deterministic responses. Independent of brokkr and ratatoskr. |
| Plan 3 (this note, in brokkr) | Commands that spawn plan 2's binary and ratatoskr's harness binary (`app --test-harness <sync_script.lua>`) together, drive a sync workload via the script, collect metrics, store results. The Lua script speaks JSON-RPC to the Service via plan 1's `ServiceClient` userdata; brokkr never touches the wire. |

The three plans are independent in source - plan 2 is a separate Rust
project with its own crate; plans 1 and 3 are subcommands inside
brokkr; ratatoskr depends on none of them. Brokkr depends on neither
ratatoskr nor plan 2 at the source level - it spawns binaries and
talks line-delimited JSON / HTTP only.

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
| Plan 2 (standalone repo) | Mock IMAP/JMAP server: protocol implementations, fixture model, failure-injection knobs. Independent of brokkr and ratatoskr. |
| Plan 3 (this note, in brokkr) | Commands that build + spawn plan 2's binary alongside plan 1's harness binary, thread the mock endpoint into the harness via env var, let the script drive sync, collect wall-clock + sidecar metrics, store results. |

Boundary rules:

- Mock-server project owns protocol code, fixture loading, port
  binding, failure-injection knobs.
- Ratatoskr owns sync code, correctness assertions, the test-only
  `RequestParams` surface used to drive sync workloads
  (`TestStartSync`, `TestQueryDbState`, etc., named at Phase 8 design
  time), and the `.lua` scripts that exercise them.
- Brokkr owns spawn/teardown of both children, port allocation,
  env-var threading, wall-clock timing of the harness binary's
  lifetime, artefact-dir aggregation, results storage. Backstop
  timing inside the script lives in plan 1's harness module, not in
  brokkr.

Plan 3 contains no protocol code, no fixture content, no sync
correctness logic, no JSON-RPC client. If something feels like one of
those, it belongs in plan 2, in ratatoskr, or inside the Lua script.

## Reuse from plan 1

Plan 1 splits across brokkr and ratatoskr's `app` crate; plan 3 reuses
both halves.

Brokkr-side primitives carried over verbatim (already in tree per
plan 1's "implementation status"):

- Process spawn + signal + `pid_is_alive` (now driving two children -
  the mock server and the harness binary).
- Sentinel-file watch (`wait_for_sentinel`) - the mock server writes a
  "listening on port X" sentinel before sync starts.
- `/proc` snapshot - applied to both children's PIDs at failure.
- Artefact-dir lifecycle (`ArtefactDir`) - the same allocator, just
  pointed at `.brokkr/ratatoskr/sync/<test>/run-N/` instead of the
  service-test root.

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

- **Port allocation.** Multiple TCP ports per run, threaded to the
  mock server and through to the harness binary's env. Today's
  `[host].port` field is a single int and insufficient. Small
  port-pool helper expected.
- **Multi-child orchestration.** Mock server first (with readiness
  sentinel), harness binary second (inheriting the mock's endpoint
  via env var). Tear down in reverse on success or failure. The
  workload-driver process is the harness binary itself; no third
  child.
- **Endpoint injection.** The harness binary (and through it, the
  Service it spawns) is told the mock endpoints via env var (likely a
  `RATATOSKR_TEST_*_ENDPOINT` family read under the existing
  `test-helpers` feature gate). Brokkr only sets the var; ratatoskr
  decides what reads it.
- **Bench timing via marker FIFO.** For `sync-bench`, the Lua script
  emits `SYNC_START` and `SYNC_END` markers via brokkr's existing
  `BROKKR_MARKER_FIFO` protocol (already used by the sidecar). The
  sidecar picks up wall-clock spans for those markers without needing
  to peek at JSON-RPC. The script also writes a small JSON summary
  (e.g. `summary.json`) into `BROKKR_HARNESS_ARTEFACT_DIR` for richer
  metrics (`provider_request_count`, `final_db_size_bytes`,
  `messages_synced`); brokkr reads it post-exit and joins it with
  the marker spans + sidecar samples in the results DB.

## Commands

Top-level commands tagged `[ratatoskr]`, project-gated to
`Project::Ratatoskr`. Flat namespace, matching pbfhogg/elivagar/nidhogg.

- `brokkr mock-serve [--imap] [--jmap] --fixture <NAME>` - spawn the
  mock server(s) standalone, print listening endpoints, run until
  ctrl-C. Manual-exploration tool, not a test command. Does not
  touch the harness binary. Tears down cleanly on signal.
- `brokkr sync-smoke <SCRIPT>` - build mock + harness binary, spawn
  mock first (waiting for readiness sentinel), spawn `app
  --test-harness <SCRIPT>` second with mock endpoints injected via
  env, wait for the harness binary to exit. PASS/FAIL based on the
  harness binary's exit code; assertions live inside the Lua script.
  No metrics storage. The sync analogue of plan 1's single-script
  run.
- `brokkr sync-bench <SCRIPT> [--bench N]` - same spawn shape as
  sync-smoke, measured. Wall-clock spans come from `SYNC_START` /
  `SYNC_END` markers the script emits via `BROKKR_MARKER_FIFO`;
  richer metrics come from a `summary.json` the script writes into
  the artefact dir. Best-of-N stored in `.brokkr/results.db` via the
  existing `BenchHarness`. Comparable via `brokkr results --compare`.
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
brokkr at the mock-server fixture file. `protocol` selects which
mock-server flavour (`--imap` / `--jmap`) brokkr spawns. Scripts
themselves use plan 1's `ServiceClient` userdata to drive sync and
assert; brokkr only sets endpoint env vars and watches exit codes.

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
mock_server_binary = "../<mock-project>/target/release/mock-mail"
fixtures_dir = "../<mock-project>/fixtures"
test_endpoint_env_imap = "RATATOSKR_TEST_IMAP_ENDPOINT"
test_endpoint_env_jmap = "RATATOSKR_TEST_JMAP_ENDPOINT"
sync_script_dir = "crates/app/tests/sync-harness"  # optional override

[ratatoskr.harness]
sweep = "harness"   # already declared by plan 1
binary = "app"      # already declared by plan 1
```

Fixture names referenced by sync-test script frontmatter resolve
relative to `fixtures_dir`. The mock-server binary path resolves
relative to `brokkr.toml`. Endpoint env-var names are configurable so
brokkr doesn't hardcode ratatoskr's test-only flag spelling.
`sync_script_dir` defaults to `crates/app/tests/sync-harness`; override
only if the layout changes.

Plan 3 reuses plan 1's `[ratatoskr.harness]` to know which harness
binary to spawn - no separate plan-3 harness binary entry. The mock
server is built on demand from `mock_server_binary`'s parent project
via `crate::build::cargo_build`, same model as `brokkr serve` for
nidhogg.

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
module). Plan 3 adds a parallel artefact dir for the mock server and
points the harness at a sub-directory underneath the run dir.

`.brokkr/ratatoskr/sync/<test>/run-N/`:

- `harness/` - the harness binary's artefact dir, written by
  ratatoskr's harness module per plan 1. Contains the standard plan-1
  payload: `frames.jsonl` (UI <-> Service JSON-RPC), `events.jsonl`,
  `steps.jsonl`, `service.stderr`, `proc-at-failure.txt`, `data-dir/`,
  `exit.txt`, `run.toml`. For sync-bench runs, also `summary.json` -
  the per-run metrics dump (`provider_request_count`,
  `messages_synced`, `final_db_size_bytes`, etc.) the script writes.
- `mock-server.stderr` - mock server logs.
- `proc-at-failure-mock.txt` - `/proc` snapshot of the mock at
  failure-declaration time, taken by brokkr's `snapshot_proc`.
- `exit-mock.txt` - mock-server exit code, signal, wait time.
- `run.toml` - plan-3 metadata: fixture name, protocol, brokkr
  version, mock-server version, ratatoskr commit, sync-script path.

Sync-protocol traffic between Service and mock is over TCP. Capturing
it is plan 2's concern (the mock can write request/response logs to
its stderr or to a configured file); plan 3 just preserves whatever
the mock writes.

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

1. `brokkr mock-serve --jmap --fixture small` starts the mock server,
   prints its endpoint, runs until ctrl-C. Tears down cleanly. No
   harness binary involved.
2. `brokkr sync-smoke crates/app/tests/sync-harness/jmap-initial.lua`
   builds mock + harness, spawns mock with the script's declared
   fixture, spawns `app --test-harness` with the mock endpoint
   injected, the script drives an initial sync via
   `client:request("TestStartSync", ...)` and asserts the resulting
   DB state via `client:request("TestQueryDbState", ...)`, exits 0.
3. `brokkr sync-bench <script> --bench 5` does the same five times,
   reads each run's `SYNC_START`/`SYNC_END` markers and `summary.json`,
   stores rows in `.brokkr/results.db`, prints best-of-5 summary.
4. `brokkr sync-list` discovers all `.lua` scripts under
   `[ratatoskr] sync_script_dir` and prints them with frontmatter.
5. IMAP path lands once plan 2 supports it and ratatoskr's IMAP sync
   is in scope (Phase 5+).

## Out of scope

- Mock-server protocol implementation, fixture authorship, failure
  injection knobs - plan 2.
- Ratatoskr's sync correctness assertions and test-helper RPC surface
  - ratatoskr.
- Threshold-based regression gating. Add once threshold data exists.
- Sanitized real-world protocol traces as fixtures - plan 2.
- Multi-account workloads. v1 is single-account; multi-account is a
  fixture and orchestration extension for later.

## Suggested implementation order

The order separates work that can land before plan 1's harness module
exists from work that requires it.

Independent of plan 1's harness module:

1. Plan 2 bootstraps with at least one fixture and a basic JMAP
   listener that writes a readiness sentinel.
2. `Project::Ratatoskr` enum variant + plan-3 `[ratatoskr]`
   brokkr.toml field parsing (plan 1 already landed the variant +
   `[ratatoskr.harness]`; plan 3 adds the `mock_server_binary` /
   `fixtures_dir` / `*_endpoint_env_*` / `sync_script_dir` fields).
3. Port-pool helper.
4. `brokkr mock-serve` - simplest case, one child, foreground. Uses
   plan 2's binary directly; no harness binary involved.

Requires plan 1's harness module (Phase 8 ratatoskr-side):

5. `brokkr sync-list` - walks `sync_script_dir`, parses frontmatter,
   prints sorted table. Can land before scripts exist (empty-state
   message); useful early.
6. `brokkr sync-smoke <SCRIPT>` - spawns mock + harness binary,
   threads endpoints, waits for exit. The first script + first
   `RequestParams::TestStartSync` / `TestQueryDbState` variants on the
   ratatoskr side land together as the wedge.
7. `brokkr sync-bench <SCRIPT>` - wraps sync-smoke in `BenchHarness`,
   adds marker-FIFO span collection and `summary.json` ingestion.
8. IMAP follows when plan 2 supports it and ratatoskr's IMAP sync is
   in scope.

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

Still open:

- **Fixture format / naming.** Plan 2's concern, but brokkr needs to
  know how fixtures are referenced from script frontmatter. Coordinate
  at integration time.
- **Mock-server CLI shape.** Plan 2 defines its argv; plan 3 defines
  how brokkr's per-fixture / port-pool flags compile down. Coordinate
  at integration time.
- **Build orchestration for plan 2's binary.** Same model as
  `brokkr serve` (nidhogg) - cargo build on demand from a known
  project root via `crate::build::cargo_build`. The
  `mock_server_binary` config field documents the expected artefact
  path. Implementation detail; resolves when plan 2 is in tree.
- **`RequestParams` surface for sync triggering / assertion.** Phase 8
  ratatoskr-side design. Names like `TestStartSync`, `TestQueryDbState`
  used in this note are placeholders. The harness Lua binding picks
  them up automatically once they exist (no harness module recompile,
  per plan 1).
- **Per-script vs per-fixture run dirs.** Today's
  `.brokkr/ratatoskr/sync/<test>/run-N/` keys on script name. If a
  script runs against multiple fixtures (sync-bench across a sweep)
  the path needs a fixture component. Defer until that shape lands.
- **Mock-server stderr capture vs streaming.** Mirror plan 1's
  service.stderr handling - capture verbatim per run into the
  artefact dir, no live stream.
