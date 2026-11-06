# brokkr bench

Run a script bench, a `[bench.NAME]` table in brokkr.toml (`brokkr man
bench-config`), and store the run in the per-user bench store, which `brokkr
results` reads (`brokkr man results`, Script benches): the local file, or
the results server your user-wide config names (`results_server`, `brokkr
man config`). To check a script compiles without running it, run `brokkr
check` (`brokkr man bench-config validating-a-script`).

```
brokkr bench                          # the benches this project declares
brokkr bench NAME                     # run one, three iterations
brokkr bench --iterations 10 NAME
brokkr bench --commit v0.5.0 NAME     # build and run that commit
brokkr bench --hotpath NAME           # profile: its marked calls' reports
brokkr bench NAME --help              # its build settings and params
brokkr bench NAME --dataset denmark   # bind a param
brokkr bench NAME --osc 4705..4710    # a collection: one run over all six
brokkr bench NAME --dataset all       # fan out: one run per dataset
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

After NAME come the bench's params, `--NAME VALUE` or `--NAME=VALUE`, and
`--help` or `-h` where a param's name could stand. How they bind against
the resolved config, and what is refused, is `brokkr man bench-config`
(Binding params), including lists, ranges and `all`. `NAME --help` lists
each param with its `from` path, its default or `(required)` and, for a
`from` param, the keys it takes on this host, in key order, the parents it
depends on chosen
from the same command line, else their defaults; a parent given several
values lists the keys under each path they lead to.

Refused before anything runs, and not stored: a param refusal in any arm (a
`required` param not given among them), a
bound input another reservation writes or whose pin does not match (every
arm's inputs are reserved for reading, then hashed, after the script is
read and the store opened, before any build), and a `--commit` that names
no commit; from there on every attempt is stored, a worktree that could not
be set up included.

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
`hotpath_features`; `default_features = false`
drops the package's defaults. A timing run (no `--hotpath`) of a bench that
sets `hotpath_features` fails its arm when cargo resolved any of them, as a
default or through another feature does: that build is instrumented. The
built executable is snapshotted into
`.brokkr/artifacts/<xxh128>/<name>` as `brokkr build` does, and every child
runs that snapshot. The run records what was asked for, the bin and the
features cargo resolved for it, and the snapshot's digest.

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

Scripts are dellingr Lua (`brokkr man bench-config`): no `io`, `os`, `load`
or `require`, so every child goes through brokkr.

- `binary(...)` - run the built executable once and wait for it. Only inside
  `bench()`.
- `binary_with(options, ...)` - `binary()` with call options (below). Only
  inside `bench()`.
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
  fails the iteration. Allowed at top level too, and each sha is asked once
  per run; asked first inside `bench()`, the git call falls in the first
  iteration's wall, so prefer the top level.
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
  phase (`brokkr sidecar`). Refused beside `hotpath`: a child stopped at a
  marker is killed and writes no report.

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

Under `--hotpath` an iteration that completed fails if any marked call did
not capture, naming each, or if no call was marked; its children,
captures and record are stored as they came. Without `--hotpath` nothing is required:
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

Each iteration owns one directory, `.brokkr/bench-iterations/.brokkr-iteration-<token>`
under the project root, holding `out/`, `scratch/`, the children's stdout
spools and the marked calls' hotpath reports, on the resource protocol `reference/bench-resources.md` describes: it
is removed when the iteration ends, and one a crash left behind goes once
nothing that inherited its reservation is still running. It sits on the
project's disk, not the host's scratch disk.

## What a run stores

Every attempted run is stored, a failed build or an interrupted iteration
included, one per arm, with its bound params (each key, its `from` path and
a file's digest), what it measured (timing, or `--hotpath`), every child's
argv, ending, `/proc` samples, marker FIFO records and capture losses, and a
marked child's hotpath capture; `brokkr sidecar <uuid>` reads one child's
through the sidecar views (`brokkr man measure`, Script-bench children).
Runs pair in three classes that never pair with each other: timing,
`--hotpath`, and incidental, a timing run any of whose children captured a
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
the exact source, as `source`, or `source_hex` when it is not UTF-8) and
`[config]` (brokkr.toml as composed, includes folded and the user-wide layer
beneath, every table's keys sorted). `brokkr results --compare` names what
moved in it between two paired runs. The resolved view a script saw is not
stored: it derives from this document and the run's hostname.
