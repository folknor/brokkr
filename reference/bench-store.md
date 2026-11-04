# The bench store

How script-bench runs are stored, and the contract a writer, a reader and a
results server share. The code is `src/bench/store/`; brokkr's side of it
(where the file lives, turning a sidecar capture into evidence, credential
redaction) is `src/bench/evidence.rs`. Typed-command history is not here: it
stays in each project's `.brokkr/results.db` and `.brokkr/sidecar.db`.

## Where it lives

One SQLite file per user, `$XDG_DATA_HOME/brokkr/bench.db`
(`user_dirs::bench_db`), shared by every project: each run carries its
project, and its checkout as data, since clones of one project share its
history. `brokkr env` lists it and `brokkr check`'s free-space probe covers
it; `brokkr clean` never removes it, and nothing prunes it.

The file must be on a local filesystem: opening it refuses a file where WAL
does not take. SQLite on a network mount is not supported; a results server
owns its file locally and is reached over the network instead.

## The seam

`src/bench/store/` reaches only std, rusqlite, serde, serde_json and
xxhash-rust, never another brokkr module (the `bench-store-self-contained`
textlint rule). A second binary in this crate includes it by path, as the
rustc guard includes the lock probe, and gets the same document,
validation, migration chain and transaction; a results server is that
module with a network layer in front, never a second schema. Resolving the
path, creating its directory and redacting credentials stay with the caller,
so a document is already redacted when it leaves the process that made it.
The store's tests live outside the module (`src/bench/store_tests.rs`, and
`src/bench/wire_tests.rs` for the wire) for the same reason, except the
frozen-step check, which needs nothing else.

That second binary exists: `brokkr-results-server` (`src/bin/results_server/`,
`reference/results-server.md`) includes the module with a `#[path]`
attribute and serves its wire contract (below). The wire is in the module
too (`wire.rs`), so the server and a client decode the same types.

## The run document

The unit of every write and read is one `RunDocument`: one attempted run,
whole. Attempted means an identity and a persisted outcome whether or not a
result came of it: a build that failed, a run interrupted before its first
iteration and a run whose iterations disagreed are all stored. It holds:

- identity: a client-generated uuid (32 lowercase hex), project, bench,
  checkout, hostname, the full commit sha, whether the tree was dirty, the
  commit subject, the start time, brokkr's version and argv;
- conditions: kernel, CPU governor, available memory, the captured
  environment;
- the build: what was requested (target, profile, features with host
  features folded in, default features), always; what resolution produced
  (the bin and cargo's resolved features) and the artifact digest, each
  only as far as the build got;
- params by name, each with `many`, its resolved `from` path (`None` for a
  plain string) and its values in bound order, each a key and, for a file,
  a digest. An unbound param is absent; a bound empty collection keeps its
  name and path;
- the resolved config's text, stored once per distinct text;
- the iterations requested, the run's outcome (`completed`, `failed` with a
  reason, `interrupted`), and every iteration that ran, in execution order.

An iteration holds its outcome (`completed` with a verdict, where nil and
`""` differ; `failed` with a reason; `interrupted`), its block wall from the
call of `bench()` (absent when `bench()` was never called), its `record` as
extracted at the end of the sample (a JSON object; absent when extraction
never happened, which is not the same as an extracted empty table), and its
children in launch order.

A child holds its program, argv and environment additions, pid, its origin
offset from the start of its iteration's block, its wall, its ending
(exited, signal, stop marker, deadline as a whole `Duration` in seconds and
nanoseconds, interrupted, wait failed; evidence only, since the script
decides what an exit means), and
its evidence: the `/proc` samples, every FIFO record in one arrival order
(markers, counters, sealing emissions and terminal records, with their call
stream), the capture loss by cause, the clock check, and the call-id census.
Stamps stay on the child's own clock and are never rebased; an off-contract
stamp is kept as it came. Library call boundaries are not stored: only a
library call document supplies them, and a script bench parses none.

Every struct refuses unknown fields and the document carries `format`, so a
document a newer writer extended is refused rather than stored short of its
new fields. The outcome and ending enums are externally tagged for the same
reason: an internally tagged unit variant would drop any field beside its
tag.

Publication also refuses what would not read back as it came: a record
nesting deeper than 64 levels (the reader's JSON parser stops at 128), and
a capture count past SQLite's signed 64-bit range.

## Derived, not submitted

The store computes what follows from the document, so every writer agrees
and a replayed document compares equal to the stored one:

- **The result.** A run has one exactly when its outcome is `completed`, and
  then it is the first iteration with the smallest wall. Publication refuses
  a `completed` run unless it holds every requested iteration, each
  completed with a wall and an extracted record, all with one verdict, and
  a built artifact. A `failed` run may hold only completed iterations (a
  verdict disagreement, or a failure after the last one); a `failed` or
  `interrupted` run may hold none. Iterations of a run without a result
  are evidence only and never pair or compare.
- **Positions.** A sample's index is its position; a record's arrival index
  is its position across every kind; a marker's index is its position among
  markers.
- **The pairing key**, `bench-pair-v1`: project, bench, and each bound param
  by name with its `many` flag and its values' keys and digests in bound
  order. Collection order, a digest change and a param bound or not all
  split it; the commit, the config text, the build and the record do not.
  The key is a projection of the stored params: a later algorithm is a new
  projection computed from them, and two rows compare only under one
  algorithm, never by matching a `v1` string against another version's.
- **The config digest**: xxh3-128 of the text, as 32 hex digits. A digest
  already naming a different text is refused, not shared.

## Publication

`Store::publish` validates the document, then under one `BEGIN IMMEDIATE`
looks up its uuid, and either inserts every row or none. The uuid is the
request id: a uuid already stored is answered `AlreadyStored` when the
stored document equals the submitted one and refused as a conflict when it
does not; a stored run that cannot be read back is a store failure, never a
conflict. The answer to a replay is reconstructible from the run itself, so
no separate request record exists for this write kind. A write kind that
deletes runs would need one, since a deleted run can no longer answer its
publication's replay.

Errors say what a writer may conclude: `Refused` (the request, the store's
version or the file) and `Conflict` and `Failed` (anything before the
commit) are known unapplied; `CommitUncertain` (the commit itself errored)
may have landed, and republishing the same document settles it.

## Reading

`Store::open_read_only` opens an existing store and never creates its file:
a missing file is `None`, which a reader treats as no history. A store at the
current version opens read-only and `query_only`. One on an older version is
migrated first through a writable connection opened without
`SQLITE_OPEN_CREATE` (so a file removed in between is an error, not a fresh
store), then reopened read-only; a newer or negative stamp and a file with
tables but no stamp are refused, as by `Store::open`. The promise is about
the database file: SQLite may make the `-wal` and `-shm` files a WAL read
needs.

The listing queries live here, not in the reader, so a results server answers
the same queries from the same SQL. `RunFilter` always names a project, and
selects by uuid prefix, commit prefix (of the full sha), bench substring,
captured env, a bound param's value (its keys joined by `,` in bound order),
literal invocation terms (brokkr's argv, every child's argv and environment
additions, the captured env, each element matched as stored rather than as
JSON text), and whether the run has a result or a clean tree. Every listing
is in one total order, newest first by `started_unix_ms` then uuid, never by
insertion: publication may arrive late or be replayed from another client.
`Store::run` returns a run with what the store derived (config digest,
pairing key, result iteration), its children read only on request.

Two runs pair when their stored pairing keys are equal. The algorithm tag is
inside the key, so rows of two algorithms never match; a reader that needs
the tag reads it from the key.

## On the wire

`wire.rs` is the contract a results server and its clients share, with no
HTTP in it. One route, `POST /bench`, takes one request and answers it;
`GET /bench` answers the server's identity (`server`, `version`,
`protocol`) without touching the store. Every request carries the
`brokkr-bench-protocol` header naming `PROTOCOL_VERSION`, and every answer
the server writes carries it back.

**Requests and answers** are externally tagged, so a large document decodes
in place rather than buffered to find its tag: `publish` (a whole
`RunDocument`; its uuid is the request id, as in Publication above),
`select` (a `RunFilter`, answered with uuids in the total order), `run` (a
uuid and whether to read children, answered with a `StoredRun` or null),
`has_runs` and `elsewhere`, each answered by the variant of its name. A run
read without children (`children_loaded` false) is a summary: its
iterations' children are empty because they were not read, so it is never a
document to republish, and a client checks the flag is what it asked for.
`wire::execute` runs a decoded request on a `Store`; `wire::serve` decodes a
body, executes it and encodes the reply, which is all a server does with a
body once it holds the store.

**Strict, one protocol at a time.** Requests and answers refuse unknown
fields and require every non-optional one, in both directions; there are no
versioned adapters. Any change to how a request, an answer or a type inside
them (the run document included) encodes or decodes is a new
`PROTOCOL_VERSION`, and a request naming another version is refused as
`bad_request` naming both, so client and server are upgraded in step.
`SHIPPED_PROTOCOLS` holds one fingerprint per version, the xxh3-128 of the
canonical encoding of a frozen set of fixtures (every request and answer
kind, every failure kind); a shipped entry never changes, and a test fails
when the encoding no longer matches the last one. The fingerprint sees only
those fixtures' encoding, so a second test holds the accepted input language:
unknown fields at the top and nested, a missing required field, an
out-of-range integer and an unknown request kind are refused. The route, the
header and the request cap are pinned outside any version, since a client of
any version must find the server.

**The failure envelope** is the one tolerant shape, and its meaning is
stable across protocol versions, since a client meeting a server of another
version must still read the refusal: `{"error": {"kind", "message",
"applied", "retry"}}`. `retry` is required; `applied` is present, and
`false`, only when non-application of this attempt is proved; extra fields
and kinds a client does not know are tolerated, and a duplicated field makes
the envelope unreadable. The kinds:

| Kind | Applied | Retry | Meaning |
| --- | --- | --- | --- |
| `refused` | false | no | `StoreError::Refused`: an invalid document. |
| `conflict` | false | no | `StoreError::Conflict`: the uuid is stored with other content. |
| `failed` | false | yes | `StoreError::Failed`: the store failed before any commit. |
| `commit_uncertain` | absent | yes | `StoreError::CommitUncertain`: the commit errored. |
| `bad_request` | false | no | Malformed, an unknown kind, another protocol, no body length. |
| `too_large` | false | no | Over `MAX_REQUEST_BYTES` (256 MiB). |
| `busy` | false | yes | At capacity, or the store's worker gone; never run. |
| `read_timeout` | false | yes | The body did not arrive in time. |
| `wait_expired` | false or absent | yes | The server stopped waiting; false only when the request was withdrawn before it started. |
| `internal` | absent | yes | The server failed after the request started (a panic, an unencodable answer). |

A client reads the envelope through `Failure::classify`: `applied: false`
proves this attempt unapplied, `applied: true` is no value the protocol uses
and reads as uncertain, a known kind whose `applied` or `retry` contradicts
the table proves nothing and reads as uncertain, an unknown kind is read by
its `applied` and `retry` alone, and a `StoreError` is rebuilt only for a store kind
whose `applied` and `retry` are exactly that kind's (`conflict` with the
uuid the client published), so a contradiction never reaches a caller as a
store error whose class claims more than the envelope proves. A body without
a well-formed envelope (a proxy's page, hyper's own refusal of malformed
HTTP, a truncated answer) is classified by its status alone, whatever its
headers.

**What a client owes the contract.** `applied: false` speaks for that
attempt only. Once any attempt of a publication is uncertain (no answer,
`commit_uncertain`, an uncertain `wait_expired`, `internal`, a 5xx without
an envelope), it stays uncertain across later refusals and proved-unapplied
attempts, since the uncertain one may still commit; only a successful answer
(`stored` or `already_stored`) settles it, and republishing the same
document is how to get one. A 200 to a publication acknowledges it even when
its body cannot be read. The HTTP status is advisory (400, 404, 405, 408,
409, 411, 413, 500, 503, 504); the envelope decides.

## The migration chain

`schema.rs`'s `MIGRATIONS` owns the schema: step N takes `user_version` N to
N + 1, and every pending step applies under one `BEGIN IMMEDIATE`, the stamp
re-read inside it. A shipped step never changes, since every store and a
server replay the same chain; a schema change is a new step, and the
frozen-step test pins each shipped step's text by digest. Sample columns are
spelled out in the step that adds them, and a test holds them to the store's
sample fields and those to the sidecar's, so a new `/proc` field needs a new
step. Opening refuses a store stamped newer than the code, and a file with
tables but no stamp, which is not a bench store. Tables are `STRICT`, and
the checks that tie an outcome to its result, failure and verdict columns
are in the schema as well as in validation.
