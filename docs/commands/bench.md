# brokkr bench

Run a script bench, a `[bench.NAME]` table in brokkr.toml (`brokkr man
bench-config`), and store the run in the per-user bench store, which `brokkr
results` reads (`brokkr man results`, Script benches): the local file, or
the results server your user-wide config names (`results_server`, `brokkr
man config`). To check a script without running it (its syntax and that every
global it reads is defined), run `brokkr check` (`brokkr man bench-config
validating-a-script`).

```
brokkr bench                          # the benches this project declares
brokkr bench NAME                     # run one, three iterations
brokkr bench --iterations 10 NAME
brokkr bench --commit v0.5.0 NAME     # build and run that commit
brokkr bench --hotpath NAME           # profile: its marked calls' reports
brokkr bench --alloc NAME             # profile allocations the same way
brokkr bench NAME --help              # its annotations, build settings and params
brokkr bench NAME --dataset denmark   # bind a param
brokkr bench NAME --osc 4705..4710    # a collection: one run over all six
brokkr bench NAME --dataset all       # fan out: one run per dataset
brokkr bench --dry-run NAME --row x   # check the invocation, build nothing
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
- `--hotpath` - profile: build with the bench's `hotpath_features` (`brokkr
  man bench-config`) added, and require every call marked `hotpath`
  (`binary_with()`, below) to capture its report. Refused, before anything
  runs, for a bench that sets no `hotpath_features`. A profiled run pairs
  only with profiled runs (What a run stores).
- `--alloc` - profile allocations: `--hotpath` with the bench's
  `alloc_features` added in place of its `hotpath_features`, the calls
  marked and captured the same way. Refused for a bench that sets no
  `alloc_features`, and beside `--hotpath`. An alloc run pairs only with
  alloc runs.
- `--dry-run` - check the invocation and stop before anything is built
  (Dry run).

After NAME come the bench's params, `--NAME VALUE` or `--NAME=VALUE`, and
`--help` or `-h` where a param's name could stand. How they bind against
the resolved config, and what is refused, is `brokkr man bench-config`
(Binding params), including lists, ranges and `all`. Bare `brokkr bench`
lists each bench with its version and the first non-blank line of its
description, when it has them (`brokkr man bench-config`, Annotations).
`NAME --help` shows the bench's annotations under its name, multi-line
text indented under its field, then its build settings, and lists
each param with its `from` path, its default or `(required)` and, for a
`from` param, the keys it takes on this host, in key order, the parents it
depends on chosen
from the same command line, else their defaults; a parent given several
values lists the keys under each path they lead to.

Refused before anything runs, and not stored, in this order: a param
refusal in any arm (a `required` param not given among them, a value not
among its `choices`), a bench whose `validated` params have no `validate()`
to check them, a `validate()` that refuses any arm (Validate), a
bound input another reservation writes or whose pin does not match (every
arm's inputs are reserved for reading, then hashed, after the script is
read and the store opened, before any build), a `--commit` that names
no commit, and a preflight that refuses any arm (Preflight); from there on
every attempt is stored, a worktree that could not be set up included.

## Validate

A script that assigns the global `validate` anywhere (`function
validate(params, keys) ... end`) declares a validate: whether these params
are ones the bench can run, judged by the script itself. It is where a
param declared `validated = true` (`brokkr man bench-config`, Params) is
checked, and where a rule between params goes (`--osc` only for one row,
say). It runs for every arm as soon as the params bind, before anything
else the invocation does, on every run and under `--dry-run`: a fresh Lua
state, the chunk's top level, then `validate(params, keys)`. Returning
nothing passes; `fail(reason)` refuses the invocation whole, every refusing
arm named, before anything is opened, reserved, preflighted, built or
stored. So a typo in a queued invocation is refused at once, not hours in
as a failed run:

```lua
local ROWS = { inspect = true, sort = true }
function validate(params)
    if not ROWS[params.row] then fail("unknown --row " .. params.row) end
end
```

- It is pure: it may not launch a child, make a handle, read a file or call
  `since()`. `config()`, `json`, `fail()`, `require` and `print` work.
  `params` and `keys` are as `preflight()` gets them, every file entry's
  field its path string.
- Its declaration, return and refusal rules are preflight's (below), each
  refusal worded `validate refused` or `validate script error`.
- A module it asks `require` for is stored with the run, as a preflight's
  is.

## Preflight

A script that assigns the global `preflight` anywhere (`function
preflight(params, keys) ... end`, the way it assigns `bench`) declares a
preflight: what the host must provide before the bench can run, checked by
the script itself. It is a check, not a preparation: what a measured child
needs of the page cache, a file or the host's state is set up inside
`bench()`, just before that child, since a preflight runs before the build
and before every other arm, and nothing it leaves is still true when the
child starts. Whether the params make sense is `validate()`'s (Validate). It runs for every arm, before anything is built, after
the inputs are reserved and the commit resolved, before the inputs are
hashed: a fresh Lua state, the chunk's top level, then `preflight(params,
keys)`. Returning nothing passes. Anything else refuses the invocation
whole: no arm is built, nothing is stored, and every refusing arm is named
with its reason. A stop during a preflight stops its child, and the
invocation ends as an interrupt. `--dry-run` does not run it. So a bench
that needs io_uring says so in Lua, with its own fix-it text:

```lua
function preflight(params)
    if params.io ~= "uring" then return end
    local r = binary_with({program = "sh"}, "-c", "ulimit -l"):ok()
    local kb = r.stdout:match("%S+") or "unknown"
    local have = tonumber(kb)
    if kb ~= "unlimited" and (have == nil or have < 16384) then
        fail("io_uring needs RLIMIT_MEMLOCK of 16 MB or more, have " .. kb .. " KB\n"
            .. "Fix: sudo prlimit --pid=$$ --memlock=unlimited:unlimited")
    end
end
```

- Its children are external programs only: `binary_with({program = ...})`
  (`binary_with()`), since the build does not exist yet; `binary()` and a
  call without `program` are refused, and so are `hotpath`, `stop_at` and
  every handle. A program that cannot be found or executed is a result,
  not a failure: `r.start_error` says why, with no `exit`, `signal` or
  output, and `r:ok()` fails naming it. Each child runs on a launch lease
  of its own that reserves nothing and is inherited by the child, so the
  call finishes only once nothing it started still holds it: a child that
  leaves a descendant running fails the preflight once the lease's release
  window after its exit passes.
  Nothing a preflight launches is stored.
- `params` is the bound structure `bench()` gets, every file entry's field
  the absolute path string, not a handle: nothing a preflight does with it
  is declared. `keys` is the same. `since()`, `config()`, `json`, `fail()`
  and `print` work as in `bench()`; `output()`, `scratch()`, `read()`,
  `write()` and `filesize()` do not.
- The declaration is the source, not the run: a `local function
  preflight` or one made only through `_G` declares nothing, and a script
  that assigns `preflight` but whose top level leaves no function there
  (inside an `if`, say) is refused. Define it unconditionally and branch
  inside it. Returning a value is refused too.
- Passing means every arm passed before the build began, not that what it
  checked still holds when the arm runs. Without `--commit`, HEAD is read
  again once every preflight has run and the inputs are hashed, before the
  first arm, as before each later one (Arms).
- A refusal says which kind it is: `preflight refused` for `fail()` and a
  failed `r:ok()`,
  `preflight script error` for a Lua error, a builtin the script may not
  call or called wrongly, a preflight that is no function or returned a
  value (`return nil` included) or a NUL byte in an argument, and
  `preflight could not run` when brokkr's own machinery failed, a lease it
  could not release (a child that left a descendant holding it) among it;
  that one ends the preflights at the arm it happened in, since every later
  arm would meet the same machinery. brokkr runs an external
  program by calling `execve` itself, never through libc's `/bin/sh`
  fallback for a file in no executable format, in `bench()` too, and tells
  the exec's own refusal (no such file, no permission, no format the kernel
  runs, a `#!` interpreter that is missing) from a failure of its own setup
  before it: only the first is a `start_error`. An exec that fails for want
  of resources (memory, argument space, descriptors) is brokkr's too. `r.start_error` is
  the message, `r.start_operation` is `resolve` or `execute`, and
  `r.start_errno` the OS error number when there was one.

## Modules

What fifty benches share lives once, in the project's `bench_lib`
directory (`brokkr man bench-config`, `bench_lib`), and a script loads it
with `require`:

```lua
-- bench/lib/preflight.lua
local M = {}
function M.io_uring(params)
    if params.io ~= "uring" then return end
    local r = binary_with({program = "sh"}, "-c", "ulimit -l"):ok()
    -- ... fail() with the fix-it text
end
return M
```

```lua
-- any bench
local checks = require("preflight")
function preflight(params) checks.io_uring(params) end
function bench(params) ... end
```

Name the local something other than `preflight` or `bench`: with a local of
that name in scope, `function preflight` assigns the local, so the script
declares no preflight.

- `require(name)` takes the module's path under `bench_lib` without `.lua`,
  its directories joined by dots, every segment letters, digits and `_`
  (`io/uring.lua` is `io.uring`). A file whose path makes no such name, and
  a symbolic link, is not a module.
- The whole directory is read once when the run starts, every module
  compiled then, so an edit during a run changes nothing, as for the script;
  a module that does not compile fails only the `require` that names it.
- A module runs on its first `require` in a State, in the State the script
  runs in, its chunk at top level whatever phase asked for it, so loading
  it launches nothing and makes no handle even inside `bench()` (The
  script's builtins); a function it exports runs in the phase that calls
  it. Its export is cached for every later `require` there. Each validate,
  each preflight and each iteration is a fresh State, so each loads its
  modules afresh.
- A module returns exactly one non-nil value, its export: anything else is
  refused. A module requiring itself, directly or through others, is
  refused naming the chain. A module's own error comes back as it is, at
  the module's own file and line.
- A module defines no global (`brokkr check` holds it to that, `brokkr man
  bench-config`, Validating a script); what it shares is what it returns.
  Mutating a shared table (`record`, `string`) is still possible and is the
  module's own business.
- Every module a run's validate, preflight or an iteration asked for, loaded or not,
  is stored with the run, with its file and exact source (What a run
  stores), so `brokkr results --compare` names a changed helper as
  `modules.<name>.source`. A module of the library never required is not.

## Dry run

`brokkr bench --dry-run NAME PARAMS` checks an invocation the way a run
would, up to the build, and stops: every arm bound (Binding params,
`brokkr man bench-config`) and validated (Validate), the commit resolved,
every bound input's shape checked and every pinned one hashed against its
pin, then one line naming what each arm would build and one per arm naming
its params, and `ok`. It opens no store, reserves no input, runs no
preflight (which may launch programs, so it is the run's to make, and a
`preflight not run` line says one was skipped), builds and stores nothing,
and takes no lock, so a queue of invocations can be checked while a bench
runs. A dry run that passes says the invocation is one the bench takes;
the host, the build and the inputs as they are when it runs are the run's
to find out.

## Arms

A param that is not a collection, given several values, fans out: each
value, and each combination of several fanned params, is an **arm**, run as
a run of its own, one after another in the order Binding params gives, each
with its own uuid, worktree setup, build (after the first, cargo finds
nothing to do and the snapshot already exists) and iterations, and each
stored on its own. All arms share the script read once, the store opened
once, the inputs reserved and hashed once, and the commit resolved once.
With several arms, each starts with an `arm I/N` line naming the params that
vary, and its result and failure lines carry them (`bench cat
(dataset=denmark)`).

An arm that fails, at its worktree, its build, the check after its build
(The build) or an iteration, does not stop the rest; its cause is printed
when it fails, and once every arm has run the command fails naming the
failed arms. These end the invocation after the arm in flight is stored,
the arms after it not run and not stored, and named: a stop (Ctrl-C or
`brokkr kill`), a run that cannot be stored, and, without `--commit`, the
checkout's HEAD having moved off the commit resolved for the invocation,
since the arms left would build the live tree under a sha they no longer
are. HEAD moving during an arm's own build fails that arm (The build);
without `--commit` the invocation then ends the same way, unless the live
HEAD is back at that commit by the next arm. HEAD moving
while an arm iterates changes nothing it measured, since its children run
the snapshot already built. A stop that lands while the last arm is being
stored ends nothing: every arm ran and was stored.

## The build

The bench's `target` (else `[bin] default`, else the workspace's only
runnable) is resolved by cargo metadata in the tree being built, under
`--commit` the worktree, so a target that moved between crates is found by
name on both sides. The profile is the bench's `profile`, else `[bin]
profile` / `[bin] debug`, else release. Features are the host section's `features`
(`brokkr man config`) followed by the bench's, and under `--hotpath` its
`hotpath_features`, under `--alloc` its `alloc_features`; `default_features = false`
drops the package's defaults. A timing run (no `--hotpath` or `--alloc`)
fails its arm when cargo resolved any of the bench's `hotpath_features` or
`alloc_features`, as a default or through another feature does: that build
is instrumented. A `--hotpath` run fails the same way on an
`alloc_features` entry its `hotpath_features` does not share. The
built executable is snapshotted into
`.brokkr/artifacts/<xxh128>/<name>` as `brokkr build` does, and every child
runs that snapshot. The run records what was asked for, the bin and the
features cargo resolved for it, and the snapshot's digest. A `program` call
(`binary_with()`) runs another executable in its place.

Once the build and its snapshot have succeeded, and unless a stop is
already pending (the arm is then interrupted with no iteration), the tree
built (under `--commit` the worktree) is checked again: if its HEAD is no
longer the commit resolved for the invocation, the build may have read
either tree, so the arm fails there, stored with what it built and no
iteration. Otherwise, without `--commit`, the live tree's state is read
again, and the run is dirty if the tree was dirty before the build or is
now (What a run stores). An arm whose build or snapshot fails gets neither
second reading. The check compares two observations, so a change present
at neither (a checkout moved and moved back, an edit undone before the
check) is not seen.

## An iteration

Each iteration is a fresh Lua state, so no global carries from one to the
next, running the one copy of the script read when the run began (an edit
during a run changes nothing). The chunk's top level runs first, untimed;
then `bench(params, keys)` is called, and the iteration's wall runs from
that call to its return. `params.NAME` is each bound param's entry: the
value at its chosen key in the resolved config (a table, string or number,
a fresh copy each iteration), with a bound file entry's `file` (or a plain
dataset's `path`) replaced by an input handle; a plain param's string; a
`many` param's list of entries, in bound order. `keys.NAME` is the chosen
key (a list for a `many` param), since an entry has no room for its own. A
fanned param holds the arm's one value. An unbound param is
nil in both; a `required` one never is. TOML integers beyond 2^53 arrive as their decimal text,
datetimes as their TOML text. The whole block is
measured: brokkr's own work for each `binary()` call (admitting it,
preparing its spool and marker FIFO, releasing it) is inside the wall, and
each child's own wall and start offset are recorded beside it.

`bench()` returning nothing completes the iteration with a nil verdict, a
string completes it with that verdict, anything else fails it. Then `record`
is read: a table with string keys, below it tables (string keys, or a list
1..n), strings, finite numbers, booleans and `json.null`, nested at most 64
deep; anything else, a handle included, fails the iteration (JSON, below). `record` starts empty every iteration.

The run stops at the first iteration that does not complete. All completed
with one verdict, the run completed and its result is the first fastest
iteration; iterations that disagree on the verdict fail the run.

## The script's builtins

Scripts are dellingr Lua (`brokkr man bench-config`): no `io`, `os` or
`load`, so every child goes through brokkr, and dellingr's own `require` is
absent too; brokkr's loads `bench_lib` modules alone (Modules).

A chunk's top level, the script's and every module's, is declarations and
computation: no builtin there launches a child, makes or reads a handle, or
asks git (`since()`). What it may call is `require`, `config()`, `json`,
`fail()` and `print`, the same in every phase, so loading a script or a
module does the same thing whichever phase it is loaded for.

- `require(name)` - the export of the `bench_lib` module `name` (Modules).
  Allowed at top level too, and anywhere after it.

- `binary(...)` - run the built executable once and wait for it. Only inside
  `bench()`. `binary_with`'s `program` runs another executable instead.
- `binary_with(options, ...)` - `binary()` with call options (below). Only
  inside `bench()`, and inside `preflight()` with `program` (Preflight).
- `output(name)` - a handle on a fresh path under the iteration's `out/`,
  not created. Only inside `bench()`.
- `scratch(name)` - a handle on a fresh empty directory under the
  iteration's `scratch/`. Only inside `bench()`.
- `read(h)`, `write(h, data)`, `filesize(h)` - brokkr's own file access
  through a handle (Files, below). Only inside `bench()`.
- `json.encode(v)`, `json.decode(s)`, `json.null` - JSON (below). Allowed at
  top level too.
- `since(sha)` - whether the commit under test (the `--commit` one, else the
  checkout's HEAD) is `sha` or descends from it. A sha git cannot resolve
  fails the iteration. Inside `bench()` and `preflight()` only, never at
  top level or in `validate()`. Each sha is asked of git once per run, so
  inside `bench()` the git call falls in the first iteration's wall only.
- `config()` - a fresh copy of the resolved config (`brokkr man
  bench-config`, The resolved view): this host's datasets, `data`, `host`
  and every other table as composed, file paths absolute. Allowed at top
  level too. Its file entries hold plain path strings, not handles: a file
  a script picks from it is undeclared and unpinned, so bind it with a
  param to have it reserved, digested and paired.
- `fail(reason)` - fail the iteration with `reason`.
- `record` - the table stored with the iteration (above).
- `print(...)` - a `[bench]` line on brokkr's console.

A handle is a table; its `path` field is the path as a string. Passing the
handle itself to `binary()` reserves what it names for that call; passing
the string does not. An input handle (a bound file) is read for the call;
an output or scratch handle written; a result's `stdout_file` read. What
makes a table a handle is a metatable brokkr made and protected
(`getmetatable` answers `false`, `setmetatable` refuses), never a key, so
no table of config data or decoded JSON is ever taken for one. `record` and
`json.encode` refuse a handle: store `h.path`.

Every builtin but `print` meets a pending stop before it looks at its
arguments (Interrupting a run). A builtin's failure ends the iteration with
its reason after the script line that called it (`brokkr.toml:14: binary():
the call was refused: ...`), `fail(reason)` and `r:ok()` included; a Lua
error keeps dellingr's message and traceback. An inline script's lines are
its config file's (`brokkr man bench-config`, Inline line numbers).

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
empty (unless `binary_with` gives one), stdout written in full to a spool in the iteration directory, stderr
captured, and the marker FIFO (`BROKKR_MARKER_FIFO`) and sidecar wired in,
one FIFO per child. The result is a table:

- `exit` - the exit code, or nil when a signal ended it; `signal` - that
  signal.
- `stdout`, `stderr` - the first MiB of each, exact bytes;
  `stdout_truncated`, `stderr_truncated` - whether there was more. A
  truncated preview fails nothing.
- `program` - the executable's file name.
- `stdout_file` - a handle on the spool holding the child's whole stdout:
  pass it to another call (read), as its `stdin`, or to `read()` and
  `filesize()`. Pipelines run eagerly: one call's output reaches the next
  through this file, never a pipe between two running children.
- `r:ok()` - returns `r` when the child exited 0 or was stopped at its
  `stop_at` marker (`binary_with()`), and otherwise fails the iteration
  naming how it ended and its last stderr line.

An exit code is the script's to judge, so a nonzero exit fails nothing by
itself. These end the iteration before the script sees a result: Ctrl-C or
`brokkr kill` (the child's process group is stopped; the iteration is
interrupted), a child that could not start or be waited for, and a child
whose descendants still hold its reservation two seconds after it exited.

## `binary_with()`

`binary_with(options, ...)` is `binary(...)` with a table of options first;
the arguments after it follow `binary()`'s rules exactly, so a child's own
`--env` flag is still `{env = ...}` among them.

```lua
local cfg = output("cfg.json")
write(cfg, json.encode({ input = params.input.file.path }))
local r = binary_with({
    env = { RUST_LOG = "info" },
    reads = { params.input.file },
    stdin = earlier.stdout_file,
}, "run", "--config=" .. cfg.path)
```

- `env` - variables added to the child's environment, `NAME = value`:
  names `[A-Za-z_][A-Za-z0-9_]*`, `BROKKR_*` refused (brokkr's own channel
  to a child, the marker FIFO among it), values strings or finite numbers
  (written as an argv number is), no NUL. They are stored with the child,
  beside `BROKKR_MARKER_FIFO`, redacted as credentials are.
- `reads`, `writes` - lists of handles the call reads or writes, overriding
  what passing a handle gives it and declaring a handle the argv does not
  show (a path inside a file the script wrote, a `--output=PATH`
  spelling). Handles only: a path string declares nothing. A handle in
  both is refused, and so is an input or a `stdout_file` in `writes`: both
  are read-only for the run.
- `stdin` - a file handle (an input, an output or a `stdout_file`) the child
  reads as its stdin, from its start, opened by brokkr as `read()` opens a
  file and declared read for the call (over an output's default write; in
  `writes` too it is refused).
- `hotpath` - a stage name (a param name's grammar: lowercase, a letter
  first, then letters, digits, `_` or `-`) marking the call as an
  instrumented stage whose hotpath report brokkr captures (Hotpath capture,
  below). Any `HOTPATH_*` in `env` beside it is refused.
- `stop_at` - an exact span-end marker name, `FOO_END`, of letters, digits
  and `_` (so never the FIFO's `@` counter or `%` call-tag form): brokkr ends the
  child, its whole process group, once it sees the first marker of that
  name on the child's own marker stream (a call-tagged marker never stops
  it). The lenient `FOO` and the `-FOO` shorthand `--stop` takes are
  refused, since a bare name matches two markers. The result then carries
  `stopped_at`, that name, and no `exit` or `signal`: the marker wins over
  whatever the child did after it, a nonzero exit included, since the
  phase it ends was complete; `r:ok()` passes it, and the child is stored
  with that marker as its ending. A child that never emits the marker ends
  on its own and has no `stopped_at`; the script judges that. The FIFO is
  read on the supervisor's ticks and when the child exits, so the child is
  stopped once the marker is recognized, with no bound on how long after
  it; the marker's stamp in the child's evidence is the precise end of the
  phase (`brokkr sidecar`). Beside `hotpath`, the child is asked to stop
  rather than killed: brokkr sends its process group one SIGTERM and waits
  up to 30 seconds for it to exit before the SIGKILL, so a build that
  cancels on SIGTERM unwinds and writes its report (Hotpath capture).
- `program` - an external program to run in place of the built
  executable, everything else about the call unchanged (its borrow, spool,
  marker FIFO, sidecar and options): a name without `/` is looked up on
  the child's own `PATH` (the call's `env` override, else brokkr's), a
  relative entry or an empty one meaning the project root; a name with `/`
  is a path from the project root. The first regular file this user may
  execute wins, and the child is launched by that absolute path, never
  looked up again and never through a shell. The child stores that path and
  the file's digest as read at launch (or why it could not be read), so
  `brokkr results --compare` names a `lua5.4` that changed between two
  runs. A program that cannot be found or started fails the iteration
  naming it: a measured block whose program is missing is a failed
  measurement; a preflight checks presence beforehand (Preflight).
  Comparing against a reference implementation is the case:
  `binary_with({program = "lua5.4"}, script)` beside `binary(script)`.

Any other key is refused, so a misspelt `writes` never runs a call with
less declared than meant.

## Hotpath capture

A call marked `hotpath = STAGE` gets `HOTPATH_METRICS_SERVER_OFF=true`,
`HOTPATH_OUTPUT_FORMAT=json` and `HOTPATH_OUTPUT_PATH`, a report file brokkr
makes empty under the iteration's private `profile/` area and declares
written by the call, in every run, `--hotpath` or not: an instrumented
build then never starts hotpath's metrics server or prints its table. The
script never holds the report's path as a handle.

Once the call has finished (its child reaped and nothing it started still
holding its reservation), whatever its exit, brokkr judges the report and
stores what it found on the child: `captured` with the report, `missing`
(still empty: nothing wrote one, as an uninstrumented build does not),
`invalid` (not a JSON object, or not one `json.decode` takes), `unreadable`
(it could not be read, or something the child started still held the call),
or `over_budget` (past 16 MiB as read, or past what is left of the run's
128 MiB of reports, charged as the store serializes each; it is not kept
and spends nothing). The budget bounds the reports alone: records, evidence
and a report a script copies into `record` count besides, and nothing
bounds a whole run against the 256 MiB a results server takes in one
publication. The result carries
`hotpath_status`, the report as Lua values in `hotpath` when captured, and
`hotpath_error` for invalid and unreadable. A capture is never a builtin
failure: the script still sees the child's exit and output.

Each capture is identified by its stage and its occurrence, the stage's
marked calls so far in the iteration counted from 1 whatever became of
them; `brokkr results` shows and compares captures by both, so a call one
side makes only conditionally shifts the later occurrences of its stage.

A marked call with `stop_at` is stopped by one SIGTERM when its marker
arrives, then SIGKILLed if it has not exited within 30 seconds. A program
that treats SIGTERM as cancellation and drops its hotpath guard on the way
out (pbfhogg's CLI does) writes a report covering its run up to the marker
plus the unwinding; one killed by the SIGTERM, or still running at the
SIGKILL, writes none and its capture is `missing`.

Under `--hotpath` or `--alloc` an iteration that completed fails if any marked call did
not capture, naming each, or if no call was marked; its children,
captures and record are stored as they came. Without either nothing is required:
a capture is evidence only, and a run that captured anything pairs as
incidental, never with clean timing runs (What a run stores).

One report producer in the whole process tree a marked call launches: an
instrumented program that runs another instrumented program hands it the
same `HOTPATH_OUTPUT_PATH`, and the file holds whichever wrote last. brokkr
cannot tell which process wrote it. Per call, so a pipeline marks only its
instrumented stage.

`record.hotpath` is the script's own: brokkr never fills it. A script that
sets it (to `r.hotpath`, say) gets it rendered beside the record.

## Files

`read(h)` returns the whole file a handle names, `filesize(h)` its size in
bytes, `write(h, data)` replaces an output's file with the string `data`.
Each is admitted as a call reading (writing) the handle would be
(`reference/bench-resources.md`, brokkr's own file access), so a file a child turned into a link out of what the iteration
holds, or onto an input for `write`, is refused, and the file is opened
without following a link.

- `read` and `filesize` take an input, an output or a `stdout_file`; a
  `stdout_file` reads as the bytes its call wrote, and one a later child
  replaced is refused. `read` refuses a file past 16 MiB, the most a
  script's string holds, and `filesize` a size past 2^53.
- `write` takes an output only: an input and a `stdout_file` are read-only
  for the run. It removes whatever the name holds (a directory refused) and
  creates the file afresh, so an output a child hard-linked to another file
  is cut, never written through.
- A scratch handle names a directory, which none of them takes.

## JSON

`json.encode(v)` returns compact JSON text, object keys sorted;
`json.decode(s)` one JSON document as Lua values. The shape is `record`'s:
string keys an object, a list 1..n an array, nothing nested deeper than 64,
finite numbers, UTF-8 strings, no handle or function.

- An array `json.decode` makes, and every list brokkr hands a script
  (`params`, `keys`, `config()`), carries brokkr's array metatable, so an
  empty one encodes, and records, as `[]`; a plain empty table is `{}`.
- `json.null` stands for null inside a table, so `[1,null,3]` decodes to
  three elements; `v == json.null` tests for it. Decoding always uses
  brokkr's own, whatever the script assigns to `json.null`; a `json.null`
  with keys set in it is refused.
- `json.decode` refuses a key given twice in one object. An integer within
  2^53 is a number and one beyond it, up to 64 bits, its decimal text (as
  the resolved view hands TOML integers); a number with a fraction or an
  exponent, or an integer past 64 bits, is a double. Whole numbers within
  2^53 encode as integers.
- `json.encode` refuses output past 16 MiB.

## Interrupting a run

Ctrl-C during a preflight stops its child and ends the invocation as an
interrupt, with nothing stored, since no arm was attempted.
Ctrl-C during a child stops the child and ends the iteration interrupted;
the run is then stored as interrupted and the command exits as an
interrupt. During Lua, at the top level or in `bench()`, it is seen
within a few milliseconds at the VM's next safepoint (a loop's backward
jump or a call), so a script spinning in a loop that calls no builtin
stops too. An iteration a stop
reached is stored as interrupted, whatever it would otherwise have been. A second Ctrl-C while the
first is pending takes the default action and kills brokkr at once: that run
is not stored, and its iteration directory is reclaimed by a later `brokkr
clean` or lease scan.

## Where an iteration's files live

Each iteration owns one directory, `bench-iterations/.brokkr-iteration-<token>`
under the host's resolved `scratch` (by default `data/scratch` under the
project root), holding `out/`, `scratch/`, the children's stdout
spools and the marked calls' hotpath reports, on the resource protocol `reference/bench-resources.md` describes: it
is removed when the iteration ends, and one a crash left behind goes once
nothing that inherited its reservation is still running. Its outputs and
scratch so land on the drive the typed commands write to. A `scratch`
inside the project's `.brokkr/` is refused unless it is `.brokkr` itself,
and a `scratch` inside a directory brokkr installs, prunes or removes by
fixed name (a tool install or cache under the data dir) is a misplacement
the protections do not cover.

## What a run stores

Every attempted run is stored, a failed build or an interrupted iteration
included, one per arm, with its bound params (each key, its `from` path and
a file's digest), what it measured (timing, `--hotpath` or `--alloc`), every child's
argv, ending, `/proc` samples, marker FIFO records and capture losses, a
marked child's hotpath capture, and a `program` child's executable path and
digest; `brokkr sidecar <uuid>` reads one child's
through the sidecar views (`brokkr man measure`, Script-bench children).
Runs pair in classes that never pair with each other: timing,
`--hotpath`, `--alloc`, and incidental, a timing run any of whose children captured a
report, since its build was instrumented by other means. A
run of a dirty tree, dirty as its attempt starts or after its build (The
build), is stored and marked dirty: `brokkr results` lists it with `-dirty`
and never compares it. Dirty is the measurement policy's: a change to the
root `brokkr.toml`, to a config file it includes (one that is not
`Cargo.lock` or a toolchain file) or to a `.md` file does not make a tree
dirty, and a `--commit` worktree counts as clean. The
run's uuid is printed whatever `--verbose` says, as the one handle `brokkr
results <uuid>` finds it by, with where it went. A run that cannot be
stored fails the command naming its uuid; one whose storing may or may not
have landed says so, naming the attempt whose outcome is unknown, and
`brokkr results <uuid>` then shows whether it did.

**Where it goes.** The store is opened before anything is built: the local
`bench.db`, or with `results_server` set, the server is checked to answer
with this brokkr's protocol, so a server that cannot be reached fails the
command before any measurement. Publishing retries what may succeed on a
repeat: locally a few times at once; to a server with a growing pause, a
retry starting only within two minutes of the first attempt and eight
attempts at most, each attempt waiting as long as the server's own deadline
(five minutes and a proxy's allowance) before giving up on it, and a
publication whose outcome is unknown always gets one more attempt past
those limits. A
Ctrl-C that stopped the run still leaves it one publication attempt; a
Ctrl-C during the retries ends them once the attempt in flight returns.
The whole invocation holds the brokkr lock, every arm's publication
included.

The stored config is one TOML document: `[run]` (the live project root and
build root, under `--commit` too), `[script]` (the bench, the file its script came from and
the exact source, as `source`, or `source_hex` when it is not UTF-8),
`[modules.NAME]` (each `bench_lib` module the run asked for, its file as
`origin` and its exact source the same way; absent when none was) and
`[config]` (brokkr.toml as composed, includes folded and the user-wide layer
beneath, every table's keys sorted). `brokkr results --compare` names what
moved in it between two paired runs. The resolved view a script saw is not
stored: it derives from this document and the run's hostname.
