# Process hygiene

This page covers what bounds, protects and reports every locked brokkr
command. "Time ceilings" covers the run, phase and idle ceilings on `check`,
`test` and `clippy`, and the per-test cap. "lock" covers what `brokkr lock`
shows about the holder and the queue, "kill" what `brokkr kill` stops,
"Strays" the stray and orphan reaps, and "guard" the rustc guard.
"Refusals" lists what these commands refuse and how to get past each.

## Time ceilings

Time ceilings bound how long a `check` run, and each of its phases, may hold
the lock. The per-test and idle bounds are applied by the test runner; the
check watchdog applies the phase and whole-run bounds.

Libtest lanes use brokkr's JSON event stream, and a forwarded `--format` value
can't replace it. Raw passthrough also refuses `--format` and
`--test-threads`; thread counts come from the lane policy.

A whole `check` run may hold the lock for at most 25 minutes, measured from
lock acquisition. A wait behind another brokkr command isn't charged. Each
phase has its own ceiling on top of that:

| Phase | Ceiling |
|---|---|
| `clippy`, `rustdoc` | 5 min |
| `prepare`, `test`, `member_check`, `fixture_lint` | 15 min |
| `coverage`, `script_check` | 5 min |
| every other phase | 2 min |

The other phases are `gremlins`, `header`, `textlint`, `manifest`,
`dependency_rules` and `publish_cycle`.

`prepare` runs on every run that runs tests (see the `coverage` phase and
"Diagnostic continuation" in `brokkr man check`). It gets the test phase's
fifteen minutes because it does the test phase's builds and every listing.
Those builds are every lane's `cargo test --no-run`, cold on a fresh store. A
ceiling that fires in `prepare` leaves an explicitly incomplete plan
(`plan_complete = false`), never a partial one passed off as whole.

The per-child deadlines bound one invocation each: the 20s hung-test watchdog
and the captured runner's deadline. Nothing else bounds a phase or the run as
a whole, and one clippy invocation was observed holding the lock for 1h15m.
Clippy's five minutes is set against that hang, not its normal cost: warm, the
largest consuming workspace finishes clippy in about 20s.

The phase clock restarts at every phase entry, so a `script_check` stage that
runs three times gets five minutes each time. The same step points the
summary's `failed_phase` at the phase in flight, so the two can't drift. A
`background` script check runs across phases, so it isn't on the phase clock:
it gets the `script_check` ceiling measured from its own start, and the run
ceiling bounds it like everything else.

The same phase ceilings bound the two commands that share `check`'s
pipelines, with no whole-run ceiling:

- `brokkr clippy` runs under the `clippy` ceiling.
- `brokkr test` runs under the `test` ceiling, restarted for every sweep and
  for every `-N` iteration after that. A sweep's clock covers its pre-build,
  its `--timeout` enumeration and its first run.

### Ceiling kills

When a ceiling fires, brokkr kills the run's processes the way `brokkr kill
--hard` would, not the cooperative way. The cooperative path depends on
whichever runner is polling the shutdown flag, and a run that has overrun by
this much is the one whose runner may not be polling.

The watchdog thread SIGKILLs every descendant of the brokkr process found in
`/proc`, each checked against its starttime first. Test children are spawned
into their own process groups, and not all of them are published to the
lockfile. So neither a group signal nor the lockfile's published children
would reach them all. `[error]` lines say which ceiling fired and how many
processes were killed.

Between those two lines, before anything is killed, a third lists what was in
flight. It shows every live descendant as a tree, with its PID, age and
command line. Under any process blocked on a file lock (brokkr included), it
shows the locked file and the process holding it, read from `/proc/locks`.

The ceiling names only the phase whose clock ran out. The listing names what
the run was waiting on, which can be something else:

- a `background` script check running beside the phases
- a cargo parked on another cargo's lock, whose `Blocking waiting for file
  lock` line went into a captured stream

The listing has two seconds to be collected, since reading `/proc` can block
behind a process wedged in the kernel. Past that, the kill goes ahead and the
line says the listing couldn't be made.

brokkr itself isn't killed. The watchdog raises the shutdown flag and keeps
the subtree dead while the main thread unwinds through its normal exit path.
That path prints `check stopped by its time ceiling in ...` and exits 124
(`timeout(1)`'s convention). So the run is recorded in `brokkr history`, the
run log and status line are closed, and the lock's drop restores a
`disable_toolchain` pin.

The main thread may be blocked somewhere no kill releases. If it hasn't
unwound 20 seconds after the kill, the watchdog exits the process itself,
restoring the toolchain pin and recording the history row first. The kernel
then releases the flock, and lockfile readers fail closed on the stale
metadata exactly as after a hard kill.

### Idle ceiling

The libtest runner has a third clock, the 5-minute idle ceiling, shared by
`check`'s test phase and `brokkr test`. The 20s per-test watchdog ages a test
only after libtest announces it. So it's blind to everything around the
tests: the compile and link inside `cargo test`, and the binary's teardown
after the last result.

When no test has been in flight for five minutes, the watchdog kills the
process group led by cargo or by a directly run test binary. It reports the
killed group through the hung-test path, blamed as `(no test in flight)`,
with the usual `/proc` snapshot. The cargo-led case is the one it was built
for: cargo parked on `Blocking waiting for file lock on build directory`,
because a rust-analyzer `cargo check` held the target lock for over an hour,
with nothing for the per-test clock to age.

`brokkr test`'s sweep pre-build and its `--timeout` enumeration run outside
the libtest runner. They get the same five minutes as an output-inactivity
bound: every chunk cargo prints (each `Compiling ...` line) restarts it. So a
long cold build that keeps reporting progress is never killed, while a cargo
parked on a lock is. Their total wall time falls under the `test` phase
ceiling.

A `brokkr test -N` repeat run has no whole-run ceiling. Each iteration is
bounded by the idle clock, the per-test one and the `test` phase ceiling.

### Per-test cap

**Every test brokkr runs gets 20 seconds of wall time and no more.
Exceeding it fails the run and brokkr stops.** There is exactly one
exception: `brokkr test --timeout SECS` raises the cap for one resolved test.
The CLI parser hard-caps that flag at 280 seconds.

"Stops" is literal: a blown budget isn't a failing test. A failing test lets
the phase carry on so it can report every failure it can reach. A timeout
means the run is already outside the contract every later measurement
assumes. Whatever wedged it is still there for the next sweep to inherit: a
deadlock, a lock nobody will release, a runaway child.

So a timeout propagates as an error out of the sweep loop and ends the run,
in `check` and in `brokkr test` alike. That includes between `-N` iterations,
where the repeat summary still prints for the iterations that did run.

### Two clocks

Two measurements enforce the per-test cap, because a test can hide in two
ways:

1. **A test brokkr is watching** runs past 20s.
2. **Nothing completes** for 20s while a suite is under way. A lost start
   record produces this case: with no start, nothing is in flight to age, so
   without this clock the only bound would be the five-minute idle ceiling.
   Once a suite announces itself, brokkr must see a completion at least every
   20s. If it doesn't, either a test is over budget or its records were lost,
   and the budget is blown either way.

Before any suite announces itself there's no test to bill, so that window
belongs to the idle ceiling instead.

### Timeout reports

A timeout report names only what varies per occurrence:

- the clock that fired
- the time it measured against its limit: one number when the kill was on
  time, both when they differ
- whether SIGKILL was sent to the process group, and the child processes
  a reading saw in it. A sent signal is not an observed exit.
- each candidate thread: a thread of the test binary whose name matches a
  test it had in flight. Its line gives the state, wait channel and
  syscall read before the stop, and its state after the stop. It gives
  CPU time and time spent waiting for a CPU between a reading taken a
  few seconds before the deadline and the live one, and over the
  thread's life. It also lists the CPUs the thread may run on. The lines
  are observations, not a verdict. Cargo's shared stream names no thread.
- brokkr's own concurrency at the deadline: test group leaders not yet
  reaped, and live harnesses in the session. Then the load average from
  the reading, and pressure growth where the kernel reports PSI.
- when the stop and the kill were sent, and how complete each reading was
- the snapshot directory, under `.brokkr/test-hung/`. Its `evidence.txt`
  holds the lines above and every thread of every reading.

brokkr waits at most half a second after the deadline for the readings,
then attempts the kill whether or not they finished. On a kernel before
Linux 6.9, a group whose leader brokkr has already reaped is not
signalled, since its number may belong to another group by then.

A snapshot outlives the run, a later interrupt included. Only `brokkr clean`
and the retention bound (the newest 16) remove it.

The three clocks mean:

- **Per-test timeout**: a test exceeded its cap (normally 20s), or a suite
  made no progress for that long after a start record was lost. Cargo build
  time is excluded. A shared harness's named test is a suspect; only an
  invocation resolved to one exact test gives a verified name.
- **Wall deadline**: the run's authoritative bound. It's measured from a
  clock nothing the tests print can reach, so unlike the per-test cap, output
  that swallows a libtest record can't mislead it. No offender is named,
  because expiry says nothing about which test caused it. The tests in flight
  are listed as unverified context: "in flight when the deadline expired" is
  not "caused the timeout".
- **Idle ceiling**: no test in flight for the idle window. Cargo wedged
  before the first test or after the last, covering the compile, link and
  teardown the per-test cap can't see. A build-directory lock held by another
  cargo (a rust-analyzer `cargo check`, say) looks exactly like this.

### Stopping per lane

Stopping on a timeout has a price in two lanes:

- **The parallel-binaries lane** runs a bounded pool of workers over a queue
  of binaries, each waiting for its budget slots before it starts.
  Cancellation is lane-wide. The first binary to blow its budget sets a
  shared flag, and queued binaries return without starting. Every binary
  already executing sees the flag within one watchdog poll and kills its own
  process group. Binaries never started are reported as skipped.
- **The nextest lane** doesn't enumerate every failure in one run. In that
  engine a timeout arrives as a failed test, so `MaxFail::All` would keep
  running later tests after the cap. Nextest can't cancel on a timeout
  without cancelling on a failure, so the lane uses `MaxFail::Count {
  max_fail: 1, terminate: Immediate }`. brokkr recognizes a timed-out
  terminal event as a per-test stop and ends the test phase; ordinary
  failures use the same fail-fast lane behavior. The cap isn't negotiable and
  the failure list is. Its `grace-period` is `0s`, because a grace period
  SIGTERMs at the ceiling and SIGKILLs only afterwards, which lets a test that
  blocks SIGTERM outlive its budget.
### Interrupts

Interrupting brokkr stops the tests too. The libtest runners put their
children in their own process groups, so the watchdog can kill a whole test
tree. That puts them out of reach of terminal Ctrl-C and of `brokkr kill`'s
SIGTERM to brokkr's pid.

So brokkr registers every test group, and the signal handler SIGKILLs every
registered group before anything else. Under `check`'s and `brokkr test`'s
signal guard, the handler then sets the shutdown flag and the run unwinds as
interrupted. Outside a guard, it re-raises the signal and brokkr dies,
taking its tests with it. The nextest engine handles SIGINT and SIGTERM
itself while it runs, and the orphan reap covers its test processes too.

A SIGKILLed brokkr runs no handler. So `brokkr kill --hard` stops brokkr and
SIGKILLs every process beneath it itself, which reaches a test binary under
cargo. It verifies every recorded child and mock by starttime first, and
refuses the whole kill on one it can't verify. The exception is a recorded
process that has provably exited, or whose pid now names another process: it
is skipped with a line saying so. So a wedged holder whose child already died
stays killable.

Such a recorded process may have led a process group, or been killed without
a group signal: it exited before the signal arrived, or was signalled through
its pidfd alone. Either way, its group's surviving members are SIGKILLed one
by one. Each is killed only if it has the hold's launch cohort (`BROKKR_EXEC`)
and still sits in that group: a group number is reused once its members are
all gone, so a group signal could reach a stranger.

A child missing from the record kill read, because brokkr started it after
the read or had not recorded it yet, is found by the walk while it lives
beneath brokkr. If it leads a group of its own, that group's members that
left brokkr's tree are swept the same way. A group whose leader already
exited or left the tree is reachable only through the record.

A cooperative stop admits no new launch. Once the holder accepts it, every
launch is refused, before its fork or by the child itself as its last step
before the exec, and the command unwinds as interrupted. A child that passed
that last step before the stop may still start its program just after it.
It is an ordinary child, which the unwind stops.

Against any other SIGKILL (the OOM killer), the runners' direct child has a
parent-death SIGKILL. That reaches the test binary on the parallel lane but
only cargo on the others. Whatever survives below cargo is killed by the
orphan reap at the next brokkr command that takes the lock (see "Strays"):
the test binary, a doctest, anything a test spawned.

## lock

`brokkr lock` says who holds the lock and what it's doing right now. It asks
the holder itself over its control socket, so it works from the host against
a sandboxed holder and between sandboxes. A typical report looks like this:

```text
[lock]    brokkr (holding 29m24s, own CPU 1.2s, own RSS 19 MB): elivagar: brokkr bench ...
[lock]    root: ...
[lock]    doing: bench tilegen (29m10s)
[lock]      iteration (3m02s)
[lock]    processes: 1 observed (sampled 248ms ago over 254ms): 0.98 cores,
          summed RSS 4.1 GB
[lock]      PID 4242 elivagar ... - 0.98 cores, CPU 2m55s, RSS 4.1 GB,
            up 3m02s, io r 2.1 GB w 800 MB [under bench tilegen]
[lock]    cgroup brokkr-...: CPU 1h02m since entry 29m ago, now 1.01 cores,
          3 member process(es)
[lock]    last marker: STAGE2_START (12s ago)
```

The long lines are wrapped here; brokkr prints each `[lock]` entry on one
line. The lines mean:

- **The first line is brokkr itself.** Its CPU and RSS are the holder's own,
  mostly a process waiting on its children. The work is in the lines below.
- **`doing:`** lists the holder's open activities, nested. These are the
  `check` phase in flight (with the lane's status line beside it), a parallel
  lane with its binaries done out of total and each binary in flight,
  measured runs with run N of total, a cargo build, or a download. A long
  non-verbose `verify` also keeps that step as a live status line on its own
  terminal.
- **`processes:`** are the holder's processes, described under "lock
  processes".
- **`cgroup`** appears only where the holder's cgroup is delegated to you:
  under the systemd user manager, not a plain ssh or tty login session, whose
  scope belongs to the system. Each hold then runs in its own
  `brokkr-ACQUISITION_ID` cgroup. The line is everything charged to it since
  entry, exited and reparented processes and the holder itself included.
- **`last marker`** is the latest sidecar phase marker the holder has seen in
  this hold, with how long ago. It's a marker observed, not proof of the
  child's current step.

The holder never waits on `brokkr lock`: the socket serves the latest sample
and starts a fresh one. `brokkr lock` asks once more half a second later when
the first answer had no sample or said a fresh one was being collected. So
the rows it prints are under a second old. A field the holder's reply leaves
out prints no line.

### Processes

The `processes:` rows come from two `/proc` scans about a quarter second
apart. They list the holder's descendants, plus every process that has the
hold's launch token. Every child brokkr spawns under a hold gets
`BROKKR_EXEC=ACQUISITION_ID.SEQUENCE`, inherited by everything below it. So
a grandchild that reparented or outlived its parent is still found.

Each row names the step it was launched under:

- `[under STEP]` on the process brokkr spawned
- `[from LAUNCH under STEP]` on one that process started, where LAUNCH is the
  spawned program and its first arguments

A child that exited and waits for its parent to reap it is counted in the
header as "exited awaiting reap", not listed.

Treat the list as observed, not complete. A process that cleared its
environment, or work a daemon does on the hold's behalf, isn't in it. The
"environment(s) not inspectable" count covers processes whose membership
couldn't be checked and could still be the hold's. They aren't already
listed, and they started after the hold began, since an older process can't
have its token. So the same user's long-lived unrelated processes don't show
up there.

Rows form a tree. A process comes before the processes it started, indented
beneath it. A process whose parent isn't in the sample, or was cut by the
size bound, stands at the top level. Siblings stay busiest first.

The figures on a row follow these rules:

- A row's CPU rate needs the process in both scans with the same token.
- The summed RSS counts shared pages once per process.
- IO counters include children the process has already waited for.
- PIDs are printed only when they mean the same thing to you as to the
  holder.

`BROKKR_EXEC` is reserved like the hold variables under "guard": a sweep env
value for it is ignored. A nextest lane's processes share one launch token
for the lane.

### Silent holder

When the holder doesn't answer (a stopped holder, say) and runs
in your namespace, `brokkr lock` samples its processes from your side with
the same scanner. It says so if the hold ended meanwhile. Activities and the
marker live in the holder's memory and aren't available then.

That output opens with `brokkr PID N`, not `brokkr (holding ...)`, and a line
saying the socket gave no status, why, and whether the holder is stopped.
Rows name their launch by cohort number only. "Not inspectable" counts
processes started after the holder process itself, the nearest start this
side knows. A holder in another PID namespace can't be sampled from here, and
`brokkr lock` says so.

### Queue

Waiters take the lock in the order they joined the queue. A command passes an
older waiter only when that waiter has stopped polling for a second (stopped,
or starved of CPU). The passed waiter loses its place and rejoins at the back
when it resumes. A command that doesn't wait for the lock refuses while a
waiter is ahead of it, even in the moment after a release.

A small mutex beside the queue serializes admission. A command stopped while
it holds that mutex stops admission until it resumes or dies, though
`brokkr kill` still cancels waiters.

After the holder's lines, or `no active lock`, `brokkr lock` prints one
`queued:` line per brokkr command waiting for the lock, in admission order.
Each line gives the command's project and invocation, how long it has waited,
its state root, and its PID where that means the same thing to you. A command
with no `brokkr.toml` shows as `(no brokkr.toml)` in place of a project, the
holder too. A waiter whose cancel committed but which hasn't yet let go shows
as `queued, cancelled`.

Then `brokkr lock` prints one `serve:` line per live `brokkr serve` server,
since a serve holds no lock but makes every measurement refuse. It only
looks: the commands that act on them clean what a dead waiter or a stopped
serve left (`brokkr man serve`).

## kill

`brokkr kill` stops your own project's brokkr commands, never another
project's unless you ask. Several projects share one host and one lock, so a
kill meant to cancel this project's queued command must not take down another
project's holder and let the queued one run.

The scope is the `project` key of `brokkr.toml`:

- A second checkout of the same project, or a `--commit` worktree, is the
  same scope.
- Without a `brokkr.toml`, the scope is the commands run with none under the
  same state root (the working directory). So two directories of one
  unconfigured repository are two scopes.
- Where a `brokkr.toml` is present but doesn't load, a bare kill can't know
  the key and refuses.
- `--project NAME` takes the commands of the project with that key instead,
  and `--all` takes every project's.

In the scope, kill does three things, in order:

1. **Queued commands are cancelled.** Every brokkr command waiting for the
   lock is recorded in the lock queue before it prints its wait line. A kill
   marks the scope's records cancelled, without a signal, so it reaches a
   waiter in any PID namespace. The waiter prints that it was cancelled and
   didn't run, and exits 130. A cancelled waiter never runs, even one that was
   stopped or had just won the lock. Kill waits up to two seconds for the
   cancelled waiters to let go and says which haven't. A kill covers every
   command that had printed its wait line before it started; one starting at
   the same moment can escape it.
2. **The lock holder is stopped when it's in the scope**, read after the
   cancels. Without `--hard`, that's the cooperative stop over its control
   socket (`brokkr man measure run-lifecycle` has the protocol, `brokkr man
   run` what `run` and `bench` do with it). With `--hard`, it's the SIGKILL
   of the holder and everything beneath it. `--hard` changes only this step;
   queued commands are cancelled the same way with or without it. A holder
   outside the scope is left alone, and kill names it and the form that would
   stop it (`--project NAME`, or `--all`).
3. **Serves are listed, not stopped.** A `brokkr serve` whose start returned
   holds no lock and is no command in flight. Kill prints each of the scope's
   live serves as left running, with the `brokkr serve --stop TOKEN` that
   stops it. A serve still starting holds the lock and is stopped like any
   holder, taking the server it was starting down with it.

Queued commands outside the scope are listed as left waiting. Say a nidhogg
checkout has a serve running and a `brokkr check` holding the lock. There,
`brokkr kill` stops the check, cancels any other queued nidhogg commands, and
leaves the server running, saying so. Another project's queued commands keep
their places.

Kill exits 0 when everything in its scope was stopped or cancelled, including
when there was nothing to stop. It exits 1 in either of these cases:

- A stop failed, a refused or failed signal included.
- A holder or queued command couldn't be placed in or out of the scope,
  because its record can't be read. Under `--all` an unreadable holder record
  fails too: the holder is in scope but can't be addressed.

## Strays

brokkr assumes every cargo on a development host runs through it. A cargo
running outside brokkr competes with whatever is being measured. It also
takes cargo's build-directory lock, on which brokkr's own cargo then blocks
with nothing to time out (a rust-analyzer `cargo check` once held it for over
an hour).

So every locked command, once it holds the lock, reaps cargo-family processes
that have no verified brokkr owner: cargo, rustc, rustdoc, clippy-driver and
build scripts. It kills leaves first, and prints one `[strays]` line naming
what it killed and who started it. The reap follows these rules:

- A rust-analyzer starter is killed too, since it would only restart the
  cargo. A shell or editor starter never is.
- This brokkr, its ancestors and its descendants are always spared.
- Every signal is checked against the process's starttime, so a reused pid is
  never hit.
- Formatters (`cargo fmt`, `cargo-fmt`) are never strays: they take no build
  lock and compile nothing, and `brokkr fmt` usually runs without the lock.

A second reap at the same point kills orphaned test processes. These are
tests left running by a brokkr that died without a handler (the OOM killer).
The reap finds them by the run token every test child has, not by name.

`brokkr strays` is the by-hand form. Bare, it lists; with `--kill`, it lists
then kills. It doesn't list orphans, and it works with no `brokkr.toml`.
## guard

The rustc guard stops strays compiling at all, where the reap removes them
after the fact. `brokkr guard --install` enrolls `brokkr-rustc-guard`
(installed next to `brokkr`) as `build.rustc-wrapper` in
`$CARGO_HOME/config.toml`.

From then on, while a brokkr holds the lock, a compile is admitted only if it
descends from that hold, which means it has the hold's capability in its
environment. Anything else is refused at its first real compile. With no
hold, everything compiles. Compiler-information queries (`rustc -vV`,
`--print`) are always admitted, because cargo caches a refused probe and
replays it to later, legitimate cargos.

The guard follows these rules:

- `BROKKR_CARGO=1` is the human escape hatch for one invocation. brokkr never
  sets it on its own children.
- A poisoned probe cache seen outside brokkr is cured by
  `CARGO_CACHE_RUSTC_INFO=0 BROKKR_CARGO=1 cargo ...`.
- `BROKKR_HOLD_NONCE`, `BROKKR_COMPILE_LEASE`, `CARGO_CACHE_RUSTC_INFO` and
  `BROKKR_EXEC` (the launch token under "lock") are reserved: configs,
  `[[check]] env` and build scripts must not set them.
- A fresh acquisition drains compiles admitted earlier, and can **fail** if
  they don't drain in time. It reports that rather than measuring alongside
  them.
- The guarantee is conditional. Compiles that take part in the lease protocol
  are excluded from protected work, but `BROKKR_CARGO` and a guard whose lock
  files are unreachable (it fails open) can overlap it.
- Enrolling costs one full rebuild per target dir, once. The wrapper is part
  of cargo's fingerprint, which is also why brokkr never bypasses it for its
  own builds.

Bare `brokkr guard` shows status and warns about a missing or out-of-date
guard binary; brokkr also warns at each locked command. `--install` refuses
to overwrite a wrapper brokkr didn't write (sccache, say), and `--remove`
unsets brokkr's own. The command works with no `brokkr.toml`.

`brokkr-rustc-guard` exits 1 when it refuses a compile, 2 when called
without a wrapped command, and 127 when it can't exec the wrapped compiler,
including the version handshake on an old guard.

### Descendant refusal

A locked brokkr started beneath a command that holds the lock refuses rather
than waits. Examples are `brokkr run brokkr -- test NAME`, or a script check
or test that shells out to brokkr. Such a brokkr would wait on the lock while
its ancestor waits for it to exit. Instead it recognizes the inherited
capability and refuses at once, naming the holder.

Run the inner command outside the outer one; for brokkr itself, invoke the
built `target/debug/brokkr` directly. A brokkr started from inside an
admitted compile (a proc macro shelling out to cargo) refuses the same way.
## Refusals

The table lists what the lock, `kill` and `guard` refuse, with the text
brokkr prints after `[error]`. Words in capitals stand for values brokkr
fills in:

| Message | Cause | Fix |
|---|---|---|
| `PATH/brokkr.toml does not load, so a bare brokkr kill cannot know this project's key` | A bare `brokkr kill` found a `brokkr.toml` that doesn't load, so it has no scope (see "kill"). | Fix the config, or name the scope with `brokkr kill --project NAME` or `--all`. |
| `lock: the holder runs in another PID or time namespace (a sandboxed command)` | `--hard` signals by PID, and the holder's PIDs mean nothing in your namespace. | Run `brokkr kill` without `--hard`, or stop the session that owns the holder. |
| `lock: ROLE PID N: identity could not be verified from this namespace - ... no signals were sent` | `--hard` couldn't verify a recorded process by starttime, so it sent nothing (see "Interrupts"). The same suffix follows `could not open a pidfd` and `process changed identity during verification`. | Run `brokkr lock` to see the holder, then retry, or use `brokkr kill` without `--hard`. |
| `lock: ..., and the holder runs in a PID or time namespace this process cannot verify` | The holder didn't answer its control socket, and the numeric SIGTERM fallback needs your namespace. | Stop the session that owns the holder, or run the kill from the holder's namespace. |
| `lock: stop delivered but not confirmed` | The holder got the stop but didn't answer, and still held the lock a few seconds later. | Run `brokkr lock` to see whether it is stopping; use `brokkr kill --hard` if it is wedged. |
| `lock: the lock is held, but its holder's record cannot be read` | The holder record didn't parse after several re-reads, so kill couldn't place it in or out of the scope. | Wait a moment and rerun `brokkr kill`. |
| `lock: cannot take the brokkr lock independently while the hold you were started under is active` | A locked brokkr was started beneath the command holding the lock (see "Descendant refusal"). | Run the inner command outside the outer one, or invoke `target/debug/brokkr` directly. |
| `lock: refusing to take the brokkr lock from inside a compilation brokkr admitted` | A build script or proc macro ran brokkr inside a compile the guard admitted. | Run the brokkr command outside the build. |
| `lock: the brokkr lock is held ... and this command does not wait for it` | A command that doesn't wait found the lock held. | Rerun once `brokkr lock` shows no holder. |
| `lock: commands are queued for the brokkr lock and this command does not wait for it` | A command that doesn't wait found a waiter ahead of it (see "Queue"). | Rerun once `brokkr lock` shows no `queued:` line. |
| `lock: compilation leases did not drain within Ns` | A compile admitted before this hold still holds its lease (see "guard"). | Run `brokkr strays` to find it, stop it, and rerun. |
| `lock: cannot record this command in the brokkr lock queue` | The queue directory couldn't be written. `cannot claim this command's place` and `cannot read or update the brokkr lock queue's admission order` are the same failure later in the wait. | Fix the cause in the parenthesized error, then rerun. |
| `cancelled by brokkr kill while waiting for the brokkr lock; this command did not run` | A `brokkr kill` in this command's scope cancelled it while it was queued (see "kill"). | Rerun the command. |
| `stopped by brokkr kill; this command starts no new work` | A `brokkr kill` stopped this command, and it then tried to take the lock or start work under it. It exits 130, as an interrupt does. | Rerun the command. |
| `stopped by its time ceiling` | A `check` run or phase overran its ceiling (see "Time ceilings"). | Read the in-flight listing above it to find what was waiting, and fix that. |
| `config: PATH already sets build.rustc-wrapper = "WRAPPER" - refusing to overwrite a wrapper brokkr did not write` | `guard --install` found another tool's wrapper, sccache say. | Remove that wrapper from the cargo config yourself, then rerun `brokkr guard --install`. |
| `config: PATH already sets build.rustc-wrapper to a brokkr guard from another install` | Another brokkr install's guard is enrolled. | Run `brokkr guard --remove`, then `brokkr guard --install`. |
| `config: GUARD not found next to the running brokkr` | `brokkr-rustc-guard` isn't installed beside `brokkr`. | Run `brokkr install` in the brokkr repository. |
| `config: build.rustc-wrapper = "WRAPPER" is not a brokkr guard - refusing to remove a wrapper brokkr did not write` | `guard --remove` found another tool's wrapper. | Leave it, or remove it from the cargo config yourself. |
| `PATH exists but cannot be read; cargo would read it instead of CONFIG once it can` | A legacy `config` file in `$CARGO_HOME` exists but can't be read. | Fix or remove the legacy file, then rerun. |
| `PATH is not a regular file; brokkr only edits a regular file` | The cargo config is a directory or other special file. `is a dangling symlink` is the same refusal for a broken link. | Replace it with a regular file, or point the link at one. |
