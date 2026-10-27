# Time ceilings, strays, lock status and the rustc guard

What bounds, protects and reports every locked brokkr command: the time
ceilings and per-test cap on `check`, `test` and `clippy`, the stray and orphan
reaps, what `brokkr lock` shows about a holder, and the rustc guard. Their
internals are in `reference/check-internals.md`.

## Time ceilings

The time bounds and their ordering live in `src/test_runner/ceilings.rs`.
`test_runner` applies the per-test and idle bounds; the check watchdog applies
the phase and whole-run bounds. Libtest lanes use brokkr's JSON event stream,
and forwarded `--format` values cannot replace it. Raw passthrough also refuses
`--format` and `--test-threads`; thread counts come from the lane policy.

A whole `check` run may hold the lock for at most 25 minutes, measured from
lock acquisition (a wait behind another brokkr command is not charged), and
each phase has its own ceiling on top of that:

| Phase | Ceiling |
|---|---|
| `clippy`, `rustdoc` | 5 min |
| `prepare`, `test`, `member_check`, `fixture_lint` | 15 min |
| `coverage`, `script_check` | 5 min |
| every other phase (`gremlins`, `header`, `textlint`, `manifest`, `dependency_rules`, `publish_cycle`) | 2 min |

`prepare` (every run that runs tests; see the `coverage` phase and
"Diagnostic continuation") gets the test phase's fifteen minutes because it
does the test phase's builds - every lane's `cargo test --no-run`, cold on a
fresh store - plus every listing. A ceiling that fires there leaves an
explicitly incomplete plan (`plan_complete = false`), never a partial one
passed off as whole.

The per-child deadlines - the 20s hung-test watchdog, the captured runner's
deadline - bound one invocation each; nothing bounded a phase or the run as a
whole, and one clippy invocation was observed holding the lock for 1h15m.
Clippy's five minutes is set against that hang, not against its normal cost:
warm, the largest consuming workspace finishes clippy in about 20s. The phase
clock restarts at every phase entry (`begin_phase`, which also points the
summary's `failed_phase` at the phase in flight, so the two cannot drift), so a
`script_check` stage that runs three times gets five minutes each time. A
`background` script check runs across phases, so it is not on the phase clock:
it gets the `script_check` ceiling measured from its own start, and the run
ceiling bounds it like everything else.

The same phase ceilings bound the two commands that share `check`'s pipelines,
with no whole-run ceiling: `brokkr clippy` runs under the `clippy` ceiling, and
`brokkr test` under the `test` ceiling, restarted for every sweep (covering its
pre-build, its `--timeout` enumeration and its first run) and for every `-N`
iteration after that.

When a ceiling fires, the run's processes are killed the way `brokkr kill
--hard` would kill them, not the cooperative way: the cooperative path depends
on whichever runner is currently polling the shutdown flag, and a run that has
overrun by this much is the one whose runner may not be polling. The watchdog
thread (`src/check_cmd/watchdog.rs`) SIGKILLs every descendant of the brokkr
process found in `/proc`, each checked against its starttime first - test
children are spawned into their own process groups and not all of them are
published to the lockfile, so neither a group signal nor the lockfile's
published children would reach them all. Two `[error]` lines say which ceiling fired
and how many processes were killed.

brokkr itself is not killed. The watchdog raises the shutdown flag and keeps
the subtree dead while the main thread unwinds through its normal exit path,
which prints `check stopped by its time ceiling in ...` and exits 124
(`timeout(1)`'s convention) - so the run is recorded in `brokkr history`, the
run log and status line are closed, and the lock's drop restores a
`disable_toolchain` pin. Only if the main thread has not unwound 20 seconds
after the kill (blocked somewhere no kill releases) does the watchdog exit the
process itself, restoring the toolchain pin and recording the history row
first; the kernel then releases the flock and lockfile readers fail closed on
the stale metadata exactly as after a hard kill.

The libtest runner carries a third clock, shared by `check`'s test phase and
`brokkr test`: the 5-minute **idle ceiling** (`IDLE_TIMEOUT` in
`src/test_runner/ceilings.rs`, re-exported from `src/test_runner.rs`). The 20s
per-test watchdog ages a test only after
libtest announces it, so it is blind to everything around the tests - the
compile and link inside `cargo test`, the binary's teardown after the last
result. When no test has been in flight for five minutes the watchdog kills
the process group led by cargo or by a directly run test binary and reports
which process group it killed through the hung-test path, blamed as `(no test
in flight)` with the usual `/proc` snapshot. The cargo-led case it was built
for: cargo parked on `Blocking waiting for file lock on build directory`
because a rust-analyzer `cargo check` held the target lock - over an hour,
with nothing for the per-test clock to age. `brokkr test`'s sweep pre-build
and its `--timeout` enumeration run outside the libtest runner, so they carry
the same five minutes as an output-inactivity bound: every chunk cargo prints
(each `Compiling ...` line) restarts it, so a long cold build that keeps
reporting progress is never killed, while a cargo parked on a lock is. Their
total wall time falls under the `test` phase ceiling. A `brokkr test -N` repeat run has
no whole-run ceiling; each iteration is bounded by this clock, the per-test
one and the `test` phase ceiling.

### The per-test budget is a hard cap

**Every test brokkr runs gets 20 seconds of wall time and no more. Exceeding it
fails the run and brokkr stops.** There is exactly one exception:
`brokkr test --timeout <SECS>`, which raises the cap for one resolved test and is
itself hard-capped at 280 seconds by the CLI parser.

"Stops" is literal. A blown budget is not a failing test: a failing test lets the
phase carry on so it can report every failure it can reach, but a timeout means
the run is already outside the contract every later measurement assumes, and
whatever wedged it - a deadlock, a lock nobody will release, a runaway child - is
still there for the next sweep to inherit. So a timeout propagates as an error out
of the sweep loop and ends the run, in `check` and in `brokkr test` alike
(including between `-N` iterations, where the repeat summary still prints for the
iterations that did run).

### Two clocks, because a test can hide in two ways

The cap is enforced by two measurements, not one:

1. **A test brokkr is watching** runs past 20s. The obvious case.
2. **Nothing completes** for 20s while a suite is under way. This is the case a
   lost *start* record produces, and it escaped entirely at first: with no start
   there was nothing in flight to age, so the only remaining bound was the
   five-minute idle ceiling and a test could run for minutes under a
   twenty-second contract. Once a suite announces itself, brokkr must see a
   completion at least every 20s; if it does not, either a test is over budget or
   its records were lost, and the budget is blown either way.

Before any suite announces itself there is no test to bill, so that window belongs
to the idle ceiling instead.

### Reading a timeout report

The report names only what varies per occurrence: the clock that fired, the
time it measured against its limit (one number when the kill was on time, both
when they differ), the killed process group and test children, whichever of
`/proc/<pid>/wchan` and `/proc/<pid>/stack` could be read (the stack is
normally unreadable without root, so its line is simply absent), and the
snapshot directory. The three clocks mean:

- **Per-test timeout** - a test exceeded its cap (normally 20s), or a suite
  made no progress for that long after a start record was lost. Cargo build
  time is excluded. A shared harness's named test is a suspect; only an
  invocation resolved to one exact test gives a verified name.
- **Wall deadline** - the run's authoritative bound, measured from a clock
  nothing the tests print can reach, so unlike the per-test cap it cannot be
  misled by output that swallows a libtest record. No offender is named, because
  expiry says nothing about which test caused it; the tests in flight are listed
  as unverified context ("in flight when the deadline expired" is not "caused
  the timeout").
- **Idle ceiling** - no test in flight for the idle window: cargo wedged before
  the first test or after the last, covering the compile, link and teardown the
  per-test cap cannot see. A build-directory lock held by another cargo (a
  rust-analyzer `cargo check`, say) looks exactly like this.

### What stopping costs, per lane

"Stops" has a price in two lanes and it is worth naming:

- **The parallel-binaries lane** creates every binary's thread up front and they
  wait for a budget slot. A killed binary used to release its slots and let the
  next queued binary start, so the run continued past a timeout. Cancellation is
  now lane-wide: the first binary to blow its budget sets a shared flag, queued
  binaries return without starting, and every binary *already executing* sees the
  flag within one watchdog poll and kills its own process group. Binaries never
  started are reported as skipped.
- **The nextest lane** no longer enumerates every failure in one run. In that
  engine a timeout arrives as a failed test, so `MaxFail::All` would keep running
  later tests after the cap. There is no way to cancel on a timeout without
  cancelling on a failure, so the lane uses `MaxFail::Count { max_fail: 1,
  terminate: Immediate }`. Brokkr recognizes a timed-out terminal event as a
  per-test stop and ends the test phase; ordinary failures still use the same
  fail-fast lane behavior. The cap is not negotiable and the failure list is.
  Its `grace-period` is also `0s`: a grace period SIGTERMs at the ceiling and only
  SIGKILLs afterwards, which lets a test that blocks SIGTERM outlive its budget.

Interrupting brokkr stops the tests too. The libtest runners put their
children in their own process groups (so the watchdog can kill a whole test
tree), which puts them out of reach of terminal Ctrl-C and of `brokkr kill`'s
SIGTERM to brokkr's pid. So every test group is registered
(`shutdown::GroupReaper`), and the signal handler SIGKILLs every registered
group before anything else: under `check`'s and `brokkr test`'s
`SigtermGuard` it then sets the shutdown flag and the run unwinds as
interrupted; outside a guard it re-raises the signal and brokkr dies as before,
taking its tests with it. A SIGKILLed brokkr runs no handler. `brokkr kill
--hard` therefore stops brokkr and SIGKILLs every process beneath it itself,
which reaches a test binary under cargo. Against any other SIGKILL (the OOM
killer) the runners' direct child carries a parent-death SIGKILL, which
reaches the test binary on the parallel lane but only cargo on the others;
what survives below cargo - the test binary, a doctest, anything a test
spawned - is killed by the orphan reap at the next brokkr command that takes
the lock (see "Strays"). The nextest engine handles SIGINT/SIGTERM itself
while it runs, and its test processes are covered by the orphan reap too.

How the watchdog wakes and freezes a run, what the cap can prove on a shared
stream, the fail-closed rule for missing evidence, and the per-lane clock table
are in `reference/check-internals.md`.

## Lock status

`brokkr lock` says who holds the lock and what it is doing right now. It asks
the holder itself over its control socket, so it works from the host against a
sandboxed holder and between sandboxes:

```
[lock]    brokkr (holding 29m24s, own CPU 1.2s, own RSS 19 MB): pbfhogg verify merge ...
[lock]    root: ...
[lock]    doing: verify merge (29m10s)
[lock]      osmium apply-changes (3m02s)
[lock]    processes: 1 observed (sampled 248ms ago over 254ms): 0.98 cores, summed RSS 4.1 GB
[lock]      PID 4242 osmium apply-changes ... - 0.98 cores, CPU 2m55s, RSS 4.1 GB, up 3m02s, io r 2.1 GB w 800 MB [launched: osmium apply-changes under verify merge]
[lock]    cgroup brokkr-...: CPU 1h02m since entry 29m ago, now 1.01 cores, 3 member process(es)
[lock]    last marker: STAGE2_START (12s ago)
```

- **The first line is brokkr itself.** Its CPU and RSS are the holder's own -
  mostly a process waiting on its children. The work is in the lines below.
- **`doing:`** lists the holder's open activities, nested: the `check` phase
  in flight (with the lane's status line beside it), a parallel lane with its
  binaries done out of total and each binary in flight, a verify check and the
  tool step it is running, measured runs with run N of total, a cargo build, a
  download. A long non-verbose `verify` also keeps that step as a live status
  line on its own terminal.
- **`processes:`** are the holder's processes, from two `/proc` scans about a
  quarter second apart: its descendants, plus every process carrying the hold's
  launch token. Every child brokkr spawns under a hold carries
  `BROKKR_EXEC=<acquisition id>.<sequence>`, inherited by everything below it,
  so a grandchild that reparented or outlived its parent is still found, and
  each row names the step it was launched under. Treat it as observed, not
  complete: a process that cleared its environment, or work a daemon does on
  the hold's behalf, is not in it, and "environment(s) not inspectable" counts
  processes whose membership could not be checked. A row's CPU rate needs the
  process in both scans with the same token; the summed RSS counts shared
  pages once per process; IO counters include children the process has
  already waited for. PIDs are printed only when they mean the same thing to
  the caller as to the holder.
- **`cgroup`** appears only where the holder's cgroup is delegated to the user
  (under the systemd user manager - not a plain ssh or tty login session, whose
  scope belongs to the system). Each hold then runs in its own
  `brokkr-<acquisition id>` cgroup, and the line is everything charged to it
  since entry: exited and reparented processes included, the holder itself
  included.
- **`last marker`** is the latest sidecar phase marker the holder has seen in
  this hold, with how long ago - a marker observed, not proof of the child's
  current step.

The holder never waits on this: the socket serves the latest sample and starts
a fresh one, and `brokkr lock` asks once more half a second later when the
first answer had no sample or said a fresh one was being collected - so the
rows it prints are under a second old. An older holder answers without any of the new lines. When
the holder does not answer (an older brokkr, or a stopped one) and runs in the
caller's namespace, `brokkr lock` samples its processes from the caller's side
with the same scanner, and says so if the hold ended meanwhile; activities and
the marker live in the holder's memory and are not available then.

`BROKKR_EXEC` is reserved like the hold variables below: a sweep env value for
it is ignored. A nextest lane's processes share one launch token for the lane.

## Strays

brokkr assumes every cargo on a development host runs through it. A cargo
running outside brokkr competes with whatever is being measured and takes
cargo's build-directory lock, on which brokkr's own cargo then blocks with
nothing to time out (a rust-analyzer `cargo check` once held it for over an
hour).

So every locked command, once it holds the lock, reaps cargo-family processes
(cargo, rustc, rustdoc, clippy-driver, build scripts) that have no verified
brokkr owner, leaves first, and prints one `[strays]` line naming what it
killed and who started it. A rust-analyzer starter is killed too, since it
would only restart the cargo; a shell or editor starter never is. This brokkr,
its ancestors and its descendants are always spared, and every signal is
checked against the process's starttime so a reused pid is never hit.
Formatters (`cargo fmt`, `cargo-fmt`) are never strays: they take no build lock
and compile nothing, and `brokkr fmt` usually runs without the lock.

A second reap at the same point kills **orphaned test processes**: tests left
running by a brokkr that died without a handler (the OOM killer), found by the
run token every test child carries rather than by name.

`brokkr strays` is the by-hand form: bare lists, `--kill` lists then kills
(orphans are not listed). Works with no `brokkr.toml`. Ownership rules,
PID-namespace handling and the orphan token protocol are in
`reference/check-internals.md`.

## The rustc guard

The reap removes strays after the fact; the guard stops them compiling at all.
`brokkr guard --install` enrolls `brokkr-rustc-guard` (installed next to
`brokkr`) as `build.rustc-wrapper` in `$CARGO_HOME/config.toml`. From then on,
while a brokkr holds the lock, a compile is admitted only if it descends from
that hold (it carries the hold's capability in its environment); anything else
is refused at its first real compile. With no hold, everything compiles.
Compiler-information queries (`rustc -vV`, `--print`) are always admitted,
because cargo caches a refused probe and replays it to later, legitimate cargos.

- `BROKKR_CARGO=1` is the human escape hatch for one invocation. brokkr never
  sets it on its own children.
- A poisoned probe cache seen outside brokkr is cured by
  `CARGO_CACHE_RUSTC_INFO=0 BROKKR_CARGO=1 cargo ...`.
- `BROKKR_HOLD_NONCE`, `BROKKR_COMPILE_LEASE`, `CARGO_CACHE_RUSTC_INFO` and
  `BROKKR_EXEC` (the launch token above) are reserved: configs, `[[check]] env`
  and build scripts must not set them.
- A fresh acquisition drains compiles admitted earlier, and can **fail** if they
  do not drain in time. It reports that rather than measuring alongside them.
- The guarantee is conditional: compiles that take part in the lease protocol
  are excluded from protected work, but `BROKKR_CARGO` and a guard whose lock
  files are unreachable (it fails open) can overlap it.
- Enrolling costs one full rebuild per target dir, once: the wrapper is part of
  cargo's fingerprint, which is also why brokkr never bypasses it for its own
  builds.

`brokkr guard` bare shows status and warns about a missing or out-of-date guard
binary (brokkr also warns at each locked command). `--install` refuses to
overwrite a wrapper brokkr did not write (sccache, say); `--remove` unsets
brokkr's own. Works with no `brokkr.toml`.

### A descendant of the holder refuses rather than waits

A locked brokkr started *beneath* a command that holds the lock -
`brokkr run brokkr -- test NAME`, or a script check or test that shells out to
brokkr - would wait on the lock while its ancestor waits for it to exit. It
recognizes the inherited capability and refuses at once, naming the holder.
Run the inner command outside the outer one (for brokkr itself, the built
`target/debug/brokkr` invoked directly). A brokkr started from inside an
admitted compile (a proc macro shelling out to cargo) refuses the same way.

Admission, the compile lease, the decision order, the version handshake and the
guard decision probe are in `reference/check-internals.md`.
