# The results server

This page covers how `brokkr-results-server` is built and why. How to run it
is `docs/results-server.md`; the wire it speaks is
`reference/bench-store.md`, On the wire. The code is
`src/bin/results_server/`. Shape and Owning the file cover the binary and
its database; The worker, Admission and bounds and HTTP cover serving;
Deploy covers the deploy subcommand.

## Shape

The server is a third bin target of this crate, beside `brokkr` and
`brokkr-rustc-guard`, and like the guard it's self-contained. It includes
`src/bench/store/` by path (`#[path = "../../bench/store/mod.rs"] mod
store;`, which keeps the store's own submodules beside its `mod.rs`) and
links nothing else of brokkr. The included copy has parts only brokkr uses
(the read-only open, the local loader), so it takes
`allow(dead_code, unused_imports)` on that module alone.

It isn't a brokkr subcommand. A server is a long-running process with its
own lifecycle, run by whoever hosts one, and brokkr's global lock, project
detection and run logs have nothing to say about it.

Nothing deployment-specific is compiled in. You always give `serve` the
database file (`--db`) and the listening address (`--listen`), and
`deploy` reads what a host declares. There's no application auth: who can
reach the address is the network's business, and the documented deployment
listens on loopback behind a TLS proxy.

The source files:

- **`main.rs`**: the two subcommands, `Limits` and startup
- **`owner.rs`**: the file's ownership
- **`worker.rs`**: the single worker and the job state machine
- **`http.rs`**: the HTTP layer
- **`identity.rs`**: the executable digest and the probe
- **`deploy.rs`**: the deploy
- **`tests.rs`**: the server against its contract over real loopback HTTP
- **`deploy_tests.rs`**: deploy against such a server

## Owning the file

Before SQLite opens anything, the server resolves the file to one canonical
path. An existing file, a symlink included, resolves fully; a missing one
resolves by its canonical parent and its name, the directory created first.
The server refuses a dangling symlink, and a file with more than one hard
link, since the other name would take its own lock.

It then takes an exclusive, non-blocking `flock` on `FILE.lock` beside it
(`std::fs::File::try_lock`), a file never removed. So a second server on
the same file fails at once rather than waiting, and a replaced lock file
can't let one in. Only then does the server call `Store::open`, which
requires WAL, migrates through the frozen chain and refuses a store newer
than itself, so a server migrates at startup and never later. It never uses
`Store::open_read_only`, whose migration is a reader's.

The lock coordinates servers only. A brokkr whose local store is the same
file is outside it: a newer brokkr would migrate the file under a running
older server. That the file belongs to its server alone is an operational
requirement the docs state, not something the server can enforce.

## The worker

One thread owns the `Store`, whose connection is opened
`SQLITE_OPEN_NO_MUTEX` and never shared. Every request, reads included,
becomes a job on one bounded queue (`Limits::queue`). The worker decodes
the body, runs it through `wire::serve` and encodes the answer. So the
async threads only move bytes, no unbounded synchronous work runs on them,
and every write is serial under `BEGIN IMMEDIATE`.

A job is `Queued`, then `Running` or `Withdrawn`, by compare-and-swap on
both sides. The worker claims a job before touching it, and instead
withdraws a job whose deadline passed before the worker got there. A
handler that stops waiting withdraws the job through a drop guard, whatever
stopped it: its wait expiring, the connection's deadline, or the connection
closing and its future dropped. The outcomes:

- Winning the withdrawal proves the job never ran: `wait_expired` with
  `applied: false`.
- Losing it to `Running` leaves the outcome unknown: `wait_expired` without
  `applied`, and the job runs to its end.
- A worker that drops a running job unanswered is `internal`, uncertain.

A reply racing the timeout may be reported uncertain needlessly, which is
safe.

Each job runs under `catch_unwind`, and a contained panic answers
`internal`. After every job, before its answer is delivered, the worker
checks the connection is back in autocommit (`Store::in_transaction`). A
connection left inside a transaction would poison every later request, so
the worker stops instead, and the answer is never sent.

A guard covers the worker's whole lifetime and signals the server when it
ends, however it ends: a panic outside a job, the queue closing, or a
poisoned connection. The server then exits non-zero so its service manager
restarts it. A thread that can't be spawned fails startup before anything
is served.

## Admission and bounds

`Limits::DEFAULT` holds every bound, with no flag to tune them:

- **Connections** (256): one past the cap is closed unanswered.
- **The head** (10s, hyper's `header_read_timeout` with its tokio timer,
  64 KiB buffer). The server takes one request per connection (keep-alive
  off), so there's no idle connection to time out.
- **Routing and checks from the head alone**: the path, the method, the
  protocol header, a `Content-Length` (a chunked body without one is
  refused), and the request cap (256 MiB).
- **Admission before the body**: an in-flight slot (64) and a reservation
  of the declared length against a byte budget (1 GiB), both taken before
  the body is polled. A refused request is answered `busy` without its body
  being read; hyper sends no `100 Continue` for a body never polled.
- **The body** (120s), then the queue (`busy` when full).
- **The wait** (60s) for the worker's answer.
- **The connection** (300s, `wire::CONNECTION_DEADLINE`), head to last
  byte. The three phases fit inside it (a compile-time assertion), so a
  request that isn't stuck has its answer written before the deadline cuts
  the connection. The bound lives in the wire module because brokkr's
  client reads it too: a client attempt outlasts it
  (`reference/bench-store.md`, brokkr's client).

The admission moves into the job, and the worker releases it when it drops
the job, not when the job's handler gives up. A withdrawn job keeps its
queue slot, in-flight slot and reservation until the worker reaches and
drops it, so one long job can leave withdrawn bodies holding capacity until
it ends.

Some things aren't bounded, and this page states them rather than hiding
them:

- The byte budget bounds the request bytes admitted, not the memory
  decoding them allocates.
- Answers aren't paginated or capped: a `select` answers every uuid, and
  `compare_sides` needs every candidate before pairing. So a small `run`
  request can produce a large answer. The connection cap and deadline bound
  how many answers are in flight and for how long, not their size.
- The wait bounds a caller's wait, not the work: a running job holds the
  worker until it finishes. `BUSY_TIMEOUT` (5s) bounds SQLite's lock
  waits, not an insert or a read.
- A document over the request cap is publishable to a local store and
  refused remotely as `too_large`.

## HTTP

The server uses hyper 1's HTTP/1.1 server on the tokio runtime
nextest-runner already brings, rather than bz's hand-written subset. bz is
synchronous throughout, which ruled the async servers out there, while
brokkr links tokio anyway. It speaks HTTP/1.1 only, since the server sits
behind a TLS proxy speaking HTTP/1.1 to its backend.

Every answer the service writes has the protocol header, a JSON body and,
for a failure, the envelope. Its status is advisory: `Kind` maps to 400,
408, 409, 413, 500, 503 or 504; an unknown route is 404, a method other
than GET and POST 405, and a missing length 411. hyper's own refusals of
malformed HTTP have neither the header nor an envelope.

An early answer (a refusal from the head, `busy`) is delivered best
effort. There's no drain, so a peer still sending its body may see the
connection reset instead. A client counts that as possibly sent and
retries, which is idempotent by the run uuid.

This layer answers `GET /bench`, never the worker. The answer has the
server, its version and protocol, `build`, `stamp` and an echoed `probe`
(Deploy, below). `stamp` is the `.brokkr.build_info` line, read once from
`/proc/self/exe` through the shared `src/elf_section.rs`, and omitted when
the executable has none. Every answer the service composes has
`Cache-Control: no-store`, since a replayed identity could report a build
no live process runs.

The server logs failures on stderr, one line each with the status and
kind, for the service manager's journal; there's no access log. On SIGTERM
the process ends without draining. SQLite rolls back an interrupted
transaction, and a publication whose commit succeeded but whose answer was
lost is answered from the stored run when the client republishes.

## Deploy

Deploy has to establish one thing: after an install, the process serving
the declared endpoint runs the executable deploy itself was started as. A
hook that installs and then runs deploy makes that the one just installed;
an install racing the deploy can separate the two. If the process doesn't
run it, deploy fails, so the hook that ran it keeps it owed. The proof
comes from that process, not from a service manager.

At startup, before taking the file, the server streams an xxh3-128 digest
of its opened `/proc/self/exe`. That names the file object it runs, even if
the path is replaced or unlinked while it migrates. Deploy digests its own
executable the same way, and accepts only an answer that:

- echoes the fresh random probe it sent
- comes from a server of its protocol
- reports the same digest
- arrives before its deadline

The identity route exists only once the accept loop runs: after ownership,
migration, the bind and a spawned worker (`worker::start` fails startup
when the thread can't spawn). A digest the server can't read is logged and
omitted, so the server still serves and deploy fails. xxh3 detects
accidental mismatches, not forgery, and the digest covers the executable's
bytes, not libraries or configuration.

There's no freshness check. A server already running this build has
nothing left to deploy, whatever the restart command did. A restart that
restarts nothing is caught at the next install that changes the
executable, when the old digest answers. Two freshness checks were
rejected on that ground:

- a baseline instance compared before and after: an unobtainable baseline
  proves nothing
- a start time on a shared clock: a time sampled by startup code isn't
  process birth, and time namespaces break the comparison

Other shapes were rejected too:

- **A hostname compiled in**: brokkr is public.
- **A gate on `systemctl is-enabled`**: its exit status counts static,
  alias, generated and transient units, and an unreachable manager looked
  like no server.
- **A unit deploy writes and converges**: drop-ins, unit lookup precedence
  and concurrent deploys stay independent inputs, and withdrawing loses the
  evidence that it was unfinished.
- **`sd_notify` readiness behind a fixed `TimeoutStartSec`**: a migration
  has no duration that separates slow from stuck, and the probe already
  gives readiness.
- **An `[[install]]` step**: it would restart services on every
  development install.
- **The server exiting when its binary is replaced**: a new build that
  fails to start would be invisible to the hook.

The declaration's path is fixed under `$HOME` without XDG lookup, because a
fleet hook's stripped environment and a login shell must find the same
file. An absent file is the one outcome that touches nothing. The restart
command is an argv you write, run without a shell from the home directory,
so whatever its launcher needs (`XDG_RUNTIME_DIR` for `systemctl --user`)
is written there rather than guessed here.

One deadline on the monotonic clock, fixed before anything else, covers:

- the self digest
- the command, which deploy doesn't start once the deadline has passed and
  tries to kill at it. One that can't be killed is left running, not
  waited for, and what it started may continue.
- every probe, each capped by what remains, with no proxy, no redirects, a
  bounded answer, and a probe drawn without blocking on the kernel's
  generator

The work runs on a thread of its own (`deploy::watched`), which the caller
waits for until the deadline plus a second's grace. So a spawn, a reap or a
TLS handshake that ignores its own timeout can't hold deploy past its
bound. The grace is for the work's own report, not for new operations.
Writing deploy's last line is outside the bound: a caller that never reads
a full pipe can still hold it.

Deploy remembers another build's answer and names it at the deadline,
rather than failing on sight, since an asynchronous restart may still
replace it. Deploy holds no lock and keeps no state: two overlapping
deploys each report what they observed, not which build serves at the end.

## Tests

`tests.rs` runs the server on a loopback port with a raw HTTP/1.1 client.
It covers:

- publication, replay and conflict, and every read
- the refusals made from the head: route, protocol, length, cap, and a
  malformed body
- the store's refusal, and `busy`
- ownership: a second server, a symlinked alias, a hard link, a dangling
  link, and a relative path
- the job races

The job races run on the handler's own submission path, against a queue the
test drains by hand. The handler runs on a paused clock, so its wait
expires when the test blocks on it and no scheduling window decides a
race. Each race checks the admission is held until the worker drops the
job:

- A handler dropped before the claim withdraws its job, which never runs.
- A wait expiring before the claim is proved unapplied.
- A wait expiring after the claim is uncertain, while the job still runs
  to completion.

`deploy_tests.rs` runs deploy's convergence against such a server:

- this build confirmed
- another build, an empty port, a failed status and an oversized answer,
  each failing at the deadline
- a failing and a hanging restart command
- the answers that don't count
- the declaration's refusals, a dangling link and an unrepresentable
  deadline among them

The wire without HTTP is tested in `src/bench/wire_tests.rs`, in the main
crate.
