# brokkr bench

Run a script bench, a `[bench.NAME]` table in brokkr.toml (`brokkr man
bench-config`), and store the run in the per-user bench store, which `brokkr
results` reads (`brokkr man results`, Script benches).

```
brokkr bench                          # the benches this project declares
brokkr bench NAME                     # run one, three iterations
brokkr bench --iterations 10 NAME
brokkr bench --commit v0.5.0 NAME     # build and run that commit
brokkr bench NAME --help              # its build settings and params
```

## The command line

brokkr's own options go before NAME and the bench's params after it; brokkr
reads nothing after NAME, `--help` included, so no option brokkr gains can
collide with a param. brokkr's options:

- `--commit REF` - build REF in a persistent worktree, as `brokkr build
  --commit` does, and run it. The benches are always the working tree's:
  HEAD's scripts serve every commit, and branch on `since()` (below).
- `--iterations N` - how many iterations, default 3. The result is the
  fastest.

Params are not bound yet: a bench that declares any is refused, and so are
tokens after NAME other than a lone `--help`. A `--commit` that names no
commit is refused before anything runs; from there on every attempt is
stored, a worktree that could not be set up included.

## The build

The bench's `target` (else `[bin] default`, else the workspace's only
runnable) is resolved by cargo metadata in the tree being built, under
`--commit` the worktree, so a target that moved between crates is found by
name on both sides. The profile is the bench's `profile`, else `[bin]
profile` / `[bin] debug`, else release. Features are the host section's `features`
(`brokkr man config`) followed by the bench's; `default_features = false`
drops the package's defaults. The built executable is snapshotted into
`.brokkr/artifacts/<xxh128>/<name>` as `brokkr build` does, and every child
runs that snapshot. The run records what was asked for, the bin and the
features cargo resolved for it, and the snapshot's digest.

## An iteration

Each iteration is a fresh Lua state, so no global carries from one to the
next, running the one copy of the script read when the run began (an edit
during a run changes nothing). The chunk's top level runs first, untimed;
then `bench(params)` is called, with `params` an empty table, and the
iteration's wall runs from that call to its return. The whole block is
measured: brokkr's own work for each `binary()` call (admitting it,
preparing its spool and marker FIFO, releasing it) is inside the wall, and
each child's own wall and start offset are recorded beside it.

`bench()` returning nothing completes the iteration with a nil verdict, a
string completes it with that verdict, anything else fails it. Then `record`
is read: a table with string keys, below it tables (string keys, or a list
1..n), strings, finite numbers and booleans, nested at most 64 deep;
anything else fails the iteration. `record` starts empty every iteration.

The run stops at the first iteration that does not complete. All completed
with one verdict, the run completed and its result is the first fastest
iteration; iterations that disagree on the verdict fail the run.

## The script's builtins

Scripts are dellingr Lua (`brokkr man bench-config`): no `io`, `os`, `load`
or `require`, so every child goes through brokkr.

- `binary(...)` - run the built executable once and wait for it. Only inside
  `bench()`.
- `output(name)` - a handle on a fresh path under the iteration's `out/`,
  not created. Only inside `bench()`.
- `scratch(name)` - a handle on a fresh empty directory under the
  iteration's `scratch/`. Only inside `bench()`.
- `since(sha)` - whether the commit under test (the `--commit` one, else the
  checkout's HEAD) is `sha` or descends from it. A sha git cannot resolve
  fails the iteration. Allowed at top level too, and each sha is asked once
  per run; asked first inside `bench()`, the git call falls in the first
  iteration's wall, so prefer the top level.
- `fail(reason)` - fail the iteration with `reason`.
- `record` - the table stored with the iteration (above).
- `print(...)` - a `[bench]` line on brokkr's console.

A handle is a table; its `path` field is the path as a string. Passing the
handle itself to `binary()` reserves what it names for that call; passing
the string does not.

## `binary()`

Arguments become the child's argv in call order: a string or number is one
argument, a handle its path, and a table of flags is emitted at its place,
sorted by key: a one-character key as `-k`, a longer one as `--key`; `true`
is a bare flag, `false` is dropped, a string, number or handle is a separate
following argument (`--jobs 4`, never `--jobs=4`), and a list repeats the
flag once per element.

```lua
local r = binary("cat", input, { jobs = 4, output = output("out.pbf") })
```

The child runs in the live project root, under `--commit` too, with stdin
empty, stdout written in full to a spool in the iteration directory, stderr
captured, and the marker FIFO (`BROKKR_MARKER_FIFO`) and sidecar wired in,
one FIFO per child. The result is a table:

- `exit` - the exit code, or nil when a signal ended it; `signal` - that
  signal.
- `stdout`, `stderr` - the first MiB of each, exact bytes;
  `stdout_truncated`, `stderr_truncated` - whether there was more. A
  truncated preview fails nothing.
- `program` - the executable's file name.
- `r:ok()` - returns `r` when the child exited 0, and otherwise fails the
  iteration naming how it ended and its last stderr line.

An exit code is the script's to judge, so a nonzero exit fails nothing by
itself. These end the iteration before the script sees a result: Ctrl-C or
`brokkr kill` (the child's process group is stopped; the iteration is
interrupted), a child that could not start or be waited for, and a child
whose descendants still hold its reservation two seconds after it exited.

## Interrupting a run

Ctrl-C during a child stops the child and ends the iteration interrupted;
the run is then stored as interrupted and the command exits as an
interrupt. During pure Lua it is seen only at the next call of a builtin
other than `print`, at the end of `bench()` or between iterations: the Lua VM brokkr links cannot yet
be interrupted mid-script, so a script spinning in a loop that calls no
such builtin cannot be stopped by the first Ctrl-C. An iteration a stop
reached is stored as interrupted, whatever it would otherwise have been. A second Ctrl-C while the
first is pending takes the default action and kills brokkr at once: that run
is not stored, and its iteration directory is reclaimed by a later `brokkr
clean` or lease scan.

## Where an iteration's files live

Each iteration owns one directory, `.brokkr/bench-iterations/.brokkr-iteration-<token>`
under the project root, holding `out/`, `scratch/` and the children's stdout
spools, on the resource protocol `reference/bench-resources.md` describes: it
is removed when the iteration ends, and one a crash left behind goes once
nothing that inherited its reservation is still running. It sits on the
project's disk, not the host's scratch disk.

## What a run stores

Every attempted run is stored, a failed build or an interrupted iteration
included, with every child's argv, ending, `/proc` samples, marker FIFO
records and capture losses. A run of a dirty tree is stored and marked
dirty: `brokkr results` lists it with `-dirty` and never compares it. The
run's uuid is printed whatever `--verbose` says, as the one handle `brokkr
results <uuid>` finds it by. A run that cannot be stored fails the command
naming its uuid; one whose storing may or may not have landed says so.

The stored config is one TOML document: `[run]` (the live project root and
build root, under `--commit` too), `[script]` (the bench, the file its script came from and
the exact source, as `source`, or `source_hex` when it is not UTF-8) and
`[config]` (brokkr.toml as composed, includes folded and the user-wide layer
beneath, every table's keys sorted). `brokkr results --compare` names what
moved in it between two paired runs.
