# brokkr serve

`brokkr serve` runs a long-lived dev server in the background without holding
the global lock: a geocoder answering queries, a tile server, an API under
test. The client tools, checks and builds that talk to it or work beside it
can run meanwhile. It is available in any Rust project, with or without a
`brokkr.toml`. This page covers starting a serve, the bench-lock it imposes,
listing and stopping, what is guaranteed, its files and its exit statuses.

The command has three forms:

```text
brokkr serve                       # list the live serves, all projects
brokkr serve TARGET [build flags] [--name NAME]
             [--ready TEXT [--ready-timeout SECS]]
             [--grace SECS] [-- ARGS...]    # start (or replace) one
brokkr serve --stop [WHICH]        # stop one
```

`brokkr run` holds the lock for its program's whole life. That is right for a
program that finishes and wrong for one that serves: every brokkr command in
every project would wait for the server to stop, including a client that is
itself a `brokkr run`. `serve` splits the build stage, which holds the lock,
from the serving stage, which doesn't.

## Starting a serve

`brokkr serve TARGET` takes the lock as `run` does and builds `TARGET` with
`run`'s build flags:

- `--debug`, `--release` and `--profile`
- `--features`, `--all-features` and `--no-default-features`
- `--commit REF`
- the `--with TARGET` family, which hands the program a second built
  executable in `BROKKR_WITH_EXE` (see `brokkr man run`)

brokkr builds both executables as `brokkr build` does, into snapshots, and
links them into the serve's own generation directory. A later build never
changes what runs, and `brokkr clean` removing `.brokkr/artifacts/` never
takes a served file away. Everything after `--` goes to the program.

brokkr then starts the program detached, under a brokkr supervisor in its own
session. The program's working directory is the build root, as for `run`;
stdin is closed, and stdout and stderr go to the generation's log. brokkr
prints the server's PID and the log's path, releases the lock and returns.
The serve lives until it is stopped or exits.

A serve's identity is its project's state root plus its name. The state root
is where `brokkr.toml` lives, or the working directory without one. The name
is the target's name unless `--name NAME` gives another, so you can serve one
target twice, say two datasets on two ports.

Starting an identity that is already served replaces it. brokkr stops the
live one first, through the same path as `--stop`, and a stop that can't
finish refuses the start. Serves of different identities run side by side; a
port they both want is the servers' clash, which the second one reports in
its log.

Without `--ready`, the start returns once the supervisor reports the server
spawned. `--ready TEXT` waits, still under the lock, until the log contains
TEXT, for up to `--ready-timeout` seconds (default 60). Use
`--ready 'Listening on'`, for example, for a server that says so once its
socket is bound.

If the server exits first or the wait runs out, the start fails, prints the
end of the log and stops what it started. A server that buffers its output
may print its ready line late or never; that is the server's to fix.

A stop while the start still holds the lock takes the starting server down.
That covers `brokkr kill` in the serve's project and Ctrl-C. `brokkr kill
--hard` takes it down too, since the supervisor is still a descendant of the
command.

A served program, one whose environment has `BROKKR_SERVE`, can't start
another serve: that serve's work would leave the serve that owns it.

## Bench-lock

A live serve holds no lock, so other commands run. The rule is the
bench-lock: every measurement refuses while any serve is outstanding, because
a serving process perturbs what is measured. A measurement is any of:

- a measured command with `--bench`, `--hotpath` or `--alloc`, or one whose
  default mode measures (sluggrs `hotpath`), unless `--dry-run`
- `brokkr bench NAME` (not `--dry-run` or `--test`), refused before its
  preflight launches anything
- `brokkr crit TARGET` sampling into a baseline (`--compare` and
  `--baselines` sample nothing)
- ratatoskr `sync --bench`

brokkr checks for serves where the measurement starts, under its hold. No
serve can start inside that hold, so the answer stays true for the whole
measurement. A measured command that builds first is refused after its build.
The refusal names the live serves and how to stop them.

Every other command that takes the lock (`check`, `test`, `run`, `build`)
proceeds, with one warning naming the serves.

An application's own exclusions are its own: a writer of the store a server
serves, such as an ingest into it, takes the application's lock, not
brokkr's.

## Listing and stopping

Bare `brokkr serve` lists every outstanding serve of this user, in every
project. Each line shows:

- the project and name
- a short id
- whether it is serving, and the server's PID
- the target and its age
- the state root and the log

`brokkr lock` prints the same lines after the lock holder's. Neither command
takes the lock.

`brokkr serve --stop` stops the current project's one live serve. It refuses,
listing them, when the project has none or several. `--stop WHICH` stops the
serve named WHICH in the current project, or the one in any project whose
listed id starts with WHICH (four characters at least). A WHICH matching two
serves refuses.

A stop takes no lock. It resolves WHICH once, to one generation, and acts on
that generation only, so a stop racing a replacement never stops the
replacement.

A stop sends the supervisor SIGTERM through a verified pidfd. The supervisor
then does this:

1. Sends the server's process group SIGTERM.
2. Waits up to the serve's grace for the server to exit (`--grace SECS` at
   the start, default 5).
3. Sends the group SIGKILL, at once if the server exited sooner, and reaps
   it.
4. SIGKILLs any process still carrying the serve's token, and exits.

brokkr SIGKILLs a supervisor that hasn't finished within the grace plus a
margin, which takes the server with it. A supervisor ignores a repeated
SIGTERM, so two stops never turn a server's graceful shutdown into a kill.

`brokkr kill` doesn't stop a serve. Once its start has returned, a serve
isn't the lock holder, and kill cancels commands. Kill lists its scope's live
serves as left running, each with the `brokkr serve --stop TOKEN` that stops
it (`brokkr man hygiene`).

## Guarantees

While a serve's supervisor runs, every measurement refuses, strictly. The
supervisor gives up the serve's flock only after three things:

- its server has been reaped
- the server's process group has been SIGKILLed
- it has swept the processes still carrying the serve's token (SIGKILL, then
  a rescan)

What that sweep couldn't see or finish is left to the cleanup below.

Once the supervisor is gone, because it exited or something SIGKILLed it,
the serve stays outstanding and measurements keep refusing until a scan finds
none of its processes left. Every command that reads the registry SIGKILLs
what it finds and retires the serve once nothing is left: a measurement's
check, a listing, a start, a stop and `brokkr clean`.

That cleanup is best effort. It rests on a contract the served program keeps:
every process of a served program keeps `BROKKR_SERVE` in its environment,
stays readable in `/proc` (not made undumpable) and stays in the PID
namespace the serve started in. A process that breaks the contract escapes
the cleanup, as can one forked in the instant a scan passes its parent.

brokkr never judges some serves gone. A serve recorded in another PID
namespace than the reader's, or whose recorded scope this reader can't
compare with its own, blocks measurement until you stop it from where it
started. A start refuses where its own boot id or namespaces can't be read,
since such a serve could never be judged. A serve from an earlier boot is
retired on sight; brokkr takes `~/.brokkr` to belong to one host.

## Files

A serve keeps its state in two places:

- `~/.brokkr/serves/`: the registry, one generation per token.
  `TOKEN.lock` holds the serve's record and the supervisor's flock;
  `TOKEN.state` holds what the supervisor published about itself and the
  server.
- `.brokkr/serve/NAME.TOKEN/` under the state root: the generation
  directory, holding the served executables' links (`program/`, `with/`)
  and `log`. When a serve ends, its links go and its log stays, so you can
  read a crash. The next start of the same name removes the ended
  generations of that name, and `brokkr clean` removes every ended one.

## Exit statuses

A start, a list and a stop exit 0 on success and 1 on failure. A start
interrupted by a stop request takes down what it started and exits 130. A
stop runs to its end, which the serve's grace plus a margin bounds. The
served program's own exit status is in its log, on the line the supervisor
appends when it ends.
