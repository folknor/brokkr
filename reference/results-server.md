# The results server

How `brokkr-results-server` is built and why. How to run it is
`docs/results-server.md`; the wire it speaks is `reference/bench-store.md`,
On the wire. The code is `src/bin/results_server/`.

## Shape

A third bin target of this crate, beside `brokkr` and `brokkr-rustc-guard`,
and like the guard self-contained: it includes `src/bench/store/` by path
(`#[path = "../../bench/store/mod.rs"] mod store;`, which keeps the store's
own submodules beside its `mod.rs`) and links nothing else of brokkr. The
included copy carries parts only brokkr uses (the read-only open, the local
loader), so it takes `allow(dead_code, unused_imports)` on that module alone.
It is not a brokkr subcommand: a server is a long-running process with its
own lifecycle, run by whoever hosts one, and brokkr's global lock, project
detection and run logs have nothing to say about it.

Nothing deployment-specific is compiled in: the database file (`--db`) and
the listening address (`--listen`) are always given. There is no
application auth: who can reach the address is the network's business, and
the documented deployment listens on loopback behind a TLS proxy.

`main.rs` holds the arguments, `Limits` and startup; `owner.rs` the file's
ownership; `worker.rs` the single worker and the job state machine;
`http.rs` the HTTP layer; `tests.rs` the server against its contract over
real loopback HTTP.

## Owning the file

Before SQLite opens anything, the server resolves the file to one canonical
path: an existing file (a symlink included) fully, a missing one by its
canonical parent and its name, the directory created first. A dangling
symlink is refused, and so is a file with more than one hard link, since the
other name would take its own lock. It then takes an exclusive, non-blocking
`flock` on `<file>.lock` beside it (`std::fs::File::try_lock`), a file never
removed, so a second server on the same file fails at once rather than
waiting, and a replaced lock file cannot let one in. Only then does it call
`Store::open`, which requires WAL, migrates through the frozen chain and
refuses a store newer than itself, so a server migrates at startup and never
later. It never uses `Store::open_read_only`, whose migration is a reader's.

The lock coordinates servers only. A brokkr whose local store is the same
file is outside it: a newer brokkr would migrate the file under a running
older server. That the file belongs to its server alone is an operational
requirement the docs state, not something the server can enforce.

## The worker

One thread owns the `Store`, whose connection is opened
`SQLITE_OPEN_NO_MUTEX` and never shared. Every request, reads included,
becomes a job on one bounded queue (`Limits::queue`); the worker decodes the
body, runs it through `wire::serve` and encodes the answer, so the async
threads only move bytes and no unbounded synchronous work runs on them, and
every write is serial under `BEGIN IMMEDIATE`.

A job is `Queued`, then `Running` or `Withdrawn`, by compare-and-swap on both
sides. The worker claims a job before touching it, and withdraws instead a
job whose deadline passed before it got there. A handler that stops waiting,
whatever stopped it (its wait expiring, the connection's deadline, the
connection closing and its future dropped), withdraws the job through a drop
guard. Winning the withdrawal proves the job never ran (`wait_expired` with
`applied: false`); losing it to `Running` leaves the outcome unknown
(`wait_expired` without `applied`), and the job runs to its end. A worker
that drops a running job unanswered is `internal`, uncertain. A reply racing
the timeout may be reported uncertain needlessly, which is safe.

Each job runs under `catch_unwind`; a contained panic answers `internal`.
After every job, before its answer is delivered, the worker checks the
connection is back in autocommit (`Store::in_transaction`): a connection
left inside a transaction would poison every later request, so the worker
stops instead, and the answer is never sent. The worker's whole lifetime is
covered by a guard that signals the server when it ends, however it ends (a
panic outside a job, the queue closing, a poisoned connection, a failed
thread spawn), and the server then exits non-zero so its service manager
restarts it.

## Admission and bounds

`Limits::DEFAULT` holds every bound, with no flag to tune them:

- **Connections** (256): one past the cap is closed unanswered.
- **The head** (10 s, hyper's `header_read_timeout` with its tokio timer,
  64 KiB buffer). One request per connection (keep-alive off), so there is no
  idle connection to time out.
- **Routing and checks from the head alone**: the path, the method, the
  protocol header, a `Content-Length` (a chunked body without one is
  refused), the request cap (256 MiB).
- **Admission before the body**: an in-flight slot (64) and a reservation of
  the declared length against a byte budget (1 GiB), both taken before the
  body is polled. Refused, the request is answered `busy` without its body
  being read; hyper sends no `100 Continue` for a body never polled.
- **The body** (120 s), then the queue (`busy` when full).
- **The wait** (60 s) for the worker's answer.
- **The connection** (300 s, `wire::CONNECTION_DEADLINE`), head to last
  byte, which the three phases fit inside (a compile-time assertion), so a
  request that is not stuck has its answer written before the deadline cuts
  the connection. It lives in the wire module because brokkr's client
  reads it too: a client attempt outlasts it (`reference/bench-store.md`,
  brokkr's client).

The admission moves into the job and is released when the worker drops the
job, not when its handler gives up: a withdrawn job keeps its queue slot,
in-flight slot and reservation until the worker reaches and drops it, so one
long job can leave withdrawn bodies holding capacity until it ends.

What is not bounded, stated rather than hidden: the byte budget bounds the
request bytes admitted, not the memory decoding them allocates. Answers are
not paginated or capped (a `select` answers every uuid; `compare_sides` needs
every candidate before pairing), so a small `run` request can produce a
large answer; the connection cap and deadline bound how many answers are in
flight and for how long, not their size. The wait bounds a caller's wait,
not the work: a running job holds the worker until it finishes, and
`BUSY_TIMEOUT` (5 s) bounds SQLite's lock waits, not an insert or a read. A
document over the request cap is publishable to a local store and refused
remotely as `too_large`.

## HTTP

hyper 1's HTTP/1.1 server on the tokio runtime nextest-runner already
brings, rather than bz's hand-written subset: bz is synchronous throughout,
which ruled the async servers out there, while brokkr links tokio anyway.
HTTP/1.1 only, since the server sits behind a TLS proxy speaking HTTP/1.1 to
its backend.

Every answer the service writes carries the protocol header, a JSON body and,
for a failure, the envelope, with an advisory status (`Kind` maps to 400,
408, 409, 413, 500, 503 or 504; an unknown route is 404, a method other than
GET and POST 405, a missing length 411). hyper's own refusals of malformed
HTTP carry neither the header nor an envelope. An early answer (a refusal
from the head, `busy`) is delivered best effort: there is no drain, so a peer
still sending its body may see the connection reset instead, which a client
counts as possibly sent and retries, idempotent by the run uuid.

Failures are logged on stderr, one line each with the status and kind, for
the service manager's journal; there is no access log. On SIGTERM the process
ends without draining: SQLite rolls back an interrupted transaction, and a
publication whose commit landed but whose answer was lost is answered from
the stored run when the client republishes.

## Tests

`tests.rs` runs the server on a loopback port with a raw HTTP/1.1 client:
publication, replay and conflict, every read, the refusals made from the head
(route, protocol, length, cap, malformed body), the store's refusal and
`busy`; the job races, on the handler's own submission path against a queue
the test drains by hand, the handler on a paused clock so its wait expires
when the test blocks on it and no scheduling window decides a race (a handler dropped before the claim withdraws its
job, which never runs; a wait expiring before the claim is proved unapplied;
a wait expiring after the claim is uncertain while the job still lands), each
checking the admission is held until the worker drops the job; and ownership (a second server, a
symlinked alias, a hard link, a dangling link, a relative path). The wire
without HTTP is `src/bench/wire_tests.rs`, in the main crate.
