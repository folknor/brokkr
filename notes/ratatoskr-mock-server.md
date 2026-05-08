# Sæhrimnir - mock email-protocol server (plan 2)

Status: **bootstrapped at `/home/folk/Programs/sæhrimnir/`; v0 scope
shipped across five protocols.** JMAP, IMAP read-path, SMTP submission,
Microsoft Graph mail-sync, and Gmail mail-sync are all complete for
v0. The TOML and Lua (dellingr-backed) fixture loaders are wired;
reactive `on(...)` callbacks plus `wait` / `mock_done` / `mock_fail`
script controls are implemented across all five protocols.

This note captures the brokkr-side view of the contract: how brokkr
spawns sæhrimnir, what files end up on disk, and what's still pending
on the brokkr side to complete the wiring (`Project::Saehrimnir`
enum variant, plan-3 commands). For sæhrimnir's internals, the
authoritative docs are inside the sæhrimnir repo - `README.md`,
`CLAUDE.md`, `TODO.md`, and `notes/` (especially `notes/orchestration.md`
for the brokkr/sæhrimnir contract from the other side, and the
per-protocol surface notes for the wire shapes).

Companion to `notes/ratatoskr-sync-orchestration.md` (plan 3). This is
plan 2. Plan 1 (the Service-test harness) is no longer in this notes
tree - the brokkr-side scaffolding shipped and the cross-cutting design
lives at `<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`.

## Background for reviewers

Throughout this document, `<sæhrimnir>` refers to
`/home/folk/Programs/sæhrimnir/` (sibling to brokkr's checkout) and
`<ratatoskr>` to the ratatoskr repository. All other paths are
relative to brokkr's repo unless otherwise stated.

**Ratatoskr** is a Rust desktop email client (an `iced` UI process
plus a child Service worker that owns all writes - sync, DB,
body/inline/blob stores, Tantivy indexing). Its sync code talks to
real JMAP, IMAP, SMTP, Microsoft Graph, and Gmail providers; testing
that code in isolation needs deterministic peers it can talk to
instead of the public internet. Sæhrimnir is those peers, in one
process, all driven by the same fixture.

**Brokkr** is a single-binary Rust dev tool that already orchestrates
several sibling projects (`pbfhogg`, `elivagar`, `nidhogg`, etc.) -
builds, runs, benchmarks, retains artefacts, stores results in
`.brokkr/results.db`. Sæhrimnir is one more binary brokkr will spawn
as a child process during ratatoskr's sync tests; brokkr's
`[ratatoskr]` commands (in plan 3) start it, point ratatoskr's
Service at it via env vars, drive a workload, and tear it down.

**JMAP** (RFC 8620 core + RFC 8621 mail) is a JSON-over-HTTP mail
protocol. **IMAP** (RFC 3501 + extensions) is the legacy stateful
mail protocol. **SMTP** (RFC 5321 + submission RFCs) handles
outbound. **Microsoft Graph** (`graph.microsoft.com/v1.0/me/...`)
is Microsoft's REST API used for Outlook/Exchange. **Gmail** is
Google's REST API at `gmail.googleapis.com/gmail/v1/users/me/...`.
Ratatoskr's sync code talks to all five.

**Three-plan architecture this note lives inside.** The work needed
to test ratatoskr's sync code splits into three independent pieces:

| | Owns |
| --- | --- |
| Plan 1 (split: ratatoskr `app` crate + brokkr) | Deterministic Service-test harness. Ratatoskr's `app` crate hosts the Lua VM (`dellingr`), `ServiceClient` userdata bindings, the wait combinator, the frame-log tap, and the artefact-dir writers - exposed via `app --test-harness <script.lua>`. Brokkr provides build orchestration, the artefact-dir lifecycle, and low-level primitives (signal, pid_is_alive, sentinel watch, /proc snapshot). Brokkr does not depend on ratatoskr or embed dellingr. Cross-cutting design at `<ratatoskr>/docs/harness/{problem-statement,architecture,roadmap}.md`. |
| Plan 2 (this note; sæhrimnir) | The mock email-protocol server: protocol implementations, fixture model, deterministic responses, scenario-control surface. Independent of brokkr and ratatoskr at the source level. |
| Plan 3 (in brokkr) | Commands that spawn sæhrimnir and ratatoskr's harness binary together, drive a workload, collect metrics, store results. Ratatoskr's headless-sync surface (`TestStartSync` / `TestQueryDbState` `RequestParams` variants on the Service, driven from a Lua script via plan 1's `--test-harness`) is a plan-3 implementation detail; from sæhrimnir's perspective it just sees protocol traffic on its bound ports. |

Sæhrimnir talks JMAP/HTTP, IMAP/TCP, SMTP/TCP, Graph/HTTP,
Gmail/HTTP outward and reads a fixture file (TOML or Lua) inward. It
does not depend on brokkr or ratatoskr at the source level; brokkr
spawns it as a generic subprocess and points ratatoskr's Service at
the resulting endpoints via per-protocol env vars.

## What sæhrimnir is

A single-binary Rust process (`saehrimnir` - ASCII transliteration so
Cargo, filesystems, and shells stay sane; the repo dir uses the
proper Norse spelling) that:

- Loads exactly one fixture (TOML or Lua) at startup. Validates
  cross-references and refuses to start on inconsistency.
- Binds one TCP port per protocol (JMAP HTTP, IMAP, SMTP, Graph
  HTTP, Gmail HTTP). Each port can be specified or left at `0` for
  ephemeral.
- Atomically writes a readiness sentinel once every listener is
  bound, one line per protocol: `<NAME> <port>\n`
  (`<NAME>` ∈ `JMAP`, `IMAP`, `SMTP`, `GRAPH`, `GMAIL`). Atomic
  via write-temp-then-rename on the same filesystem.
- Serves the protocols deterministically: same fixture in,
  byte-stable bytes out across runs. Output JSON uses
  `serde_json::Map` (`BTreeMap`-backed) for stable key ordering.
- Exits cleanly on SIGTERM within a 1-second graceful budget.

State tokens are pinned for the lifetime of a fixture
(`fixture-state` for JMAP, `1` for IMAP `HIGHESTMODSEQ` and Gmail
`historyId`, `s.0` / `d.1` for Graph cursors).

## Protocol surface (v0)

| Protocol | Wire | What's served |
| --- | --- | --- |
| JMAP | HTTP | `/.well-known/jmap`, `/jmap/session` (advertises core + mail; deliberately omits `principals` to avoid pulling the client into `Principal/get`), `POST /jmap/api` for `Mailbox/get`, `Email/query`, `Email/get`. Out-of-scope methods return `unknownMethod`. |
| IMAP | TCP | Greeting, `CAPABILITY`, `LOGIN` / `AUTHENTICATE`, `ENABLE QRESYNC`, `LIST`, `STATUS`, `SELECT`/`EXAMINE`/`CLOSE`, `UID SEARCH`, `UID FETCH` with full RFC 822 body emission, CONDSTORE `CHANGEDSINCE`. `UID STORE` is a non-persistent no-op (post-op FETCH untagged + tagged OK; mutation does not persist). Out-of-scope commands return `BAD`. |
| SMTP | TCP | Submission only. Greeting, `EHLO`, `AUTH PLAIN`/`LOGIN`/`XOAUTH2`/`OAUTHBEARER`, `MAIL FROM`, `RCPT TO`, `DATA` with dot-stuffing reversal, `RSET`, `NOOP`, `QUIT`. Submissions captured in an in-memory `SubmissionLog` that tests can read. |
| Microsoft Graph | HTTP | `/v1.0/me/mailFolders/...` (list, by-id, by-well-known-alias, child folders), `/v1.0/me/mailFolders/{id}/messages` (with `$top`/`$skip`/`$skiptoken`/`$filter`), `/v1.0/me/mailFolders/{id}/messages/delta` (initial dump, follow-up no-op, `$deltatoken=latest` shortcut). Catchall returns the Graph error envelope so unimplemented resources are visibly out-of-scope. |
| Gmail | HTTP | `/gmail/v1/users/me/profile` + `/labels` + `/threads` (list paginated by `nextPageToken`, with `q=after:YYYY/M/D` filtering) + `/threads/{id}` (full MIME payload projection) + `/history` (no-op since fixtures don't change) + `/messages/{id}/attachments/{aid}` (404 stub) + `/settings/sendAs` (empty list). Catchall returns the Gmail error envelope. |

All five protocols project from the same canonical types in
`<sæhrimnir>/src/fixture.rs`; no fixture-format changes when a new
protocol layer landed.

## Authentication

v0: open. Every protocol accepts any credential without validation.
Bearer, basic, `LOGIN`, `XOAUTH2`, `OAUTHBEARER` all return success.
Real auth (OAuth challenge flows, credential rotation) is a v1+
concern and orthogonal to "does sync correctness work."

## Fixture model

One fixture per process. Loader dispatches by extension:

- `.toml`: declarative config. `<sæhrimnir>/fixtures/jmap-small.toml`
  is the canonical sample.
- `.lua`: dellingr (Lua VM) script that builds the same `Fixture`
  shape via `fixture({...})` / `account({...})` / `mailbox({...})` /
  `email({...})` builders, validated identically by the TOML loader's
  `normalize` cross-reference pass. `bulk_emails({ count, mailbox,
  seed, ... })` and `bulk_threads({ count, messages_per_thread, ... })`
  generate synthetic data deterministically from a seed (templates
  lifted from ratatoskr's `dev-seed` crate).

Both produce a byte-identical `Fixture`; sæhrimnir's
`tests/lua_fixture.rs` enforces it.

`<sæhrimnir>/notes/fixture-format.md` is the authoritative reference
for the shape (shared by both loaders).

### Reactive callbacks

Lua scripts can register dynamic responses via
`on(protocol, command, fn)`. The protocol layer consults the
script's dispatcher before generating its default response; the
callback receives a `req` table with `call_index` (1-based per
`(protocol, command)`) plus protocol-specific fields, and can
return `{ status = "...", message = "..." }` to override. Returning
`nil` (or no return) passes through to the default response.

Per-protocol override semantics:

- **IMAP** (`UID FETCH`): tagged `<tag> <status> <message>`, no
  untagged FETCH emitted. Status is typically `NO`/`BAD`/`OK`.
- **JMAP** (any method): method-level error envelope inside
  `methodResponses`.
- **Microsoft Graph** (mail handlers): HTTP 400 with
  `{"error": {"code": status, "message": message}}`.
- **Gmail** (mail handlers): HTTP 400 with the Gmail error envelope.
- **SMTP** (`MAIL`/`RCPT`/`DATA`): wire response
  `<code> <message>\r\n` where `code` is parsed from `status` as
  `u16` (e.g. `"452"` for rate-limited rejection). The DATA body
  is not consumed when the override fires before `354`.

Three control helpers are also script-callable:

- `wait(ms)` - block the current dispatch for `ms` milliseconds.
  Useful for latency injection. Other connections queue briefly on
  the dispatcher mutex but unrelated protocol handling continues.
- `mock_done()` - signal the runtime to shut listeners down cleanly
  (exit 0). First call wins.
- `mock_fail("reason")` - signal a fault exit. The reason is printed
  to stderr; the process returns a non-zero exit code. Lets brokkr
  observe scenario success/failure via exit code instead of
  polling.

## Cross-process contract (brokkr's view)

Argv brokkr passes:

```
saehrimnir \
    --readiness-file <PATH> \
    --fixture <PATH> \
    [--jmap-port N] [--imap-port N] [--smtp-port N] \
    [--graph-port N] [--gmail-port N] \
    [--log-file PATH]
```

All five port flags default to `0` (ephemeral). The chosen port
lands in the readiness file. Brokkr does not need to specify ports
individually for v0 - letting all five be ephemeral and reading them
out of the sentinel is the simpler path.

Sentinel content (one line per protocol, every line always present
because every listener is always bound):

```
JMAP <port>
IMAP <port>
SMTP <port>
GRAPH <port>
GMAIL <port>
```

Brokkr's `wait_for_sentinel` uses presence-only semantics; a
plan-3-side helper reads the file and parses out the port for
whichever protocol the test cares about.

Signals:

- **SIGTERM**: clean shutdown within 1s. Submission paths drain if
  they fit in the budget; otherwise the listeners drop and the
  process exits.
- **SIGKILL** (backstop): no cleanup; brokkr preserves whatever
  artefacts already exist.
- **One spawn per run.** Brokkr does not restart sæhrimnir.

Stderr is sæhrimnir's primary log channel; brokkr captures it
verbatim. `--log-file` redirects only when the caller explicitly
asks for file form.

## Determinism

Every response derives from the fixture. No clocks, no random IDs,
no unsorted iteration. Sæhrimnir's bulk generators
(`bulk_emails` / `bulk_threads`) seed a `SmallRng`, so the same seed
yields the same data set regardless of when or where it runs.

Ratatoskr's account config will need a way to point at no-auth
endpoints. Plan 3 wires this via per-protocol env vars
(`RATATOSKR_TEST_{JMAP,IMAP,SMTP,GRAPH,GMAIL}_ENDPOINT`) read under
ratatoskr's existing `test-helpers` feature gate; the specific
env-var names are configurable from ratatoskr's `brokkr.toml`.

## Repository

`/home/folk/Programs/sæhrimnir/`. Standalone Rust project. Single
crate, single binary. Brokkr depends on neither sæhrimnir source nor
sæhrimnir crates - the contract is process-level only (argv, env,
sentinel, signals, stderr).

Sæhrimnir's own `brokkr.toml` is pending - see "Brokkr-side
integration: status" below.

## Brokkr-side integration: status

**Pending on the brokkr side:**

- `Project::Saehrimnir` enum variant in `src/project.rs` so
  sæhrimnir's own `brokkr.toml` can declare `project = "saehrimnir"`
  and pass brokkr's parse-time validation. Today sæhrimnir runs
  `brokkr check` in the no-toml fallback, which is fine for in-repo
  development but blocks the `brokkr.toml`-based workflow.
  (Sæhrimnir's `TODO.md` has this as a housekeeping item.)
- Plan-3 commands (`brokkr mock-serve`, `brokkr sync-smoke`,
  `brokkr sync-bench`, `brokkr sync-list`). Tracked in
  `notes/ratatoskr-sync-orchestration.md`. `mock-serve` is
  independent of plan 1's harness module landing; the rest are
  blocked on it.

**In tree on the sæhrimnir side, ready for brokkr to consume:**

- All five protocol implementations.
- TOML + Lua fixture loaders, byte-identical output asserted.
- Reactive callbacks across all five protocols, with the
  per-protocol override semantics above.
- Bulk generators (`bulk_emails`, `bulk_threads`) demonstrated by
  `<sæhrimnir>/fixtures/jmap-bulk.lua`.
- Atomic readiness-sentinel writes.
- 1-second SIGTERM graceful shutdown.
- Lifecycle integration test (`tests/lifecycle.rs`) - spawns the
  real binary, polls for the sentinel, hits a real network
  endpoint, sends SIGTERM, asserts a clean exit. Closes the
  coverage gap that `<sæhrimnir>/scripts/smoke.sh` covers manually.

## Open questions

Resolved (kept here for review-history clarity):

- ~~**Norse-mythology name.**~~ Resolved: sæhrimnir (the boar
  slaughtered every evening and resurrected every morning - fitting
  for a fixture-driven test peer that comes up identical on every
  spawn).
- ~~**HTTP framework.**~~ Resolved: axum.
- ~~**JMAP type surface.**~~ Resolved: hand-rolled, scoped to what
  ratatoskr's client exercises. No `jmap-proto`-style crate.
- ~~**Account config injection into ratatoskr.**~~ Resolved: brokkr
  sets `RATATOSKR_TEST_{JMAP,IMAP,SMTP,GRAPH,GMAIL}_ENDPOINT` env
  vars; ratatoskr's account-config code reads them under the
  existing `test-helpers` feature. Specific env-var names
  configurable via `[ratatoskr]` `test_endpoint_env_<proto>` fields
  in ratatoskr's `brokkr.toml`.
- ~~**Tracing / log format.**~~ Resolved: human stderr by default,
  optional `--log-file` for file capture.
- ~~**Where do fixtures live?**~~ Resolved:
  `<sæhrimnir>/fixtures/`. Plan 3's `[ratatoskr] fixtures_dir`
  points at it.
- ~~**Multiple accounts in one fixture.**~~ Deferred, not resolved.
  v0 enforces `is_personal = true` and exactly one account; the
  fixture format already accommodates `[[account]]` repeated, but
  the protocol projection layers don't surface multiple accounts
  yet. Tracked in sæhrimnir's `TODO.md`.

Still open:

- **Fixture body sources.** Inline `body_text` works today. `.eml`
  paths via `body_path`, multipart MIME, attachments, `body_html`
  are on sæhrimnir's TODO list (fixture-format growth). Both
  authoring formats (TOML and Lua) extend identically.
- **Incremental sync change scripts.** `[[change]]` entries (or a
  Lua equivalent) that advance state tokens between phases - JMAP
  state, IMAP UIDVALIDITY/HIGHESTMODSEQ bumps, Graph deltatokens,
  Gmail historyId. Out of scope until every happy-path lands.

## Out of scope (v0)

- JMAP write methods (`Email/set`, `Mailbox/set`, etc.).
- JMAP push (EventSource, WebSocket).
- JMAP submission (`EmailSubmission/set`).
- IMAP write paths beyond the no-op `UID STORE`, IDLE, NOTIFY.
- Calendar / contacts / drive / groups across Graph and Gmail
  (fixture-format growth listed in sæhrimnir's TODO.md).
- People-API contacts on Gmail's side, OneDrive resumable uploads
  on Graph's side.
- Real auth flows (OAuth challenge, credential rotation).
- Failure injection beyond the override callbacks (slow responses,
  disconnects, retryable errors at protocol-state level rather than
  callback-driven).
- Sanitized real-world protocol traces as fixtures.
- Fuzz testing of the JMAP / IMAP / Graph / Gmail wire formats.

## Non-goals

- Reusable as a public Rust crate. Sæhrimnir is a private test peer
  for ratatoskr's sync code. If anyone else wants a mock email
  server later, that is a different project.
- RFC compliance beyond what ratatoskr's clients exercise. The
  specs are huge; sæhrimnir implements the subset ratatoskr's
  clients actually use, citation-tracked in the per-protocol
  surface notes inside `<sæhrimnir>/notes/`.
- Drop-in replacement for Fastmail / Stalwart / Exchange / Gmail.
  Sæhrimnir is deterministic and small; they are general.
