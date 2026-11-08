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
history. `brokkr env` lists it (and names a configured results server) and
`brokkr check`'s free-space probe covers it; `brokkr clean` never removes it, and nothing prunes it.

The file must be on a local filesystem: opening it refuses a file where WAL
does not take. SQLite on a network mount is not supported; a results server
owns its file locally and is reached over the network instead.

When the user-wide config names a server (`results_server`,
`docs/brokkr.toml.md`), brokkr uses it in place of this file for every
project: `brokkr bench` publishes there and `brokkr results` reads there,
and the local file is neither read nor written. The two are one store
behind two backends, never merged: runs already in the local file stay
there.

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
- what was asked to be measured: `timing`, or `hotpath` for `brokkr bench
  --hotpath`. A run stored before the field existed reads as `timing`;
- params by name, each with `many`, its resolved `from` path (`None` for a
  plain string; otherwise the path into the run's resolved view,
  `docs/brokkr.toml.bench.md`, with `{param}` references substituted, its
  segments joined by dots, a segment that is not a bare TOML key quoted)
  and its values in bound order, each a key and, for a file,
  a digest. An unbound param is absent; a bound empty collection keeps its
  name and path;
- the configuration text the run was made from, stored once per distinct
  text (the runner writes one TOML document holding the run's roots, the
  script's exact source and origin, and the composed brokkr.toml, keys
  sorted; `brokkr man bench`, What a run stores);
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

A child of a call marked `hotpath` also holds its **capture**: the stage
name, its occurrence (the stage's marked calls so far in the iteration,
from 1, whatever became of each, so within an iteration a stage's
occurrences run 1..n in launch order) and the outcome: `captured` with the
report, a JSON object as the child wrote it and the reader's parser takes
it (nested at most 64 deep, as a record), `missing`, `invalid` or
`unreadable` each with its reason, or `over_budget`. An unmarked child has
none. A `completed` run measured `hotpath` holds a capture in every
iteration, and every capture it holds is `captured`; any other run may hold
any outcome.

A child of a `program` call, which ran an external program in place of the
built snapshot, also holds its **executable**: the absolute path brokkr
resolved and launched, and either that file's xxh3-128 digest as read at
launch or why it could not be read. It is evidence of what was there, not a
pin: a file replaced between the digest and the exec is not seen. Absent
means no separate observation: the snapshot child, whose identity is the
build's artifact, and every child stored before the field existed. The
field is omitted when absent, so a document without one encodes as it did.
It is not part of the pairing key; `brokkr results --compare` names the
executables whose observed identities differ.

Every struct refuses unknown fields, so a document a newer writer extended
is refused rather than stored short of its new fields: that refusal, not
`format`, is what keeps an older store from taking a newer document. The
document also carries `format`, which moves only when what a document
already says would read differently; an optional field added and omitted
when absent (a child's `executable`) leaves it alone, since every document
without it means what it always did. The outcome and ending enums are externally tagged for the same
reason: an internally tagged unit variant would drop any field beside its
tag.

Publication also refuses what would not read back as it came: a record or
a captured report nesting deeper than 64 levels (the reader's JSON parser
stops at 128), and a capture count past SQLite's signed 64-bit range.

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
- **The pairing key**: project, bench, and each bound param by name with
  its `many` flag and its values' keys and digests in bound order, tagged by
  what the run measured. A run measured `timing` none of whose children, in
  any iteration, captured a report is keyed `bench-pair-v1`, exactly as
  every run was before captures existed, so that history keeps pairing. A
  run measured `hotpath` is keyed `bench-pair-v2` with the class `hotpath`,
  and a `timing` run that captured anyway, its build instrumented by other
  means, `bench-pair-v2` with `incidental`: the three never pair with each
  other. Collection order, a digest change, a param bound or not and the
  class all split it; the commit, the config text, the build and the record
  do not. The key is a projection of the stored params and children: a
  later algorithm is a new projection computed from them, and two rows
  compare only under one algorithm, never by matching one tag's string
  against another's.
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

## Marks

What a user says about stored runs afterwards lives beside them, never in
them and never in `brokkr.toml`: a stored run is not edited or deleted, and
a bench retired here stays hidden after its table is gone from the config.
The code is `marks.rs`.

**The log.** `Store::mark` appends one `MarkRequest` (a client uuid, the
project, when and on which host it was made, and the mark) to the `marks`
table, the exact request as submitted. The marks: `retire` a bench name and
`unretire` it, `invalidate` a run (by its full uuid) and `revalidate` it,
`rename` a bench name. `retire` and `invalidate` require a non-blank
reason; the undos take none. The mark uuid is the request id, as a run's is
its publication's, under one `BEGIN IMMEDIATE`: a uuid already stored is
answered `AlreadyApplied` when the stored request is equal and refused as a
conflict when not (an invalid one included), and that lookup comes before
any check but the uuid's own shape, so a request a later mark undid still answers its replay
rather than being refused or applied again. Errors read as in Publication.
`Store::marks` gives a project's log back in apply order.

**The fold.** What the marks mean is one fold over a project's log in row
order (`MarkView`: the renames in effect, the retired end names, the
invalidated runs, each retirement and invalidation with the request it came
from). Admission checks a proposed mark against that fold and the runs
stored at that moment, and refuses as `Refused`; the fold applies every
accepted mark unconditionally and never consults runs, so a mark does on
every later read what it did when admitted.

- **Renames** form chains resolved to their end (`MarkView::name`). A name
  is the source of at most one rename, and only a name the store knows (it
  has runs, or a mark in effect names it) can be renamed. Renaming onto a
  known name is refused, so histories never merge silently, except onto an
  ancestor in the source's own chain: that is the undo, and it cancels
  every rename on the path, so the view never holds a cycle. The no-merge
  check sees the runs stored when the rename commits; a run of the new name
  published afterwards (one already in flight included) joins the renamed
  history, since a publication carries no identity but its name.
- **Retirement** belongs to an end name, and a rename never changes any
  run's visibility: a forward rename moves the retirement to the new end,
  and an undo gives every name on the cancelled path a copy of it, each
  then unretired on its own. Retiring or unretiring a renamed name acts on
  its end. A run published later under a retired name is retired too.
- A run is **hidden** when it is invalidated or its resolved name is
  retired. Admission refuses a retirement of an unknown or retired name, an
  unretire of one not retired, an invalidation of a run not in the project
  or already invalidated, and a revalidation of one that is not.

**Pairing across a rename.** The stored pairing key keeps the bench's
literal name and its contract (Derived, not submitted).
`MarkView::comparison_key` is a separate, view-dependent projection: the
stored key with its bench element replaced by the resolved name, its tag
and class kept, so two algorithms' keys still never meet; a key of a shape
it does not know comes back unchanged. It is computed from the stored key,
never from a summary's document, whose children are not there to give its
class. `brokkr results --compare` groups a side's runs by resolved name and
comparison key under its selection's one view; every view shows the
resolved name and discloses a differing stored one, and anything read out of
a run's stored config (its annotations) is read under the stored name.

**Revisions.** Runs are inserted whole and never deleted and marks only
appended, so a `Revision` (the highest mark row and the highest run row a
read sees, two bounds since neither orders the other's rows) fixes one
history. A selection is made at one, in one read snapshot with the marks it
folds, and answers it; a second selection pinned to it sees the same runs
and marks whatever was written in between. A revision past the store's
current one is refused, since it would admit rows written later.

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
JSON text), and whether the run has a result or a clean tree. The bench
substring matches the run's resolved name (Marks), and hidden runs are left
out unless `hidden` is set; a uuid lookup sets it, so a hidden run is still
found by its uuid. `Store::select` answers a `Selection`: the uuids, the
revision it was made at and the `MarkView` in effect there, which a reader
resolves every run it shows against. `has_runs` and `elsewhere` count every
run, hidden or not. Every listing
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
`protocol`, and from `brokkr-results-server` its executable digest `build`
and an echoed `probe`) without touching the store. Every request carries the
`brokkr-bench-protocol` header naming `PROTOCOL_VERSION`, and every answer
the server writes carries it back.

**Requests and answers** are externally tagged, so a large document decodes
in place rather than buffered to find its tag: `publish` (a whole
`RunDocument`; its uuid is the request id, as in Publication above),
`select` (a `RunFilter` and an optional revision to pin it to, answered
`selected` with a `Selection`), `run` (a
uuid and whether to read children, answered with a `StoredRun` or null),
`has_runs` and `elsewhere`, each answered by the variant of its name, `mark`
(a `MarkRequest`; its uuid is the request id, as in Marks above, answered
`marked`) and `marks` (a project, answered with its log). A run
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
409, 411, 413, 500, 503, 504); the envelope decides. A run answer's
`children_loaded` is checked against what was asked, and every request
carries the protocol header.

**brokkr's client** (`src/bench/remote.rs`) keeps those duties, with these
rules of its own:

- **By status alone**, for an answer without a well-formed envelope: a 4xx
  is proved unapplied and final, 408 and 429 unapplied and retryable,
  since a 4xx without an envelope was refused before a brokkr server ran
  the request (by a proxy, or by the HTTP layer on malformed HTTP), which
  assumes an intermediary uses its statuses faithfully; anything else, a
  proxy's bare 502 or 503 included, is uncertain and retryable.
- **A transport error** is proved unapplied only when the connection never
  existed: the name did not resolve, connecting timed out, or it was
  refused. Each attempt makes one fresh connection (no redirects, no idle
  pool, no retry inside the HTTP client), and on Linux a TCP socket reports
  a refusal only before it connected. Every other transport error is
  uncertain.
- **An attempt is never cut short of the server's deadline**: it may take
  `wire::CONNECTION_DEADLINE` (the server's connection deadline, 300 s)
  plus 30 s for a proxy, so a server that is not stuck always answers or
  closes first.
- **Retries** (`src/bench/publication.rs`) start only inside a window from
  the first attempt and under a cap: 120 s and 8 attempts for a
  publication, 20 s and 6 for a read or the probe, with a backoff from
  250 ms. A publication whose uncertainty is latched gets one attempt past
  either, so an attempt that timed out late still has a republication to
  settle it. A final failure ends the retries; a pending stop ends them
  after the first attempt, during a backoff too, but never cuts the
  attempt in flight. No pause is taken when no attempt would follow it, and
  admission is checked again after the pause. These bound the network
  waiting and backoff, not the command: a publication's worst case is its
  window plus two whole attempts plus the pause before the last (at most
  8 s); a read's, its window plus one whole attempt plus a pause (at most
  4 s).
- **Before a run is measured**, `brokkr bench` probes the server
  (`GET /bench`, its protocol checked), as it opens the local store first:
  a server that cannot be reached, or speaks another protocol, fails the
  command before anything is built. The probe proves reachability, not that
  the publication will be stored.
- **Reads** are retried without a latch (they are idempotent), and a 200
  whose body does not decode as the expected answer is final. A listing
  costs one `run` request per run, since the protocol has no batch read.

The same latch serves the local store: a `StoreError` is classified by the
kind it would travel as (`Kind::of`), so a local `Failed` is retried and a
final error after an uncertain attempt ends the publication uncertain.

**Marks** (`src/bench/marking.rs`) are written under the same latch and the
publication policy, as one request minted once per command: the same uuid,
time, host and encoded bytes on every attempt, a conflict read under the
mark's uuid, and only a success settling it: locally `Applied` or
`AlreadyApplied`, remotely any HTTP 200, whatever its body. A
command run again is a new request with a new uuid, never a replay, so an
uncertain end is answered by reading the current state, which proves nothing
about whether the uncertain attempt may still commit. A local mark needs an
existing store (`Store::open_existing`, which never creates the file): with
none, nothing has been published to mark and the command fails.

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
