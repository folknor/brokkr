# brokkr bench

`brokkr bench NAME` runs a script bench, a `[bench.NAME]` table in
brokkr.toml (`brokkr man bench-config`), and stores the run in your per-user
bench store. `brokkr results` reads that store (`brokkr man results`, Script
benches): the local file, or the results server your user-wide config names
(`results_server`, `brokkr man config`). pbfhogg's benchmarks use this
command; its answer checking runs through its own `pbfhogg-verify` binary
(`brokkr man run`).

To check a script without running it, its syntax and that every global it
reads is defined, run `brokkr check` (`brokkr man bench-config
validating-a-script`).

This page covers the options and refusals, the script's hooks (Validate,
Preflight), Modules, Dry run and Contract tests, then how a run proceeds
(Arms, Rounds, Build, Iteration), the reference for the script's builtins
(Builtins, binary, binary_with, Hotpath capture, Files, Page cache, JSON),
and what a run leaves behind (Interrupts, Iteration directory, Stored runs).

These are the forms the command takes:

```sh
brokkr bench                         # the benches this project declares
brokkr bench NAME                    # run one, default iteration count
brokkr bench --iterations 10 NAME
brokkr bench --commit v0.5.0 NAME    # build and run that commit
brokkr bench --hotpath NAME          # profile: marked calls' reports
brokkr bench --alloc NAME            # profile allocations the same way
brokkr bench NAME --help             # annotations, build settings, params
brokkr bench NAME --dataset denmark  # bind a param
brokkr bench NAME --osc 4705..4710   # a collection: one run over all six
brokkr bench NAME --dataset all      # fan out: one run per dataset
brokkr bench --rounds 6 --versus tool NAME --tool a,b  # A/B, interleaved
brokkr bench --rounds 6 --commit main --commit HEAD NAME  # two builds
brokkr bench --rounds 6 cat-a,cat-b --dataset denmark  # two benches
brokkr bench --dry-run NAME --row x  # check the invocation, build nothing
brokkr bench --test                  # run the contract tests
```

## Options

brokkr's own options go before NAME and the bench's params after it. brokkr
reads nothing after NAME, `--help` included, so no option brokkr gains can
collide with a param. brokkr's options are:

- **`--commit REF`**: build REF in a persistent worktree, as `brokkr build
  --commit` does, and run it. The benches are always the working tree's:
  HEAD's scripts serve every commit, and branch on `since()` (see
  Builtins). A Git LFS object the server can no longer serve is left as its
  pointer file rather than failing the worktree; brokkr warns about every
  such file and stores it in the run's `build.lfs_pointers`. A root
  `Cargo.lock` the commit doesn't track is seeded from the live tree's
  (`brokkr man run commit`). Given more than once, only with `--rounds`,
  the commits are the competitors (see Rounds).
- **`--iterations N`**: how many iterations, overriding the bench table's
  `iterations` setting (default 3 when absent). Any positive u32 count is
  accepted, including one larger than the bench default. The result is the
  fastest. Under `--rounds` it counts per cell, default 1.
- **`--rounds N`**: an A/B: run the competitors N times as interleaved
  rounds, each cell its own run (see Rounds). At most 10000.
- **`--versus PARAM`**: with `--rounds`, the param whose values are the
  competitors.
- **`--hotpath`**: profile. brokkr builds with the bench's
  `hotpath_features` (`brokkr man bench-config`) added, and requires every
  call marked `hotpath` (see binary_with) to capture its report. brokkr
  refuses it, before anything runs, for a bench that sets no
  `hotpath_features`. A profiled run pairs only with profiled runs (see
  Stored runs).
- **`--alloc`**: profile allocations: `--hotpath` with the bench's
  `alloc_features` added in place of its `hotpath_features`, the calls
  marked and captured the same way. brokkr refuses it for a bench that sets
  no `alloc_features`, and beside `--hotpath`. An alloc run pairs only with
  alloc runs.
- **`--dry-run`**: check the invocation and stop before anything is built
  (see Dry run).
- **`--test`**: run the project's contract tests (see Contract tests)
  instead of a bench. It takes no NAME, params or other option, and no
  lock.

Every run that reaches its build, `--commit` or not, stores where its lock
came from and its digest after the build in `build.lockfile`.

After NAME come the bench's params, `--NAME VALUE` or `--NAME=VALUE`, and
`--help` or `-h` where a param's name could stand. How they bind against
the resolved config, including lists, ranges and `all`, and what is refused,
is in `brokkr man bench-config` (Binding params).

Bare `brokkr bench` lists each bench with its version and the first
non-blank line of its description, when it has them (`brokkr man
bench-config`, Annotations). The listing, `NAME --help` and a refused NAME
or param take no brokkr lock and write no run log, so they answer while a
check or bench runs.

`NAME --help` shows the bench's annotations under its name, with multi-line
text indented under its field, then its build settings. It then lists each
param with its `from` path and its default or `(required)`. For a `from`
param it also lists the keys the param takes on this host, in key order.
The parents it depends on are the values chosen on the same command line,
else their defaults; a parent given several values lists the keys under each
path they lead to. A path with no keys names the host only when it lies
under `datasets` or `host`, the tables that differ by host (`brokkr man
bench-config`, Binding params).

brokkr refuses these before anything runs, and stores nothing, in this
order:

1. a param refusal in any arm: a `required` param not given among them, or
   a value not among its `choices`
2. a bench whose `validated` params have no `validate()` to check them
3. a `validate()` that refuses any arm (see Validate)
4. a bound input another reservation writes, or whose pin doesn't match.
   brokkr reserves every arm's inputs for reading, then hashes them, after
   the script is read and the store opened, before any build.
5. a live `brokkr serve` server, since a serving process perturbs the
   measurement (`brokkr man serve`)
6. a `--commit` that names no commit
7. a preflight that refuses any arm (see Preflight)

From there on brokkr stores every attempt, a worktree that couldn't be set
up included.

## Validate

A script that assigns the global `validate` anywhere (`function
validate(params, keys) ... end`) declares a validate: the script's own
judgement of whether these params are ones the bench can run. A param
declared `validated = true` (`brokkr man bench-config`, Params) is checked
there, and a rule between params belongs there, such as `--osc` only for
one row.

brokkr runs it for every arm as soon as the params bind, before anything
else the invocation does, on every run and under `--dry-run`. It runs in a
fresh Lua state: the chunk's top level, then `validate(params, keys)`.
Returning nothing passes. `fail(reason)` refuses the invocation whole,
naming every refusing arm, before anything is opened, reserved,
preflighted, built or stored. A typo in a queued invocation is then refused
at once, not hours later as a failed run:

```lua
local ROWS = { inspect = true, sort = true }
function validate(params)
    if not ROWS[params.row] then fail("unknown --row " .. params.row) end
end
```

These rules apply:

- It is pure: it may not launch a child, make a handle, read a file or call
  `since()`. `config()`, `json`, `fail()`, `require` and `print` work.
  `params` and `keys` are as `preflight()` gets them, every file entry's
  field its path string.
- Its declaration, return and refusal rules are preflight's (see
  Preflight), each refusal worded `validate refused` or `validate script
  error`.
- A module it asks `require` for is stored with the run, as a preflight's
  is.

## Preflight

A script that assigns the global `preflight` anywhere (`function
preflight(params, keys) ... end`, the way it assigns `bench`) declares a
preflight: the script's own check of what the host must provide before the
bench can run. Whether the params make sense is `validate()`'s job (see
Validate).

A preflight is a check, not a preparation. Whatever a measured child needs
of the page cache, a file or the host's state is set up inside `bench()`,
just before that child (a call's `cold` and `warm`, see Page cache). A
preflight runs before the build and before every other arm, so nothing it
leaves is still true when the child starts. What a preflight checks of the
page cache is `page_cache(path, mode)`'s.

brokkr runs it for every arm, before anything is built, after the inputs
are reserved and the commit resolved, and before the inputs are hashed. It
runs in a fresh Lua state: the chunk's top level, then `preflight(params,
keys)`. Returning nothing passes. Anything else refuses the invocation
whole: brokkr builds no arm, stores nothing, and names every refusing arm
with its reason. A stop during a preflight stops its child, and the
invocation ends as an interrupt. `--dry-run` doesn't run it.

A bench that needs io_uring says so in Lua, with its own fix-it text:

```lua
function preflight(params)
    if params.io ~= "uring" then return end
    local r = binary_with({program = "sh"}, "-c", "ulimit -l"):ok()
    local kb = r.stdout:match("%S+") or "unknown"
    local have = tonumber(kb)
    if kb ~= "unlimited" and (have == nil or have < 16384) then
        fail("io_uring needs RLIMIT_MEMLOCK of 16 MB or more, have "
            .. kb .. " KB\n"
            .. "Fix: sudo prlimit --pid=$$ --memlock=unlimited:unlimited")
    end
end
```

These rules apply to its children:

- Its children are external programs only, `binary_with({program = ...})`
  (see binary_with), since the build doesn't exist yet. brokkr refuses
  `binary()`, a call without `program`, and `hotpath`, `stop_at`,
  `measured`, `cold`, `warm` and every handle.
- `deadline` bounds a probe as it bounds a bench's child. A probe that
  outlives it refuses the invocation naming it, as a failed `r:ok()` does.
- A program that can't be found or executed is a result, not a failure.
  `r.start_error` says why, with no `exit`, `signal` or output, and
  `r:ok()` fails naming it.
- Each child runs on a launch lease of its own that reserves nothing and is
  inherited by the child. The call finishes only once nothing it started
  still holds the lease, so a child that leaves a descendant running fails
  the preflight once the lease's release window after its exit passes.
- Its output is piped rather than spooled. The result has the first MiB of
  stdout and stderr and no `stderr_tail`, `stdout_file` or `stderr_file`,
  so `r:ok()` names the last stderr line of that first MiB.
- brokkr stores nothing a preflight launches.

These rules apply to its arguments and builtins:

- `params` is the bound structure `bench()` gets, every file entry's field
  the absolute path string, not a handle, so nothing a preflight does with
  it is declared. `keys` is the same.
- `since()`, `config()`, `json`, `fail()` and `print` work as in `bench()`.
  `output()`, `scratch()`, `read()`, `write()` and `filesize()` don't, and
  `page_cache()` works here alone (see Page cache).

These rules apply to its declaration:

- The declaration is the source, not the run. A `local function preflight`,
  or one made only through `_G`, declares nothing. brokkr refuses a script
  that assigns `preflight` but whose top level leaves no function there,
  inside an `if` for example. Define it unconditionally and branch inside
  it. Returning a value is refused too.
- Passing means every arm passed before the build began, not that what it
  checked still holds when the arm runs. Without `--commit`, brokkr reads
  HEAD again once every preflight has run and the inputs are hashed, before
  the first arm, as before each later one (see Arms).

A refusal says which kind it is:

- **`preflight refused`**: `fail()`, or a failed `r:ok()`.
- **`preflight script error`**: a Lua error; a builtin the script may not
  call or called wrongly; a preflight that is no function or returned a
  value, `return nil` included; or a NUL byte in an argument.
- **`preflight could not run`**: brokkr's own machinery failed, including a
  lease it couldn't release because a child left a descendant holding it.
  This ends the preflights at the arm it happened in, since every later arm
  would meet the same machinery.

brokkr runs an external program by calling `execve` itself, in `bench()`
too, never through libc's `/bin/sh` fallback for a file in no executable
format. It tells the exec's own refusal from a failure of its own setup
before it, and only the first is a `start_error`. The exec's own refusals
are no such file, no permission, no format the kernel runs, and a missing
`#!` interpreter. An exec that fails for want of resources (memory,
argument space, descriptors) is brokkr's failure too. `r.start_error` is the
message, `r.start_operation` is `resolve` or `execute`, and `r.start_errno`
is the OS error number when there was one.

## Modules

Code that many benches share lives once, in the project's `bench_lib`
directory (`brokkr man bench-config`, `bench_lib`), and a script loads it
with `require`. This module exports a check:

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

A bench then calls it from its own preflight:

```lua
-- any bench
local checks = require("preflight")
function preflight(params) checks.io_uring(params) end
function bench(params) ... end
```

Name the local something other than `preflight` or `bench`. With a local of
that name in scope, `function preflight` assigns the local, so the script
declares no preflight.

These rules apply to modules:

- `require(name)` takes the module's path under `bench_lib` without `.lua`,
  its directories joined by dots, every segment letters, digits and `_`, so
  `io/uring.lua` is `io.uring`. A file whose path makes no such name, and a
  symbolic link, isn't a module.
- brokkr reads the whole directory once when the run starts and compiles
  every module then, so an edit during a run changes nothing, as for the
  script. A module that doesn't compile fails only the `require` that names
  it.
- A module runs on its first `require` in a Lua state, the state the script
  runs in. Its chunk runs at top level whatever phase asked for it, so
  loading it launches nothing and makes no handle even inside `bench()`
  (see Builtins). A function it exports runs in the phase that calls it.
  Its export is cached for every later `require` in that state. Each
  validate, each preflight and each iteration is a fresh state, so each
  loads its modules afresh.
- A module returns exactly one non-nil value, its export; anything else is
  refused. brokkr refuses a module requiring itself, directly or through
  others, naming the chain. A module's own error comes back as it is, at
  the module's own file and line.
- A module defines no global, which `brokkr check` enforces (`brokkr man
  bench-config`, Validating a script). What it shares is what it returns.
  Mutating a shared table (`record`, `string`) is still possible, and is
  the module's own business.
- brokkr stores every module a run's validate, preflight or iteration asked
  for, loaded or not, with the run, with its file and exact source (see
  Stored runs). `brokkr results --compare` then names a changed helper as
  `modules.NAME.source`. A library module never required isn't stored.

## Dry run

`brokkr bench --dry-run NAME PARAMS` checks an invocation the way a run
would, up to the build, and stops. It:

1. binds every arm (`brokkr man bench-config`, Binding params) and
   validates it (see Validate)
2. resolves the commit
3. checks every bound input's shape and hashes every pinned one against its
   pin
4. prints one line naming what each arm would build, and one per arm naming
   its params and, for a bench with `rustflags`, the tokens it resolved
5. prints `ok`

It opens no store, reserves no input, builds and stores nothing, and takes
no lock, so you can check a queue of invocations while a bench runs. It runs
no preflight, which may launch programs and so is the run's to make; a
`preflight not run` line says one was skipped. A dry run that passes says
the bench takes the invocation. Finding out about the host, the build and
the inputs as they are when it runs is the run's job.

## Contract tests

A contract test runs a bench's real script against scripted fixtures and
asserts what it did: the argv it built, which config key it read, that it
honours a stop marker, what record and verdict it leaves, why it fails.
There is no build, no data, no child process and no store. The test files
are Lua: every regular `.lua` file under the directory `bench_tests` names
(`brokkr man bench-config`, `bench_tests`). `brokkr check`'s `bench` phase
runs them all, and so does `brokkr bench --test`, which takes no lock.

This test runs one arm and asserts its outcome, reason and argv:

```lua
test("a relations run reads the relations declaration", function()
  local r = run {
    bench = "check-refs",
    params = {"--dataset", "denmark", "--check-relations", "yes"},
    host = { datasets = { denmark = { pbf = { indexed = {
      file = "denmark.pbf",
      expect = { ["check-refs"] = "violations",
                 ["check-refs-relations"] = "clean" },
    } } } } },
    calls = { { exit = 0, stdout = "violations: 3\n" } },
    files = { [project_root .. "/data/denmark.pbf"] = "" },
  }
  assert(r.outcome == "failed", r.outcome)
  assert(string.find(r.reason, "declares clean"), r.reason)
  assert(r.calls[1].argv[2] == "--check-relations")
end)
```

A test file runs top to bottom in a Lua state of its own. The state holds
dellingr's library, `json`, `print` and `project_root`: the project root's
path, which a dataset's `path` and a `[data]` file resolve against, and
the default host data directory below it. It also holds three functions:

- **`test(name, fn)`**: one case. `fn` runs at once, and a failure inside it
  is that case's; the file goes on to the next. A file that declares no
  case, or fails outside one, is a failure itself, and a case inside a case
  is refused. Every case shares the file's state, so what one changes in a
  shared table stays changed. A stop (Ctrl-C, `brokkr kill`) ends the
  file.
- **`assert(v [, message])`**: returns `v`, or the case fails with
  `message`. dellingr has no `assert` of its own, nor `pcall`.
- **`run(spec)`**: one arm of a bench, through the lifecycle a run takes up
  to the build, and its result as a table.

None of a bench script's own builtins (`binary`, `output`, `config`,
`fail`, `record`, ...) is in a test file's state: a test reaches them only
through `run`.

`run` takes a table of fixtures, every key known and every value its shape,
so a misspelt one fails the case rather than test less:

- **`bench`**: the `[bench.NAME]` to run. Required.
- **`params`**: the words after NAME on a command line (`{"--dataset",
  "denmark"}`). They bind with the real binder against the real resolved
  view (`brokkr man bench-config`, The resolved view) of this project's
  config, every host section replaced by `host`, so the defaults, `from`
  paths and refusals are a run's. They must bind to one arm, since a test
  runs one, and a fan-out fails the case.
- **`program`**: the build's file name, as a `binary()` result names it.
  Needed only for a bench without a `target` whose script calls the build,
  since a test runs no cargo to find it. Beside a `target` it must equal it.
- **`host`**: the selected host's section, a table as the TOML would hold it
  (`datasets`, `data`, ...), held to the schema a config load holds a host
  section to. A number with an integral value within 2^53 is a TOML
  integer, any other a float. Absent, the host has no section. Its name in
  the view is `contract-test`.
- **`calls`**: one response per call `bench()` makes, in order. A response
  has `exit`, `signal` or `stopped_at`, at most one, with `exit = 0` when
  none is given. It also has `stdout`, `stderr`, `outputs` (the contents of
  files the child writes, by the name the script gave `output()`) and
  `hotpath` (the report a marked call captured; a marked call without one
  captured nothing, `missing`).
- **`preflight_calls`**: one response per call `preflight()` makes: `exit`
  or `signal`, `stdout`, `stderr`, or `start_error = {operation = "resolve"
  | "execute", message = ...}` for a program that never started.
- **`since`**: `since(sha)`'s answer for each sha, `true` or `false`.
- **`files`**: each input's contents, by its path as the script sees it.
  Only a bound input may be given; outputs, scratch and spools start empty,
  made by the iteration.
- **`page_cache`**: `page_cache(path, mode)`'s reason for each input that
  can't be prepared; an input absent from it can be.

Every string a fixture holds is UTF-8, as a Lua string handed to brokkr
always is.

The result has these fields:

- **`stage`**: how far it got: `bind`, `validate`, `preflight` or `bench`.
- **`outcome`**: at `bind`, `validate` or `preflight`, `refused` or
  `broken`; at `bench`, `completed` or `failed`. `refused` is a param or
  default the binder refuses, a `validated` param with no `validate()`, a
  `fail()` or a failed `r:ok()`. `broken` is a script error in a hook.
- **`reason`**: why, when it didn't complete.
- **`verdict`**: what `bench()` returned, when it completed and returned
  one.
- **`record`**: `record` as a run would store it, when it completed.
- **`calls`**: every call `bench()` made, each with `argv` (the words, a
  handle as its path), `origins` (per word: `text`, `input`, `output`,
  `scratch` or `spool`), `env`, `reads`, `writes`, `cold`, `warm` (paths),
  `measured`, and `stdin`, `stop_at`, `hotpath`, `program` and `deadline`
  (seconds) when it gave them.
- **`preflight_calls`**: every call `preflight()` made: `program`, `argv`,
  `env`, and `deadline` (seconds) when it gave one.
- **`modules`**: every `bench_lib` module the run asked `require` for.
- **`printed`**: every `print` the script made in `validate()`,
  `preflight()` and `bench()`, one string per call, in order. A run's
  prints go here and never reach the check output; the test file's own
  prints still do.

A refusal or a failure is the script's, and is a result the test asserts. A
fixture that the script's behaviour contradicts is the test's fault, and
fails the case at its `run`:

- a call with no response left
- a declared response no call asked for, since the script skipped what its
  test meant to exercise
- a `stopped_at` the call didn't name as its `stop_at`
- an `outputs` name that is no `output()` name, or that the call doesn't
  declare written
- a `hotpath` report for a call not marked
- a `since()` with no answer
- an input read, sized or given as stdin without contents in `files`
- a `files` path that is no bound input
- an input under `/brokkr-contract/iteration`
- a bench or host the config wouldn't have
- a build call with no `program` to name it by

The virtual hosts decide what a run's hosts decide, the same way. They apply
the same call rules: a second `measured` call, a `cold` or `warm` input the
call doesn't pass, an input in `writes`, a handle never given out, an
argument holding a NUL, a refused preflight option. They apply the same
output and scratch name rules, a `write()` only to an output, and an output
never written doesn't exist, not even as a call's stdin.

A call's result is shaped as a run's. `stdout` and `stderr` are cut at
their first MiB with `stdout_truncated` and `stderr_truncated`,
`stderr_tail` is their last, and `stdout_file` and `stderr_file` are
readable handles. Its `program` is the external program's file name, or the
build's: the bench's `target`, or the spec's `program` for a bench without
one, whose build a run finds by cargo's runnable discovery. Outputs, scratch
and spools live under `/brokkr-contract/iteration`.

A test proves the script's behaviour given what the fixtures say the world
did. A run, and brokkr's own tests against real children, cover what it
doesn't prove:

- that a program exists or starts
- that an input is a file brokkr may read
- that a call would be admitted beside other reservations
- that page-cache preparation would succeed; two paths that are one file
  are caught only by a run
- anything about a real child's markers and timing

## Arms

A param that isn't a collection, given several values, fans out. Each value,
and each combination of several fanned params, is an arm, run as a run of
its own, one after another in the order Binding params gives. Each arm has
its own uuid, worktree setup, build and iterations, and is stored on its
own. After the first arm's build, cargo finds nothing to do and the snapshot
already exists. All arms share the script read once, the store opened once,
the inputs reserved and hashed once, and the commit resolved once.

With several arms, each starts with an `arm I/N` line naming the params that
vary, and its result and failure lines name them (`bench cat
(dataset=denmark)`).

An arm that fails, at its worktree, its build, the check after its build
(see Build) or an iteration, doesn't stop the rest. brokkr prints its cause
when it fails, and once every arm has run the command fails naming the
failed arms.

These end the invocation after the arm in flight is stored, the arms after
it named but not run and not stored:

- a stop, Ctrl-C or `brokkr kill`
- a run that can't be stored
- without `--commit`, the checkout's HEAD having moved off the commit
  resolved for the invocation, since the arms left would build the live tree
  under a sha it no longer is

HEAD moving during an arm's own build fails that arm (see Build). Without
`--commit` the invocation then ends the same way, unless the live HEAD is
back at that commit by the next arm. HEAD moving while an arm iterates
changes nothing it measured, since its children run the snapshot already
built. A stop that arrives while the last arm is being stored ends nothing:
every arm ran and was stored.

## Rounds

`--rounds N` runs an A/B: competitors that differ on exactly one axis,
interleaved. Each cell, a competitor in a workload in a round, is a run of
its own. The axis is one of:

- **A param**: `--versus PARAM`, with PARAM given on the command line with
  two or more values (`--tool pbfhogg,osmium`), each a competitor. PARAM
  must be a scalar, not a collection.
- **Builds**: `--commit` given two or more times, each commit a competitor,
  each built in its worktree. Two refs that resolve to one commit are
  refused. The live tree is never a competitor here: the matched A/B of two
  builds has `--commit` on both arms (`brokkr man measure`), so commit a
  variant worth comparing locally.
- **Benches**: NAME as a comma list (`cat-a,cat-b`). Every bench in the list
  binds the params after it, so a param one bench doesn't take, or a
  `required` one it lacks, refuses the invocation: this form can't give a
  param to some benches only.

brokkr refuses giving none of them or more than one, a bench list, a
repeated `--commit` or `--versus` without `--rounds`, and any of them
without NAME.

Whatever else fans out is the workload. brokkr matches each workload's
competitors by the keys their params bound:

- Under `--versus`, every param but PARAM. A param that depends on PARAM and
  binds different keys under each competitor leaves a competitor without a
  cell in some workload, which is refused.
- Under several `--commit`, every param.
- For a bench list, the params the command line gave, every other bound
  param left to each bench. A bench whose own param fans out under one
  workload is refused, since neither arm is then the match.

Every competitor must have exactly one cell in every workload, else brokkr
refuses the invocation. Matched by key isn't matched by content: two benches
may resolve one key to different files, which the summary names (`brokkr
man results`, Script benches).

Workloads run one after another. Within one, rounds run one after another,
each round running every competitor once back to back: in competitor order
in the first round, reversed in the next, and so on. Over two rounds each
competitor takes the same mean position and each pair of competitors runs
once in each order; with two competitors that is ABBA. Nothing balances the
cell before a round's first or after its last. A cell runs `--iterations`
iterations, default 1, since the round is the repetition. With
`binary_with()`'s `cold` or `warm` on the measured call (see Page cache),
each cell starts from the cache state it asks for.

Before anything is built, brokkr prepares everything once, as for arms:

- every bench's params bound and `validate()` run
- the inputs of every arm reserved and hashed
- the preflight run for each bench and each commit its cells build, with
  `since()` answering for that commit, and its answers kept for the
  commit's cells

Then brokkr makes each distinct build once, in the order the cells first
need them, workload by workload and competitors in order. A build is
distinct by commit, target, profile, features, the bench's barred profiling
features and its resolved `rustflags`. Competitors that differ only in a
param share one build, and a workload param spliced into `rustflags` gives
each workload its own builds: `--commit A --commit B --layout all` makes
two builds per layout.

Without `--commit`, brokkr reads the live HEAD before each build, and it
must still be the commit resolved for the invocation, else brokkr refuses
the rest, not run and not stored. The check after the build (see Build)
applies too. Every cell of a build runs its snapshot, so no cell follows a
build, and HEAD moving once the builds are made changes nothing measured.

A build that fails is stored once, as a run of its own whose schedule marks
it a build (see Stored runs), attributed to the first cell it serves in that
order. Its cells are blocked, never run and never stored, and the rest still
run. A failed cell doesn't stop the rest either. The command fails once
everything ran, naming the failed builds and cells. A stop, or a run that
can't be stored, ends the invocation after the run in flight is stored,
naming how many cells were left. The last line names the invocation's id,
which `brokkr results --invocation ID` summarizes.

`--dry-run` prints the plan and stops as for arms (see Dry run). The plan
shows each competitor, each workload, with its builds when they differ by
workload, and each build with its `rustflags` tokens.

## Build

brokkr resolves the bench's `target` (else `[bin] default`, else the
workspace's only runnable) by cargo metadata in the tree being built, the
worktree under `--commit`. A target that moved between crates is found by
name on both sides. Each arm resolves afresh, so a failed resolution fails
only its arm, and every arm resolves against the tree as it then stands.

The profile is the bench's `profile`, else `[bin] profile` or `[bin]
debug`, else release. The features are the host section's `features`
(`brokkr man config`) followed by the bench's, plus its `hotpath_features`
under `--hotpath` or its `alloc_features` under `--alloc`. `default_features
= false` drops the package's defaults.

A timing run, with no `--hotpath` or `--alloc`, fails its arm when cargo
resolved any of the bench's `hotpath_features` or `alloc_features`, as a
default or through another feature, since that build is instrumented. A
`--hotpath` run fails the same way on an `alloc_features` entry its
`hotpath_features` doesn't share.

brokkr adds a bench's `rustflags` (`brokkr man bench-config`), resolved
under the arm, at the rustflags layer cargo will read for the build root, so
the project's own flags stay. It appends them to a set `RUSTFLAGS` or
`CARGO_ENCODED_RUSTFLAGS`, else adds a `--config` entry at the live target
or build table. When the config chain holds a target table brokkr can't
judge, the build fails rather than risk flags that never reach rustc.

A bench that sets `rustflags` builds with `CARGO_TARGET_DIR` and
`CARGO_BUILD_BUILD_DIR` both at `BASE/rustflags-HASH`. `BASE` is the
worktree's target dir under `--commit` and cargo's target dir otherwise,
and the hash is keyed by the resolved tokens. Each recipe, the empty one
too, keeps its own cache, and none disturbs the plain build. `--hotpath`
and `--alloc` builds take the same recipe.

brokkr snapshots the built executable into `.brokkr/artifacts/XXH128/NAME`
as `brokkr build` does, and every child runs that snapshot. The run records:

- what was asked for
- the bin and the features cargo resolved for it
- the snapshot's digest
- for a bench with `rustflags`, the resolved tokens and how they were
  injected

A `program` call (see binary_with) runs another executable in the
snapshot's place.

Once the build and its snapshot have succeeded, brokkr checks the tree built
again, the worktree under `--commit`, unless a stop is already pending; the
arm is then interrupted with no iteration. If the tree's HEAD is no longer
the commit resolved for the invocation, the build may have read either
tree, so the arm fails there, stored with what it built and no iteration.
Otherwise, without `--commit`, brokkr reads the live tree's state again,
and the run is dirty if the tree was dirty before the build or is now (see
Stored runs).

An arm whose build or snapshot fails gets neither second reading. The check
compares two observations, so it doesn't see a change present at neither,
such as a checkout moved and moved back, or an edit undone before the
check.

## Iteration

Each iteration is a fresh Lua state, so no global survives from one to the
next. It runs the one copy of the script read when the run began, so an
edit during a run changes nothing. The chunk's top level runs first,
untimed. Then brokkr calls `bench(params, keys)`, and the iteration's wall
runs from that call to its return.

`params.NAME` is each bound param's entry:

- for a `from` param, the value at its chosen key in the resolved config, a
  table, string or number, a fresh copy each iteration, with a bound file
  entry's `file`, or a plain dataset's `path`, replaced by an input handle
- for a plain param, its string; an `integer`, `number` or `bbox` param's
  too, as given, once binding has checked it
- for a `many` param, its list of entries, in bound order

`keys.NAME` is the chosen key, a list for a `many` param, since an entry has
no room for its own. A fanned param holds the arm's one value. An unbound
param is nil in both, and a `required` one never is. TOML integers beyond
2^53 arrive as their decimal text, and datetimes as their TOML text.

The whole block is measured. brokkr's own work for each `binary()` call,
admitting it, preparing its spool and marker FIFO, and releasing it, is
inside the wall, and brokkr records each child's own wall and start offset
beside it.

An iteration is ranked on that block wall unless one of its calls is marked
`measured = true` (see binary_with). Its result is then that child's wall,
from launch to reap, so setup children, cache preparation and Lua
bookkeeping fall outside it. Process startup is still inside: a workload
that must leave it out reports its own figure in `record`, which is shown,
never ranked on. At most one call per iteration may be marked.

`bench()` returning nothing completes the iteration with a nil verdict; a
string completes it with that verdict; anything else fails it. Then brokkr
reads `record`, which must be a table with string keys holding, below it,
tables (string keys, or a list 1..n), strings, finite numbers, booleans and
`json.null`, nested at most 64 deep. Anything else, a handle included,
fails the iteration (see JSON). `record` starts empty every iteration.

The run stops at the first iteration that doesn't complete. When all
complete with one verdict, the run completed, and its result is the first
fastest iteration. Iterations that disagree on the verdict fail the run, and
so do iterations of which some marked a `measured` call and others didn't.
A run ranked on a measured call pairs only with runs ranked the same way, so
`brokkr results --compare` never sets a child's wall against a block's.

## Builtins

Scripts are dellingr Lua (`brokkr man bench-config`), with no `io`, `os` or
`load`, so every child goes through brokkr. dellingr's own `require` is
absent too; brokkr's loads `bench_lib` modules alone (see Modules).

A chunk's top level, the script's and every module's, is declarations and
computation. No builtin there launches a child, makes or reads a handle, or
asks git (`since()`). It may call `require`, `config()`, `json`, `fail()` and
`print`, the same in every phase, so loading a script or a module does the
same thing whichever phase it is loaded for.

The builtins are:

- **`require(name)`**: the export of the `bench_lib` module `name` (see
  Modules). Allowed at top level too, and anywhere after it.
- **`binary(...)`**: run the built executable once and wait for it. Only
  inside `bench()`. `binary_with`'s `program` runs another executable
  instead.
- **`binary_with(options, ...)`**: `binary()` with call options (see
  binary_with). Only inside `bench()`, and inside `preflight()` with
  `program` (see Preflight).
- **`output(name)`**: a handle on a fresh path under the iteration's `out/`,
  not created. Only inside `bench()`.
- **`scratch(name)`**: a handle on a fresh empty directory under the
  iteration's `scratch/`. Only inside `bench()`.
- **`read(h)`, `write(h, data)`, `filesize(h)`**: brokkr's own file access
  through a handle (see Files). Only inside `bench()`.
- **`page_cache(path, mode)`**: whether a bound input can be prepared
  `"cold"` or `"warm"` (see Page cache). Only inside `preflight()`.
- **`json.encode(v)`, `json.decode(s)`, `json.null`**: JSON (see JSON).
  Allowed at top level too.
- **`since(sha)`**: whether the commit under test, the `--commit` one else
  the checkout's HEAD, is `sha` or descends from it. A sha git can't
  resolve fails the iteration. Only inside `bench()` and `preflight()`,
  never at top level or in `validate()`. brokkr asks git about each sha
  once per run, so inside `bench()` the git call falls in the first
  iteration's wall only.
- **`config()`**: a fresh copy of the resolved config (`brokkr man
  bench-config`, The resolved view): this host's datasets, `data`, `host`
  and every other table as composed, file paths absolute. Allowed at top
  level too. Its file entries hold plain path strings, not handles. A file a
  script picks from it is undeclared and unpinned, so bind it with a param
  to have it reserved, digested and paired.
- **`fail(reason)`**: fail the iteration with `reason`.
- **`record`**: the table stored with the iteration (see Iteration).
- **`print(...)`**: a `[bench]` line on brokkr's console.

A handle is a table whose `path` field is the path as a string. Passing the
handle itself to `binary()` reserves what it names for that call; passing
the string doesn't. brokkr reserves an input handle, a bound file, for
reading by the call, an output or scratch handle for writing, and a
result's `stdout_file` or `stderr_file` for reading. A table is a handle
because of a metatable brokkr made and protected (`getmetatable` answers
`false`, `setmetatable` refuses), never because of a key, so no table of
config data or decoded JSON is taken for one. `record` and `json.encode`
refuse a handle: store `h.path`.

Every builtin but `print` checks for a pending stop before it looks at its
arguments (see Interrupts). A builtin's failure ends the iteration with its
reason after the script line that called it (`brokkr.toml:14: binary(): the
call was refused: ...`), `fail(reason)` and `r:ok()` included. A Lua error
keeps dellingr's message and traceback. An inline script's lines are its
config file's (`brokkr man bench-config`, Inline line numbers).

## binary

`binary(...)` runs the built executable once. Its arguments become the
child's argv in call order:

- A string or number is one argument, and a handle its path.
- A table of flags is emitted at its place, sorted by key: a one-character
  key as `-k`, a longer one as `--key`. `true` is a bare flag and `false` is
  dropped. A string, number or handle is a separate following argument
  (`--jobs 4`, never `--jobs=4`), and a list repeats the flag once per
  element.

An integral finite number within +/-2^53 is written as exact decimal text,
zero as `0`. Other numbers keep Lua's `%.14g` spelling. The same applies to
numeric environment values. Script-side `tostring` and concatenation keep
Lua's formatting.

```lua
local r = binary("cat", input, { jobs = 4, output = output("out.pbf") })
```

The child runs in the live project root, under `--commit` too. Its stdin is
empty unless `binary_with` gives one. Its stdout and stderr are each written
in full to a spool in the iteration directory. brokkr wires in the marker
FIFO (`BROKKR_MARKER_FIFO`) and the sidecar, one FIFO per child. The result
is a table:

- **`exit`**: the exit code, or nil when a signal ended it.
- **`signal`**: that signal.
- **`stdout`, `stderr`**: the first MiB of each, exact bytes.
- **`stdout_truncated`, `stderr_truncated`**: whether there was more. A
  truncated preview fails nothing.
- **`stderr_tail`**: the last MiB of stderr, exact bytes, or the whole of
  it, as `stderr` is, when there was no more.
- **`program`**: the executable's file name.
- **`stdout_file`, `stderr_file`**: handles on the spools holding the
  child's whole stdout and stderr. Pass one to another call (read), as its
  `stdin`, or to `read()` and `filesize()`. Pipelines run eagerly: one
  call's output reaches the next through this file, never a pipe between
  two running children.
- **`r:ok()`**: returns `r` when the child exited 0 or was stopped at its
  `stop_at` marker (see binary_with). Otherwise it fails the iteration,
  naming how the child ended and its last non-empty stderr line within
  `stderr_tail`, or within `stderr` for a preflight's result (see
  Preflight). The line is cut at 2000 characters and marked `...` when
  longer. It is the last line written, which cleanup after an error may
  follow.

The spools go with the iteration. The store keeps a child's stderr size and
its last 64 KiB (see Stored runs).

An exit code is the script's to judge, so a nonzero exit fails nothing by
itself. These end the iteration before the script sees a result:

- Ctrl-C or `brokkr kill`: brokkr stops the child's process group and the
  iteration is interrupted
- a child that couldn't start or be waited for
- a child that outlived its call's `deadline` (see binary_with)
- a child whose descendants still hold its reservation once the release
  window after its exit has passed

## binary_with

`binary_with(options, ...)` is `binary(...)` with a table of options first.
The arguments after it follow `binary()`'s rules exactly, so a child's own
`--env` flag is still `{env = ...}` among them:

```lua
local cfg = output("cfg.json")
write(cfg, json.encode({ input = params.input.file.path }))
local r = binary_with({
    env = { RUST_LOG = "info" },
    reads = { params.input.file },
    stdin = earlier.stdout_file,
}, "run", "--config=" .. cfg.path)
```

The options are below. Any other key is refused, so a misspelt `writes`
never runs a call with less declared than meant.

**`env`** adds variables to the child's environment, `NAME = value`. Names
match `[A-Za-z_][A-Za-z0-9_]*`, and `BROKKR_*` is refused, since it is
brokkr's own channel to a child, the marker FIFO among it. Values are
strings or finite numbers, written as an argv number is, with no NUL.
brokkr stores them with the child, beside `BROKKR_MARKER_FIFO`, redacted as
credentials are.

**`reads`, `writes`** are lists of handles the call reads or writes. They
override what passing a handle gives it, and declare a handle the argv
doesn't show, such as a path inside a file the script wrote or a
`--output=PATH` spelling. They take handles only, since a path string
declares nothing. A handle in both is refused, and so is an input, a
`stdout_file` or a `stderr_file` in `writes`: all are read-only for the run.

**`stdin`** is a file handle, an input, an output, a `stdout_file` or a
`stderr_file`, that the child reads as its stdin from its start. brokkr
opens it as `read()` opens a file and declares it read for the call, over an
output's default write. In `writes` too, it is refused.

**`hotpath`** is a stage name, in a param name's grammar: lowercase, a
letter first, then letters, digits, `_` or `-`. It marks the call as an
instrumented stage whose hotpath report brokkr captures (see Hotpath
capture). brokkr refuses any `HOTPATH_*` in `env` beside it.

**`stop_at`** is a marker name of letters, digits and `_`, so never the
FIFO's `@` counter or `%` call-tag form. It may be a span end such as
`PASS1_END` or a point marker such as `LOADED`, matched exactly. brokkr ends
the child, its whole process group, once it sees the first marker of that
name on the child's own marker stream; a call-tagged marker never stops it.
Unlike `--stop`, a bare `FOO` never falls back to `FOO_END`, so a name
matches one marker, and the `-FOO` shorthand is refused.

The result then has `stopped_at`, that name, and no `exit` or `signal`. The
marker wins over whatever the child did after it, a nonzero exit included,
since the phase it ends was complete. `r:ok()` passes it, and brokkr stores
the child with that marker as its ending. The marker is a required boundary:
a child that ends any other way, an exit 0 included, fails the iteration
naming the marker and how the child ended, so a renamed marker never turns a
phase run into a full one. brokkr stores that child as it came.

What counts is that the stop fired, not that the marker text appears in the
evidence: a marker a descendant writes after the child exited stops nothing.
A script that wants no stop passes no `stop_at`. Runs pair on params, never
call options, so a stopped run and a full one are different arms only when
the stop is a bound param.

brokkr reads the FIFO on the supervisor's ticks and when the child exits, so
it stops the child once it recognizes the marker, with no bound on how long
after. The marker's stamp in the child's evidence is the precise end of the
phase (`brokkr sidecar`). Beside `hotpath`, brokkr asks the child to stop
rather than killing it. It sends the child's process group one SIGTERM and
waits up to 30 seconds, never past the call's `deadline`, for it to exit
before the SIGKILL. A build that cancels on SIGTERM can then unwind and write
its report (see Hotpath capture). brokkr SIGKILLs the group once the child
exits too, so the report is the child's to write before it exits, and a
helper of its still running is ended.

**`program`** is an external program to run in place of the built
executable, everything else about the call unchanged: its borrow, spool,
marker FIFO, sidecar and options. brokkr looks a name without `/` up on the
child's own `PATH`, the call's `env` override else brokkr's, where a
relative or empty entry means the project root. A name with `/` is a path
from the project root. The first regular file this user may execute wins,
and brokkr launches the child by that absolute path, never looked up again
and never through a shell.

brokkr stores that path and the file's digest as read at launch, or why it
couldn't be read, so `brokkr results --compare` names a `lua5.4` that
changed between two runs. A program that can't be found or started fails the
iteration naming it: a measured block whose program is missing is a failed
measurement, and a preflight checks presence beforehand (see Preflight).
Comparing against a reference implementation is the use case:
`binary_with({program = "lua5.4"}, script)` beside `binary(script)`.

**`measured`** is a boolean; `true` makes this child's wall the iteration's
result (see Iteration). A second marked call in one iteration fails it
before launching, and brokkr stores the child with the mark.

**`cold`, `warm`** are lists of input handles that brokkr advises out of the
page cache, or reads through into it, after admitting the call and before
launching it, then checks and stores (see Page cache).

**`deadline`** is a number of seconds above zero, fractions allowed, that
the child may live, counted from its launch, after admission and any `cold`
or `warm` preparation. brokkr kills a child still running once it passes,
its whole process group at once, and the iteration fails naming the call and
the bound. brokkr stores the child with its deadline as its ending, and
still keeps a hotpath report it wrote before the kill.

The deadline caps the child's whole life, not the time to reach a marker.
Beside `stop_at` and `hotpath`, the stop's SIGTERM grace ends at the
deadline, and a child still running then is overdue, not stopped. A marker
recognized after the deadline passed stops the child only when it had
already exited. brokkr observes the deadline on the supervisor's ticks, so
a child found exited after it keeps its own ending, since it may have exited
before. A descendant that left the group is out of reach, as always, and
one still holding the borrow fails the call as well.

## Hotpath capture

A call marked `hotpath = STAGE` gets three variables, in every run,
`--hotpath` or not:

- `HOTPATH_METRICS_SERVER_OFF=true`
- `HOTPATH_OUTPUT_FORMAT=json`
- `HOTPATH_OUTPUT_PATH`, a report file brokkr makes empty under the
  iteration's private `profile/` area and declares written by the call

An instrumented build then never starts hotpath's metrics server or prints
its table. The script never holds the report's path as a handle.

Once the call has finished, its child reaped and nothing it started still
holding its reservation, brokkr judges the report whatever the exit, and
stores what it found on the child:

- **`captured`**: with the report.
- **`missing`**: still empty, since nothing wrote one, as an
  uninstrumented build doesn't.
- **`invalid`**: not a JSON object, or not one `json.decode` takes.
- **`unreadable`**: it couldn't be read, or something the child started
  still held the call.
- **`over_budget`**: past 16 MiB as read, or past what is left of the run's
  128 MiB of reports, charged as the store serializes each. brokkr doesn't
  keep it, and it spends nothing.

The budget bounds the reports alone. Records, evidence and a report a script
copies into `record` count separately, and nothing bounds a whole run
against the 256 MiB a results server takes in one publication.

The result has `hotpath_status`, the report as Lua values in `hotpath` when
captured, and `hotpath_error` for invalid and unreadable. A capture is never
a builtin failure: the script still sees the child's exit and output.

Each capture is identified by its stage and its occurrence: the stage's
marked calls so far in the iteration, counted from 1 whatever became of
them. `brokkr results` shows and compares captures by both, so a call one
side makes only conditionally shifts the later occurrences of its stage.

When its marker arrives, a marked call with `stop_at` is stopped by one
SIGTERM, then SIGKILLed if it hasn't exited within 30 seconds or by its
call's `deadline`, whichever comes first. A program that treats SIGTERM as
cancellation and drops its hotpath guard on the way out, as pbfhogg's CLI
does, writes a report covering its run up to the marker plus the
unwinding. One killed by the SIGTERM, or still running at the SIGKILL,
writes none, and its capture is `missing`.

Under `--hotpath` or `--alloc`, an iteration that completed fails if any
marked call didn't capture, naming each, or if no call was marked. brokkr
stores its children, captures and record as they came. Without either flag
nothing is required: a capture is evidence only, and a run that captured
anything pairs as incidental, never with clean timing runs (see Stored
runs).

The whole process tree a marked call launches has one report producer. An
instrumented program that runs another instrumented program hands it the
same `HOTPATH_OUTPUT_PATH`, and the file holds whichever wrote last; brokkr
can't tell which process wrote it. Marking is per call, so a pipeline marks
only its instrumented stage.

`record.hotpath` is the script's own, and brokkr never fills it. A script
that sets it, to `r.hotpath` say, gets it rendered beside the record.

## Files

`read(h)` returns the whole file a handle names, `filesize(h)` its size in
bytes, and `write(h, data)` replaces an output's file with the string
`data`. brokkr admits each as it would admit a call reading or writing the
handle. It
refuses a file a child turned into a link out of what the iteration holds,
or onto an input for `write`, and opens the file without following a link.

- `read` and `filesize` take an input, an output, a `stdout_file` or a
  `stderr_file`. A spool's handle reads as the bytes its call wrote, and one
  a later child replaced is refused. `read` refuses a file past 16 MiB, the
  most a script's string holds, and `filesize` a size past 2^53.
- `write` takes an output only, since an input and a spool's handle are
  read-only for the run. It removes whatever the name holds, refusing a
  directory, and creates the file afresh, so it cuts an output a child
  hard-linked to another file rather than writing through it.
- A scratch handle names a directory, which none of them takes.

## Page cache

A call prepares its inputs' page-cache state itself, with no `dd` or
`fincore` from the child's `PATH`:

```lua
binary_with({ cold = { params.input.file }, measured = true },
    "cat", params.input.file)
```

These rules apply to `cold` and `warm`:

- Each takes a list of input handles, bound file params, each one the call
  passes in its argv, `reads` or `stdin`. An output, a spool, or an input
  the call doesn't pass is refused, so what brokkr prepares is always a file
  the child is given.
- After admitting the call, brokkr opens each file as `read()` opens it, and
  everything after acts on that descriptor. Two listings of one file, one
  inode, in one mode are one account naming both paths; in both modes the
  call is refused. So is a call whose declared writes, by their own path,
  already name a prepared file when brokkr looks. brokkr doesn't search a
  directory the call writes, and a link a child makes later is the child's
  own. When a prepared input is the call's `stdin`, the file opened as its
  stdin must be the one prepared.
- brokkr prepares every file before it scans any, so preparing one never
  silently undoes another's observation. `cold` writes back what is dirty,
  then advises the whole file out (`posix_fadvise` `DONTNEED`). `warm` reads
  it through in bounded positional reads, refusing a file that ends early.
- brokkr then scans each file with `mincore` in bounded windows, counting
  its resident bytes up to its end, and checks its size and observability
  again. `cold` must find none resident and `warm` every byte. Anything
  else fails the iteration before the child launches, naming what was seen
  and what can cause it. A sparse file or a memory limit gets no allowance.
  brokkr checks for a stop between windows and again before the launch.
- The kernel reports residency truthfully only to a process that owns the
  file, is capable over it, or may write it, and reports every page
  resident to anyone else. brokkr asks the one condition it can establish on
  the descriptor: that its effective credentials may write the file. An
  owner of a read-only file is refused though the kernel would answer it;
  the fix is to make the file writable by brokkr's user. brokkr asks before
  and after the scan, and assumes the permissions stay as they are in
  between.
- The preparation runs after the call's admission and before its launch, so
  it is inside the block's wall and outside the child's: with `measured =
  true` it is outside the result.
- The child stores its account (see Stored runs): the procedure, the scan
  interval from the block's start, and per file its paths, mode, size and
  resident bytes. A failed preparation stores no child. The iteration's
  reason accounts for every file: scanned with its count, prepared and not
  scanned, failed with why, or not attempted.

What this establishes is narrow:

- The counts are what brokkr accumulated while it queried every page during
  the scan interval, not one snapshot of every file at once.
- The kernel reports a page under read as not resident.
- Nothing keeps the cache as it was found once the child runs, including a
  page another reader or memory pressure moves.
- The state of device, controller and metadata caches is neither reset nor
  established, though the write-back and reads may change it.

`page_cache(path, mode)`, inside `preflight()` alone, checks what brokkr can
detect beforehand of a bound input. `path` is the string `params` gives; any
other path is a script error. It opens the file as an iteration would, asks
the same observability condition, maps and queries it once, and for `"cold"`
refuses tmpfs and ramfs, whose pages are their storage. It returns nil when
the conditions hold and the reason otherwise, for the script to `fail()`
with. It prepares and reads nothing, and passing it isn't support: the call
establishes and checks everything again.

```lua
function preflight(params)
    local why = page_cache(params.input.file, "cold")
    if why then fail(why) end
end
```

## JSON

`json.encode(v)` returns compact JSON text, object keys sorted, and
`json.decode(s)` returns one JSON document as Lua values. The shape is
`record`'s: string keys an object, a list 1..n an array, nothing nested
deeper than 64, finite numbers, UTF-8 strings, and no handle or function.

- An array `json.decode` makes, and every list brokkr hands a script
  (`params`, `keys`, `config()`), has brokkr's array metatable, so an empty
  one encodes, and records, as `[]`. A plain empty table is `{}`.
- `json.null` stands for null inside a table, so `[1,null,3]` decodes to
  three elements, and `v == json.null` tests for it. Decoding always uses
  brokkr's own null, whatever the script assigns to `json.null`, and
  brokkr refuses a `json.null` with keys set in it.
- `json.decode` refuses a key given twice in one object. An integer within
  2^53 is a number, and one beyond it, up to 64 bits, is its decimal text,
  as the resolved view hands TOML integers. A number with a fraction or an
  exponent, or an integer past 64 bits, is a double. Whole numbers within
  2^53 encode as integers.
- `json.encode` refuses output past 16 MiB.

## Interrupts

What Ctrl-C does depends on where it arrives:

- **During a preflight**: it stops the preflight's child and ends the
  invocation as an interrupt, with nothing stored, since no arm was
  attempted.
- **During a child**: it stops the child and ends the iteration
  interrupted. brokkr stores the run as interrupted, and the command exits
  as an interrupt.
- **During Lua**, at the top level or in `bench()`: the VM sees it within a
  few milliseconds at its next safepoint, a loop's backward jump or a call,
  so a script spinning in a loop that calls no builtin stops too.

brokkr stores an iteration a stop reached as interrupted, whatever it would
otherwise have been. A second Ctrl-C while the first is pending takes the
default action and kills brokkr at once. That run isn't stored, and a later
`brokkr clean` or lease scan reclaims its iteration directory.

## Iteration directory

Each iteration owns one directory,
`bench-iterations/.brokkr-iteration-TOKEN`, under the host's resolved
`scratch`, by default `data/scratch` under the project root. It holds:

- `out/`
- `scratch/`
- the children's stdout and stderr spools
- the marked calls' hotpath reports

brokkr removes it when the iteration ends,
and removes one a crash left behind once nothing that inherited its
reservation is still running. Its outputs and scratch go to that configured
drive.

brokkr refuses a `scratch` inside the project's `.brokkr/` unless it is
`.brokkr` itself. A `scratch` inside a directory brokkr installs, prunes or
removes by fixed name, such as a tool install or cache under the data dir,
is a misplacement the protections don't cover.

## Stored runs

brokkr stores every attempted run, a failed build or an interrupted
iteration included: one per arm, or under `--rounds` one per cell and per
failed build. A run stores:

- its bound params: each key, its `from` path and a file's digest
- what it measured: timing, `--hotpath` or `--alloc`
- every child's argv, ending, `/proc` samples, marker FIFO records and
  capture losses
- a marked child's hotpath capture
- a `program` child's executable path and digest
- each child's stderr size and last 64 KiB as text, each byte that isn't
  UTF-8 replaced, marked when earlier bytes were dropped, and noted when
  the call was still held as it was read
- for a call that prepared inputs `cold` or `warm`, its page-cache account
  (see Page cache)

`brokkr sidecar UUID` reads one child's evidence through the sidecar views
(`brokkr man measure`, Script-bench children). Stderr tails share a run
budget of 16 MiB of raw bytes, and past it a child keeps its size and no
tail. brokkr never stores the whole stderr; a script that needs more reads
`stderr_file` while its iteration lives.

Runs pair in classes that never pair with each other: timing, `--hotpath`,
`--alloc`, and incidental. An incidental run is a timing run any of whose
children captured a report, since its build was instrumented by other
means.

A run of a dirty tree, dirty as its attempt starts or after its build (see
Build), is stored and marked dirty. `brokkr results` lists it with `-dirty`
and never compares it. Dirty follows the measurement policy, so these don't
make a tree dirty:

- a change to the root `brokkr.toml`
- a change to a config file it includes, one that isn't `Cargo.lock` or a
  toolchain file
- a change to a `.md` file

A `--commit` worktree counts as clean.

brokkr prints the run's uuid whatever `--verbose` says, since it is the one
handle `brokkr results UUID` finds the run by, with where the run went. A
run that can't be stored fails the command naming its uuid. One whose
storing may or may not have succeeded says so, naming the attempt whose
outcome is unknown, and `brokkr results UUID` then shows whether it did.

### Schedule

A `--rounds` run (see Rounds) also stores its schedule:

- the invocation's id
- the whole plan: the axis, every competitor's and workload's label, the
  rounds, and which build each competitor runs in each workload
- its own slot: workload, competitor, round and position in the round
- whether it records a cell, or a build that failed or was interrupted

Every run of an invocation has the same plan, so any one names every slot
planned. A cell records the build it ran as its build made it, once, before
the cells, including whether the live tree was dirty then; its conditions
are its own. The pairing key doesn't include the schedule.

### Destination

brokkr opens the store before anything is built: the local `bench.db`, or
with `results_server` set, it checks that the server answers with this
brokkr's protocol. A server that can't be reached fails the command before
any measurement.

Publishing retries what may succeed on a repeat:

- Locally, brokkr retries a few times at once.
- To a server, it retries with a growing pause. A retry starts only within
  two minutes of the first attempt, and there are eight attempts at most.
  Each attempt waits as long as the server's own deadline, five minutes
  plus a proxy's allowance, before giving up on it.
- A publication whose outcome is unknown always gets one more attempt past
  those limits.

A Ctrl-C that stopped the run still leaves it one publication attempt. A
Ctrl-C during the retries ends them once the attempt in flight returns. The
whole invocation holds the brokkr lock, every arm's or cell's publication
included.

### Host state

Each admitted iteration also stores what the host's writeback and its
scratch drive were doing. brokkr reads these before `bench()`, every 100 ms
through it, after it, and once more after the iteration directory is
released:

- the block device holding the iteration directory, a partition's whole
  disk
- `/proc/meminfo` `Dirty` and `Writeback`
- that device's cumulative write, write-wait, in-flight and discard
  counters

It also stores the cell before it on this host. Every iteration records
itself in a per-user cursor beside the brokkr lock as it starts, and again
once released. The next one, in any project, arm or invocation, then names
its predecessor whether or not that run was ever published, or says the
predecessor never recorded an end. The run also keeps the newest row of the
command history and when `fstrim.timer` last fired.

`brokkr results UUID` shows for each iteration:

- its device
- the gap from its predecessor: elapsed, and what the device wrote and
  discarded in it, when both name one device instance
- its starting `Dirty` and `Writeback` and their peaks
- the device's writes and mean write wait through it

`brokkr results --compare` names where the result iterations' start state
or gap differ, and `brokkr sidecar` gives each phase of a child its host
window. A manual `fstrim` shows as discards in the gap, never by name. A
rest between cells is a `binary_with({program = "sleep"}, "60")` call, which
its iteration then records, or a bench of its own that the next cell names
as its predecessor.

### Stored config

The stored config is one TOML document with these tables:

- **`[run]`**: the live project root and build root, under `--commit` too.
- **`[script]`**: the bench, the file its script came from, and the exact
  source as `source`, or `source_hex` when it isn't UTF-8.
- **`[modules.NAME]`**: each `bench_lib` module the run asked for, its file
  as `origin` and its exact source the same way; absent when there was none.
- **`[config]`**: brokkr.toml as composed, includes folded and the
  user-wide layer beneath, every table's keys sorted.

`brokkr results --compare` names what moved in it between two paired runs.
brokkr doesn't store the resolved view a script saw, since it derives from
this document and the run's hostname.
