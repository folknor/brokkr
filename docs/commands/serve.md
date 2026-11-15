# brokkr serve

A long-lived dev server - a geocoder answering queries, a tile server, an API
under test - run in the background without holding the global lock, so the
client tools, checks and builds that talk to it or work beside it can run
meanwhile. Available in any Rust project, with or without a `brokkr.toml`.

```
brokkr serve                                   # list the live serves, all projects
brokkr serve TARGET [build flags] [--name NAME] [--ready TEXT [--ready-timeout SECS]]
             [--grace SECS] [-- ARGS...]       # start (or replace) one
brokkr serve --stop [WHICH]                    # stop one
```

`brokkr run` holds the lock for its program's whole life, which is right for a
program that finishes and wrong for one that serves: every brokkr command in
every project would wait for the server to stop, a client that is itself a
`brokkr run` included. `serve` splits the two stages.

## Starting one

`brokkr serve TARGET` takes the lock like `run` and builds `TARGET` with `run`'s
build flags: `--debug`/`--release`/`--profile`, `--features`, `--all-features`,
`--no-default-features`, `--commit REF`, and the `--with TARGET` family, which
hands the program a second built executable in `BROKKR_WITH_EXE` (see `brokkr
man run`). Both are built as `brokkr build` builds them, into snapshots, and
linked into the serve's own generation directory, so a later build never
changes what runs and `brokkr clean` removing `.brokkr/artifacts/` never takes
a served file away. Everything after `--` goes to the program.

Then it starts the program detached, under a brokkr supervisor in its own
session, with the program's working directory the build root (as for `run`),
stdin closed and stdout and stderr on the generation's log. It prints the
server's PID and the log's path and returns, releasing the lock. The serve
lives until it is stopped or exits.

A serve's identity is its project's state root (where `brokkr.toml` lives, or
the working directory without one) plus its name, which is the target's name
unless `--name NAME` gives another, so one target can be served twice - two
datasets on two ports. Starting an identity that is already served replaces
it: the live one is stopped first, through the same path as `--stop`, and a
stop that cannot finish refuses the start. Serves of different identities run
side by side; a port they both want is the servers' clash, reported by the
second one in its log.

Without `--ready`, the start returns once the supervisor reports the server
spawned. `--ready TEXT` waits, still under the lock, until the log contains
TEXT - `--ready 'Listening on'` for a server that says so once its socket is
bound - for up to `--ready-timeout` seconds (60 by default). A server that
exits first, or a wait that runs out, fails the start, prints the end of the
log, and stops what was started. A server that buffers its output may print
its ready line late or never; that is the server's to fix. A stop (`brokkr
kill` in the serve's project, Ctrl-C) while the start still holds the lock takes the starting server
down; `brokkr kill --hard` then does too, since the supervisor is still a
descendant of the command.

A program that is a served program itself - one whose environment carries
`BROKKR_SERVE` - cannot start another serve: its work would leave the serve
that owns it.

## While a serve is live

The serve holds no lock, so the other commands run. The rule is the
bench-lock: every measurement refuses while any serve is outstanding, because a
serving process perturbs what is measured. A measurement is

- a measured command with `--bench`, `--hotpath` or `--alloc`, or one whose
  default mode measures (sluggrs `hotpath`), unless `--dry-run`;
- `brokkr bench NAME` (not `--dry-run` or `--test`), refused before its
  preflight launches anything;
- `brokkr crit TARGET` sampling into a baseline (`--compare` and `--baselines`
  sample nothing);
- ratatoskr `sync --bench`.

The check runs where the measurement starts, under its hold, which no serve can
start inside, so the answer holds for the whole measurement. A measured
command that builds first is refused after its build. The refusal names the
live serves and how to stop them. Every other command that takes the lock -
`check`, `test`, `run`, `build`, a `corpus` run whose wall is incidental -
proceeds, with one warning naming the serves.

An application's own exclusions are its own: a writer of the store a server
serves (an ingest into it) takes the application's lock, not brokkr's.

## Listing and stopping

Bare `brokkr serve` lists every outstanding serve of this user, every project:
its project and name, a short id, whether it is serving and the server's PID,
the target, its age, the state root and the log. `brokkr lock` prints the same
lines after the lock holder's. Neither takes the lock.

`brokkr serve --stop` stops the current project's one live serve, and refuses,
listing them, when it has none or several. `--stop WHICH` stops the serve
named WHICH in the current project, or the one whose listed id starts with
WHICH (four characters at least) in any project; a WHICH matching two serves
refuses. A stop takes no lock: it resolves WHICH once, to one generation, and
acts on that generation only, so a stop racing a replacement never stops the
replacement.

A stop sends the supervisor SIGTERM through a verified pidfd. The supervisor
sends the server's process group SIGTERM, waits up to the serve's grace for
the server to exit (`--grace SECS` at the start, 5 by default), sends the
group SIGKILL - at once if the server exited sooner - reaps it, SIGKILLs any
process still carrying the serve's token, and exits. A supervisor that has not
finished within the grace plus a margin is SIGKILLed, which takes the server
with it. A repeated SIGTERM to a supervisor is ignored, so two stops never turn
a server's graceful shutdown into a kill. `brokkr kill` does not stop a serve:
a serve is not the lock holder once its start has returned, and kill cancels
commands. It lists its scope's live serves as left running, each with the
`brokkr serve --stop TOKEN` that stops it (`brokkr man hygiene`).

## What is guaranteed

While a serve's supervisor runs, every measurement refuses; this is strict.
The supervisor gives up the serve's flock only after its server has been
reaped, the server's process group has been SIGKILLed, and it has swept the
processes still carrying the serve's token (SIGKILL, then a rescan); what that
sweep could not see or finish is left to the next part.

After the supervisor is gone - it exited, or something SIGKILLed it - the
serve stays outstanding, and measurements keep refusing, until a scan finds
none of its processes left. Every command that reads the registry (a
measurement's check, a listing, a start, a stop, `brokkr clean`) SIGKILLs
what it finds and retires the serve once nothing is left. That part is best
effort: it rests on a contract the served program keeps. Every process of a
served program keeps `BROKKR_SERVE` in its environment, stays readable in
`/proc` (not made undumpable) and stays in the PID namespace the serve started
in. A process that breaks the contract escapes the cleanup, as can one forked
in the instant a scan passes its parent. A serve recorded in another PID
namespace than the reader's, or whose recorded scope this reader cannot
compare with its own, is never judged gone: it blocks measurement until it is
stopped from where it started. A start refuses where its own boot id or
namespaces cannot be read, since such a serve could never be judged. A serve
from an earlier boot is retired on sight; `~/.brokkr` is taken to be one
host's.

## Files

- `~/.brokkr/serves/`: the registry, one generation per token. `<token>.lock`
  holds the serve's record and the supervisor's flock; `<token>.state` what
  the supervisor published about itself and the server.
- `.brokkr/serve/<name>.<token>/` under the state root: the generation
  directory, holding the served executables' links (`program/`, `with/`) and
  `log`. When a serve ends, its links go and its log stays, so a crash can be
  read; the next start of the same name removes the ended generations of that
  name, and `brokkr clean` removes every ended one.

## Exit statuses

A start, a list and a stop exit 0 on success and 1 on failure; a start
interrupted by a stop request takes what it started down and exits 130. A
stop runs to its end, which the serve's grace plus a margin bounds. The served program's own exit
status is in its log, on the line the supervisor appends when it ends.
